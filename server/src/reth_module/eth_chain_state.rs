use alloy_consensus::proofs::{calculate_receipt_root, calculate_transaction_root};
use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_consensus::Header;
use alloy_consensus::TxReceipt;
use alloy_eips::eip2718::Decodable2718;
use alloy_genesis::{ChainConfig, Genesis};
use alloy_primitives::{keccak256, Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use alloy_trie::{
    proof::ProofRetainer, HashBuilder, Nibbles, TrieAccount, EMPTY_ROOT_HASH, KECCAK_EMPTY,
};
use anyhow::{Context, Result};
use client_sdk::rest_client::NodeApiClient;
use indexmap::IndexMap;
use reth_chainspec::{ChainSpec, EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Block, BlockBody, Receipt, TransactionSigned};
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::logs_bloom;
use reth_primitives_traits::{RecoveredBlock, SealedHeader, SignerRecoverable};
use reth_revm::witness::ExecutionWitnessRecord;
use reth_stateless::{ExecutionWitness, StatelessInput};
use revm::database::{BundleState, CacheDB, EmptyDB};
use revm::state::{AccountInfo, Bytecode};
use sdk::{
    BlobIndex, BlobTransaction, ContractName, ProgramId, ProofData, ProofTransaction, TxContext,
    TxId, Verifier,
};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tracing::{debug, info, warn};

// ── EthChainState ─────────────────────────────────────────────────────────────

/// Minimal receipt info stored after a transaction settles on Hyli.
#[derive(Debug, Clone)]
pub struct EthTxReceipt {
    pub block_number: u64,
    pub block_hash: B256,
    pub success: bool,
}

/// In-memory per-account EVM state (flat; trie is rebuilt as needed).
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    pub balance: U256,
    pub nonce: u64,
    /// Raw bytecode (empty for EOAs)
    pub code: Bytes,
    /// `keccak256(code)`, or `KECCAK_EMPTY` for EOAs.
    pub code_hash: B256,
    /// Storage slots: slot (U256) → value (U256). Only non-zero values are stored.
    pub storage: HashMap<U256, U256>,
}

impl AccountState {}

/// Current tracked state of the Hyli-hosted Ethereum chain (the reth contract).
#[derive(Debug, Clone)]
pub struct EthChainState {
    /// Current EVM state root (equals last settled block's `state_root`).
    pub state_root: B256,
    /// Number of settled transactions (used as the EVM block number).
    pub block_number: u64,
    /// Flat per-address account state.
    pub accounts: HashMap<Address, AccountState>,
    /// Reth chain spec (hardfork configuration).
    pub chain_spec: Arc<ChainSpec>,
    /// Chain config forwarded in `StatelessInput.chain_config`.
    pub chain_config: ChainConfig,
    /// Full genesis JSON bytes, forwarded verbatim as the `evm_config` segment of the
    /// reth proof payload. The node's reth verifier needs the complete genesis (alloc,
    /// gasLimit, etc.) to reconstruct the ChainSpec — sending only `ChainConfig` is
    /// insufficient and results in "evm config did not include a genesis specification".
    pub genesis_json: Vec<u8>,
    /// Recent ancestor headers for BLOCKHASH opcode support (up to 256).
    /// The *last* element is the most recent (parent) header.
    pub header_history: VecDeque<SealedHeader>,
    /// Receipts indexed by EVM tx hash (keccak256 of raw EIP-2718 bytes).
    /// Populated when a transaction settles on Hyli.
    pub settled_receipts: HashMap<[u8; 32], EthTxReceipt>,
}

impl EthChainState {
    /// Build `EthChainState` from a genesis JSON blob.
    ///
    /// The initial state root is computed from the genesis alloc trie.
    pub fn new(genesis_json: &[u8]) -> Result<Self> {
        let genesis: Genesis =
            serde_json::from_slice(genesis_json).context("Failed to parse genesis JSON")?;

        let chain_config = genesis.config.clone();
        let chain_spec = Arc::new(ChainSpec::from(genesis.clone()));

        // Populate account state from genesis alloc.
        let mut accounts: HashMap<Address, AccountState> = HashMap::new();
        for (addr, alloc) in &genesis.alloc {
            let code: Bytes = alloc.code.clone().unwrap_or_default();
            let code_hash = if code.is_empty() {
                KECCAK_EMPTY
            } else {
                keccak256(&code)
            };
            let mut storage: HashMap<U256, U256> = HashMap::new();
            if let Some(alloc_storage) = &alloc.storage {
                for (slot, value) in alloc_storage {
                    storage.insert(U256::from_be_bytes(slot.0), U256::from_be_bytes(value.0));
                }
            }
            accounts.insert(
                *addr,
                AccountState {
                    balance: alloc.balance,
                    nonce: alloc.nonce.unwrap_or(0),
                    code,
                    code_hash,
                    storage,
                },
            );
        }

        // Derive the state root from the genesis alloc trie.
        let (state_root, _) = build_account_trie_with_proofs(&accounts, &[]);

        // Build the genesis (block 0) sealed header.
        let genesis_header = build_genesis_header(&genesis, state_root);
        let genesis_sealed = SealedHeader::seal_slow(genesis_header);

        let mut header_history = VecDeque::new();
        header_history.push_back(genesis_sealed);

        Ok(Self {
            state_root,
            block_number: 0,
            accounts,
            chain_spec,
            chain_config,
            genesis_json: genesis_json.to_vec(),
            header_history,
            settled_receipts: HashMap::new(),
        })
    }

    /// Apply a settled transaction to the in-memory state.
    ///
    /// Re-executes the transaction and merges the state diff into `self.accounts`.
    pub fn apply_transaction(
        &mut self,
        raw_eip2718: &[u8],
        new_state_root: [u8; 32],
    ) -> Result<()> {
        let new_root = B256::from(new_state_root);

        let tx = TransactionSigned::decode_2718(&mut &*raw_eip2718)
            .context("Failed to decode EIP-2718 transaction")?;
        let signer = tx
            .recover_signer()
            .map_err(|e| anyhow::anyhow!("Failed to recover signer: {e:?}"))?;

        let parent = self
            .header_history
            .back()
            .ok_or_else(|| anyhow::anyhow!("No parent header"))?
            .clone();

        let evm_config = EthEvmConfig::new(self.chain_spec.clone());
        let exec_block =
            build_exec_block(tx, signer, &parent, self.block_number + 1, &self.chain_spec)?;

        let mut db = self.build_cache_db();
        let output = evm_config
            .executor(&mut db)
            .execute(&exec_block)
            .context("EVM execution failed in apply_transaction")?;

        let final_block = finalize_block(exec_block, &output.receipts, output.gas_used, new_root);

        self.merge_bundle(&output.state);

        self.state_root = new_root;
        self.block_number += 1;

        let new_header = SealedHeader::seal_slow(final_block.header().clone());
        self.header_history.push_back(new_header);
        if self.header_history.len() > 256 {
            self.header_history.pop_front();
        }

        Ok(())
    }

    /// Apply a transaction speculatively, computing the post-state root locally.
    ///
    /// Unlike [`apply_transaction`], no canonical root is needed — the root is derived
    /// from the EVM state diff.  Call this at sequencing time so subsequent proofs are
    /// built against the correct pre-state.  On settlement success the canonical root
    /// from Hyli is used to confirm (or correct) `state_root`.
    pub fn apply_transaction_speculative(&mut self, raw_eip2718: &[u8]) -> Result<()> {
        let tx = TransactionSigned::decode_2718(&mut &*raw_eip2718)
            .context("Failed to decode EIP-2718 transaction")?;
        let signer = tx
            .recover_signer()
            .map_err(|e| anyhow::anyhow!("Failed to recover signer: {e:?}"))?;

        let parent = self
            .header_history
            .back()
            .ok_or_else(|| anyhow::anyhow!("No parent header"))?
            .clone();

        let evm_config = EthEvmConfig::new(self.chain_spec.clone());
        let exec_block =
            build_exec_block(tx, signer, &parent, self.block_number + 1, &self.chain_spec)?;

        let mut db = self.build_cache_db();
        let output = evm_config
            .executor(&mut db)
            .execute(&exec_block)
            .context("EVM execution failed in apply_transaction_speculative")?;

        let post_root = compute_post_state_root(&self.accounts, &output.state);
        let final_block = finalize_block(exec_block, &output.receipts, output.gas_used, post_root);

        self.merge_bundle(&output.state);
        self.state_root = post_root;
        self.block_number += 1;

        let new_header = SealedHeader::seal_slow(final_block.header().clone());
        self.header_history.push_back(new_header);
        if self.header_history.len() > 256 {
            self.header_history.pop_front();
        }

        Ok(())
    }

    /// Build the reth proof payload for a pending transaction.
    ///
    /// Payload format:
    /// ```text
    /// [4-byte LE u32: calldata_len]  [borsh(Calldata)]
    /// [4-byte LE u32: stateless_len] [bincode(StatelessInput)]
    /// [4-byte LE u32: evm_config_len][JSON chain-spec bytes]
    /// ```
    pub fn build_proof_payload(&self, pending: &PendingRethProof) -> Result<Vec<u8>> {
        use borsh::to_vec as borsh_to_vec;
        use sdk::Calldata;

        let tx_hash = pending.tx_id.1.clone();
        let calldata = Calldata {
            identity: pending.hyli_tx.identity.clone(),
            tx_hash,
            private_input: vec![],
            blobs: pending.hyli_tx.blobs.clone().into(),
            index: pending.blob_index,
            tx_ctx: Some((*pending.tx_ctx).clone()),
            tx_blob_count: pending.hyli_tx.blobs.len(),
        };

        let calldata_bytes = borsh_to_vec(&calldata)
            .map_err(|e| anyhow::anyhow!("Failed to borsh-encode calldata: {e}"))?;

        let stateless_bytes = self.build_stateless_input(&pending.raw_eip2718)?;

        // Send the full genesis JSON, not just chain_config. The node's reth verifier
        // needs the complete genesis spec (alloc, gasLimit, etc.) to build a ChainSpec.
        let evm_config_json = self.genesis_json.clone();

        let mut payload = Vec::new();
        write_segment(&mut payload, &calldata_bytes);
        write_segment(&mut payload, &stateless_bytes);
        write_segment(&mut payload, &evm_config_json);

        Ok(payload)
    }

    /// Build the bincode-encoded `StatelessInput` for a single EIP-2718 transaction.
    pub fn build_stateless_input(&self, raw_eip2718: &[u8]) -> Result<Vec<u8>> {
        let tx = TransactionSigned::decode_2718(&mut &*raw_eip2718)
            .context("Failed to decode EIP-2718 transaction")?;
        let signer = tx
            .recover_signer()
            .map_err(|e| anyhow::anyhow!("Failed to recover signer: {e:?}"))?;

        let parent = self
            .header_history
            .back()
            .ok_or_else(|| anyhow::anyhow!("No parent header in history"))?
            .clone();

        // ── Step 1: Execute to collect witness record + state diff ────────────

        let evm_config = EthEvmConfig::new(self.chain_spec.clone());
        let exec_block =
            build_exec_block(tx, signer, &parent, self.block_number + 1, &self.chain_spec)?;

        let mut db = self.build_cache_db();
        let mut witness_record = ExecutionWitnessRecord::default();
        let output = evm_config
            .executor(&mut db)
            .execute_with_state_closure(&exec_block, |state_db| {
                witness_record.record_executed_state(state_db);
            })
            .context("EVM execution failed in build_stateless_input")?;

        // ── Step 2: Compute pre-state proof nodes ─────────────────────────────

        let hashed_state = &witness_record.hashed_state;
        let account_proof_targets: Vec<B256> = hashed_state.accounts.keys().cloned().collect();

        let (pre_state_root, mut witness_nodes) =
            build_account_trie_with_proofs(&self.accounts, &account_proof_targets);

        if pre_state_root != self.state_root {
            warn!(
                computed = %pre_state_root,
                tracked = %self.state_root,
                "Pre-state trie root mismatch vs tracked state root"
            );
        }

        // Add storage proof nodes for accessed storage.
        for (hashed_addr, hashed_storage) in &hashed_state.storages {
            let addr = find_address_by_hash(&self.accounts, hashed_addr);
            let empty_storage: HashMap<U256, U256> = HashMap::new();
            let storage = addr
                .and_then(|a| self.accounts.get(&a))
                .map(|s| &s.storage)
                .unwrap_or(&empty_storage);

            let storage_targets: Vec<B256> = hashed_storage.storage.keys().cloned().collect();
            let (_, storage_nodes) = build_storage_trie_with_proofs(storage, &storage_targets);
            witness_nodes.extend(storage_nodes);
        }

        // ── Step 3: Compute post-state root ──────────────────────────────────

        let post_state_root = compute_post_state_root(&self.accounts, &output.state);

        // ── Step 4: Build final block with all correct header fields ─────────

        let final_block = finalize_block(
            exec_block,
            &output.receipts,
            output.gas_used,
            post_state_root,
        );

        // ── Step 5: Assemble ExecutionWitness ─────────────────────────────────

        let headers: Vec<Bytes> = self
            .header_history
            .iter()
            .map(|h| {
                let mut buf = Vec::new();
                h.header().encode(&mut buf);
                Bytes::from(buf)
            })
            .collect();

        // Supplement witness codes with ALL known contract bytecodes.
        // `record_executed_state` only captures contracts created in this transaction
        // (via bundle_state). Pre-existing contracts called via CALL/DELEGATECALL are
        // loaded from the CacheDB but don't appear in bundle_state.contracts, so the
        // stateless verifier can't find their bytecode. Include everything we know.
        let mut codes = witness_record.codes;
        for account in self.accounts.values() {
            if !account.code.is_empty() {
                codes.push(account.code.clone());
            }
        }

        let witness = ExecutionWitness {
            state: witness_nodes,
            codes,
            keys: witness_record.keys,
            headers,
        };

        // ── Step 6: Assemble StatelessInput and serialize ─────────────────────

        let stateless_input = StatelessInput {
            block: final_block.into_block(),
            witness,
            chain_config: self.chain_config.clone(),
        };

        bincode::serialize(&stateless_input)
            .context("bincode serialization of StatelessInput failed")
    }

    // ── EVM state accessors ───────────────────────────────────────────────────

    pub fn chain_id(&self) -> u64 {
        self.chain_config.chain_id
    }

    /// Current base fee (wei), taken from the most recent block header.
    pub fn gas_price(&self) -> u64 {
        self.header_history
            .back()
            .and_then(|h| h.base_fee_per_gas())
            .unwrap_or(1)
    }

    pub fn account_balance(&self, addr: &Address) -> U256 {
        self.accounts
            .get(addr)
            .map(|a| a.balance)
            .unwrap_or_default()
    }

    pub fn account_nonce(&self, addr: &Address) -> u64 {
        self.accounts.get(addr).map(|a| a.nonce).unwrap_or(0)
    }

    /// Returns a clone of the sealed header for EVM block `number`, if within history.
    pub fn get_header_by_number(&self, number: u64) -> Option<SealedHeader> {
        self.header_history
            .iter()
            .find(|h| h.number() == number)
            .cloned()
    }

    /// Returns a clone of the most recent sealed header.
    pub fn latest_header(&self) -> Option<SealedHeader> {
        self.header_history.back().cloned()
    }

    /// Serialize the current account state as a genesis-compatible `alloc` JSON object.
    ///
    /// The returned string can be pasted directly into the `"alloc"` field of the
    /// genesis JSON in `conf_defaults.toml` to pre-deploy all currently-known contracts.
    pub fn dump_genesis_alloc(&self) -> serde_json::Value {
        use serde_json::{json, Map, Value};

        let mut alloc: Map<String, Value> = Map::new();

        for (addr, account) in &self.accounts {
            let mut entry: Map<String, Value> = Map::new();

            entry.insert(
                "balance".into(),
                json!(format!("0x{:x}", account.balance)),
            );

            if account.nonce != 0 {
                entry.insert("nonce".into(), json!(format!("0x{:x}", account.nonce)));
            }

            if !account.code.is_empty() {
                entry.insert(
                    "code".into(),
                    json!(format!("0x{}", hex::encode(&account.code))),
                );
            }

            if !account.storage.is_empty() {
                let mut storage: Map<String, Value> = Map::new();
                for (slot, value) in &account.storage {
                    let slot_bytes: [u8; 32] = slot.to_be_bytes();
                    let value_bytes: [u8; 32] = value.to_be_bytes();
                    storage.insert(
                        format!("0x{}", hex::encode(slot_bytes)),
                        json!(format!("0x{}", hex::encode(value_bytes))),
                    );
                }
                entry.insert("storage".into(), Value::Object(storage));
            }

            alloc.insert(format!("{addr:?}"), Value::Object(entry));
        }

        Value::Object(alloc)
    }

    /// Estimate the gas needed to execute a call/deployment against the current state.
    ///
    /// Runs the transaction through revm with a gas limit equal to the block gas limit,
    /// then returns the actual gas used plus a 30 % buffer, capped at 90 % of the block
    /// gas limit so that Hyperlane's own 1.1× multiplier never exceeds the block limit.
    pub fn estimate_gas(
        &self,
        from: Address,
        to: Option<Address>,
        data: Bytes,
        value: U256,
    ) -> u64 {
        use revm::context::{BlockEnv, TxEnv};
        use revm::context_interface::block::BlobExcessGasAndPrice;
        use revm::primitives::TxKind;
        use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

        let parent = self.header_history.back();
        let block_gas_limit = parent
            .map(|h| h.gas_limit())
            .filter(|&g| g > 0)
            .unwrap_or(30_000_000);
        let basefee = self.gas_price();
        let block_number = self.block_number + 1;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: Address::ZERO,
            timestamp: U256::from(timestamp),
            gas_limit: block_gas_limit,
            basefee,
            difficulty: U256::ZERO,
            prevrandao: Some(B256::ZERO),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
        };

        // If `from` is a contract address, revm rejects it with RejectCallerWithCode.
        // Use a zero address instead so the simulation can still estimate gas.
        let effective_from = if self
            .accounts
            .get(&from)
            .map(|a| !a.code.is_empty())
            .unwrap_or(false)
        {
            Address::ZERO
        } else {
            from
        };

        let tx_env = TxEnv {
            tx_type: 2,
            caller: effective_from,
            gas_limit: block_gas_limit,
            gas_price: basefee as u128,
            kind: to.map(TxKind::Call).unwrap_or(TxKind::Create),
            value,
            data,
            nonce: self.account_nonce(&effective_from),
            chain_id: Some(self.chain_id()),
            ..Default::default()
        };

        let mut db = self.build_cache_db();
        let ctx = Context::mainnet()
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = self.chain_id();
                cfg.tx_chain_id_check = false;
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            })
            .with_db(&mut db)
            .with_block(block_env);
        let mut evm = ctx.build_mainnet();

        match evm.transact(tx_env) {
            Ok(outcome) => {
                let gas_used = outcome.result.gas_used();
                // +30 % buffer, capped at 90 % of block gas limit so Hyperlane's 1.1× stays under block limit.
                let with_buffer =
                    (gas_used as u128 * 13 / 10).min(block_gas_limit as u128 * 9 / 10) as u64;
                with_buffer.max(21_000)
            }
            Err(e) => {
                warn!(
                    "estimate_gas execution failed: {e:?}, falling back to 90% of block gas limit"
                );
                block_gas_limit * 9 / 10
            }
        }
    }

    /// Execute a call against the current EVM state and return the output bytes.
    ///
    /// Returns `(success, output)` where `output` is the return data (or revert data).
    pub fn execute_call(
        &self,
        from: Address,
        to: Option<Address>,
        data: Bytes,
        value: U256,
    ) -> (bool, Bytes) {
        use revm::context::{BlockEnv, TxEnv};
        use revm::context_interface::block::BlobExcessGasAndPrice;
        use revm::primitives::TxKind;
        use revm::{Context, ExecuteEvm, MainBuilder, MainContext};

        let parent = self.header_history.back();
        let block_gas_limit = parent.map(|h| h.gas_limit()).unwrap_or(30_000_000);
        let block_number = self.block_number + 1;
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // eth_call uses zero gas price so balance checks are skipped (same as geth/reth).
        let block_env = BlockEnv {
            number: U256::from(block_number),
            beneficiary: Address::ZERO,
            timestamp: U256::from(timestamp),
            gas_limit: block_gas_limit,
            basefee: 0,
            difficulty: U256::ZERO,
            prevrandao: Some(B256::ZERO),
            blob_excess_gas_and_price: Some(BlobExcessGasAndPrice {
                excess_blob_gas: 0,
                blob_gasprice: 1,
            }),
        };

        // Use EIP-1559 with zero fees so no balance is required (same as geth eth_call).
        let tx_env = TxEnv {
            tx_type: 2,
            caller: from,
            gas_limit: block_gas_limit,
            gas_price: 0,
            kind: to.map(TxKind::Call).unwrap_or(TxKind::Create),
            value,
            data,
            nonce: self.account_nonce(&from),
            chain_id: Some(self.chain_id()),
            ..Default::default()
        };

        let mut db = self.build_cache_db();
        let ctx = Context::mainnet()
            .modify_cfg_chained(|cfg| {
                cfg.chain_id = self.chain_id();
                cfg.tx_chain_id_check = false;
                // Disable EIP-7825 tx gas limit cap for call simulation.
                cfg.tx_gas_limit_cap = Some(u64::MAX);
            })
            .with_db(&mut db)
            .with_block(block_env);
        let mut evm = ctx.build_mainnet();

        match evm.transact(tx_env) {
            Ok(outcome) => {
                let success = outcome.result.is_success();
                let output = outcome.result.into_output().unwrap_or_default();
                info!(
                    "execute_call result: success={} output=0x{}",
                    success,
                    hex::encode(&output)
                );
                (success, output)
            }
            Err(e) => {
                warn!("execute_call EVM error (not a revert): {e:?}");
                (false, Bytes::default())
            }
        }
    }

    /// Record a settled receipt keyed by `keccak256(raw_eip2718)`.
    ///
    /// Call this after updating `block_number` and `header_history` so that the
    /// stored block number and block hash reflect the block that included this tx.
    pub fn record_settled_receipt(&mut self, raw_eip2718: &[u8], success: bool) {
        let evm_hash: [u8; 32] = *keccak256(raw_eip2718);
        let block_number = self.block_number;
        let block_hash = self
            .header_history
            .back()
            .map(|h| h.hash())
            .unwrap_or_default();
        self.settled_receipts.insert(
            evm_hash,
            EthTxReceipt {
                block_number,
                block_hash,
                success,
            },
        );
    }

    /// Push a minimal synthetic sealed header after a fallback (non-EVM) state update.
    ///
    /// This keeps `header_history` in sync with `block_number` so that the next proof
    /// correctly chains from the most-recent block rather than from a stale ancestor.
    /// Without this, `build_stateless_input` would claim the wrong block number in the
    /// proof payload, triggering "invalid ancestor chain" at stateless validation.
    pub fn push_fallback_header(&mut self) {
        let parent = self.header_history.back();
        let parent_hash = parent.map(|h| h.hash()).unwrap_or_default();
        let parent_gas_limit = parent.map(|h| h.gas_limit()).unwrap_or(30_000_000);
        let parent_timestamp = parent.map(|h| h.timestamp()).unwrap_or(0);
        let parent_base_fee = parent.and_then(|h| h.base_fee_per_gas());

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .max(parent_timestamp + 1);

        let header = Header {
            parent_hash,
            number: self.block_number,
            state_root: self.state_root,
            gas_limit: parent_gas_limit,
            timestamp,
            base_fee_per_gas: parent_base_fee,
            ommers_hash: alloy_primitives::b256!(
                "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
            ),
            ..Default::default()
        };

        let sealed = SealedHeader::seal_slow(header);
        self.header_history.push_back(sealed);
        if self.header_history.len() > 256 {
            self.header_history.pop_front();
        }
    }

    fn build_cache_db(&self) -> CacheDB<EmptyDB> {
        let mut db = CacheDB::new(EmptyDB::default());

        for (addr, state) in &self.accounts {
            let bytecode = if state.code.is_empty() {
                Bytecode::default()
            } else {
                Bytecode::new_raw(state.code.clone())
            };

            let info = AccountInfo {
                balance: state.balance,
                nonce: state.nonce,
                code_hash: state.code_hash,
                code: Some(bytecode),
                account_id: None,
            };

            db.insert_account_info(*addr, info);

            for (slot, value) in &state.storage {
                db.insert_account_storage(*addr, *slot, *value)
                    .expect("insert_account_storage should not fail with EmptyDB");
            }
        }

        db
    }

    fn merge_bundle(&mut self, bundle: &BundleState) {
        for (addr, bundle_account) in bundle.state() {
            match &bundle_account.info {
                None => {
                    self.accounts.remove(addr);
                }
                Some(info) => {
                    let entry = self.accounts.entry(*addr).or_default();
                    entry.nonce = info.nonce;
                    entry.balance = info.balance;
                    entry.code_hash = info.code_hash;
                    if let Some(code) = &info.code {
                        let raw: Bytes = code.original_bytes();
                        entry.code = raw;
                    }
                    for (slot, storage_slot) in &bundle_account.storage {
                        let slot_u256 = U256::from(*slot);
                        let present: U256 = storage_slot.present_value();
                        if present.is_zero() {
                            entry.storage.remove(&slot_u256);
                        } else {
                            entry.storage.insert(slot_u256, present);
                        }
                    }
                }
            }
        }
    }
}

// ── PendingRethProof ──────────────────────────────────────────────────────────

/// A transaction sequenced on the reth contract, awaiting a reth proof.
#[derive(Debug, Clone)]
pub struct PendingRethProof {
    pub tx_id: TxId,
    pub hyli_tx: BlobTransaction,
    pub tx_ctx: Arc<TxContext>,
    /// Index of the reth blob within the transaction blobs.
    pub blob_index: BlobIndex,
    /// Raw EIP-2718 encoded transaction bytes (the actual EVM transaction).
    pub raw_eip2718: Vec<u8>,
}

/// Pending proofs indexed by `TxId`.
pub type PendingProofsMap = IndexMap<TxId, PendingRethProof>;

// ── Public helpers ────────────────────────────────────────────────────────────

/// Extract raw EIP-2718 bytes from a reth blob, unwrapping any `StructuredBlobData` wrapper.
fn extract_raw_eip2718(data: &[u8]) -> Vec<u8> {
    if let Ok(structured) =
        sdk::StructuredBlobData::<Vec<u8>>::try_from(sdk::BlobData(data.to_vec()))
    {
        structured.parameters
    } else {
        data.to_vec()
    }
}

/// Extract the reth blob from a transaction and build a `PendingRethProof`.
pub fn extract_pending_proof(
    tx_id: TxId,
    hyli_tx: BlobTransaction,
    tx_ctx: Arc<TxContext>,
    contract_name: &ContractName,
) -> Option<PendingRethProof> {
    let (blob_index, raw_eip2718) = hyli_tx
        .blobs
        .iter()
        .enumerate()
        .find(|(_, b)| b.contract_name == *contract_name)
        .map(|(i, b)| (BlobIndex(i), extract_raw_eip2718(&b.data.0)))?;

    Some(PendingRethProof {
        tx_id,
        hyli_tx,
        tx_ctx,
        blob_index,
        raw_eip2718,
    })
}

/// Submit a reth `ProofTransaction` to the Hyli node.
pub async fn submit_reth_proof(
    node: &client_sdk::rest_client::NodeApiHttpClient,
    contract_name: &ContractName,
    program_id: &ProgramId,
    proof_bytes: Vec<u8>,
) -> Result<sdk::TxHash> {
    let proof_tx = ProofTransaction {
        contract_name: contract_name.clone(),
        program_id: program_id.clone(),
        verifier: Verifier(hyli_model::verifiers::RETH.to_string()),
        proof: ProofData(proof_bytes),
    };

    debug!(
        contract_name =% contract_name,
        "Submitting reth ProofTransaction"
    );

    let tx_hash = node.send_tx_proof(proof_tx).await?;
    Ok(tx_hash)
}

// ── Genesis state root ────────────────────────────────────────────────────────

/// Compute the state root for a genesis JSON blob without constructing the full
/// `EthChainState`. Used during contract registration on Hyli.
pub fn genesis_state_root(genesis_json: &[u8]) -> Result<[u8; 32]> {
    let genesis: Genesis =
        serde_json::from_slice(genesis_json).context("Failed to parse genesis JSON")?;

    let mut accounts: HashMap<Address, AccountState> = HashMap::new();
    for (addr, alloc) in &genesis.alloc {
        let code: Bytes = alloc.code.clone().unwrap_or_default();
        let code_hash = if code.is_empty() {
            KECCAK_EMPTY
        } else {
            keccak256(&code)
        };
        let mut storage: HashMap<U256, U256> = HashMap::new();
        if let Some(alloc_storage) = &alloc.storage {
            for (slot, value) in alloc_storage {
                storage.insert(U256::from_be_bytes(slot.0), U256::from_be_bytes(value.0));
            }
        }
        accounts.insert(
            *addr,
            AccountState {
                balance: alloc.balance,
                nonce: alloc.nonce.unwrap_or(0),
                code,
                code_hash,
                storage,
            },
        );
    }

    let (root, _) = build_account_trie_with_proofs(&accounts, &[]);
    Ok(root.into())
}

// ── Trie helpers ──────────────────────────────────────────────────────────────

/// Build the account state trie from `accounts`, retaining proof nodes for
/// the given `proof_targets` (hashed addresses as `B256`).
///
/// Returns `(state_root, witness_nodes)`.
fn build_account_trie_with_proofs(
    accounts: &HashMap<Address, AccountState>,
    proof_targets: &[B256],
) -> (B256, Vec<Bytes>) {
    let target_nibbles: Vec<Nibbles> = proof_targets.iter().map(|h| Nibbles::unpack(h)).collect();
    let retainer = ProofRetainer::new(target_nibbles);
    let mut hash_builder = HashBuilder::default().with_proof_retainer(retainer);

    // Sort accounts by hashed address for deterministic trie ordering.
    let mut sorted: Vec<(B256, &AccountState)> = accounts
        .iter()
        .map(|(addr, state)| (keccak256(addr), state))
        .collect();
    sorted.sort_by_key(|(h, _)| *h);

    for (hashed_addr, state) in &sorted {
        let (storage_root, _) = build_storage_trie_with_proofs(&state.storage, &[]);

        let trie_account = TrieAccount {
            nonce: state.nonce,
            balance: state.balance,
            storage_root,
            code_hash: state.code_hash,
        };

        let rlp_encoded = alloy_rlp::encode(trie_account);
        hash_builder.add_leaf(Nibbles::unpack(hashed_addr), &rlp_encoded);
    }

    let state_root = hash_builder.root();
    let proof_nodes = hash_builder.take_proof_nodes();
    let witness_nodes: Vec<Bytes> = proof_nodes.into_inner().into_values().collect();

    (state_root, witness_nodes)
}

/// Build a storage trie retaining proof nodes for `proof_targets` (hashed slot keys).
fn build_storage_trie_with_proofs(
    storage: &HashMap<U256, U256>,
    proof_targets: &[B256],
) -> (B256, Vec<Bytes>) {
    // Filter to non-zero slots.
    let non_zero: Vec<(B256, U256)> = storage
        .iter()
        .filter(|(_, v)| !v.is_zero())
        .map(|(slot, value)| (keccak256(B256::from(*slot)), *value))
        .collect();

    if non_zero.is_empty() {
        return (EMPTY_ROOT_HASH, Vec::new());
    }

    let target_nibbles: Vec<Nibbles> = proof_targets.iter().map(|h| Nibbles::unpack(h)).collect();
    let retainer = ProofRetainer::new(target_nibbles);
    let mut hash_builder = HashBuilder::default().with_proof_retainer(retainer);

    let mut sorted = non_zero;
    sorted.sort_by_key(|(h, _)| *h);

    for (hashed_slot, value) in &sorted {
        // RLP-encode the U256 value as a byte string.
        let rlp_value = alloy_rlp::encode(value);
        hash_builder.add_leaf(Nibbles::unpack(hashed_slot), &rlp_value);
    }

    let storage_root = hash_builder.root();
    let proof_nodes = hash_builder.take_proof_nodes();
    let witness_nodes: Vec<Bytes> = proof_nodes.into_inner().into_values().collect();

    (storage_root, witness_nodes)
}

/// Compute the post-state root by applying the bundle state diff onto a copy of `accounts`.
///
/// Uses the raw `BundleState` (with actual `Address` keys) rather than `HashedPostState` so
/// that newly created accounts (e.g. contract deployments) are handled correctly — their
/// addresses are available directly without needing to reverse a keccak hash.
fn compute_post_state_root(
    accounts: &HashMap<Address, AccountState>,
    bundle: &BundleState,
) -> B256 {
    let mut updated = accounts.clone();

    for (addr, bundle_account) in bundle.state() {
        match &bundle_account.info {
            None => {
                updated.remove(addr);
            }
            Some(info) => {
                let entry = updated.entry(*addr).or_default();
                entry.nonce = info.nonce;
                entry.balance = info.balance;
                entry.code_hash = info.code_hash;
                if let Some(code) = &info.code {
                    entry.code = code.original_bytes();
                }
                for (slot, storage_slot) in &bundle_account.storage {
                    let slot_u256 = U256::from(*slot);
                    let present = storage_slot.present_value();
                    if present.is_zero() {
                        entry.storage.remove(&slot_u256);
                    } else {
                        entry.storage.insert(slot_u256, present);
                    }
                }
            }
        }
    }

    let (post_root, _) = build_account_trie_with_proofs(&updated, &[]);
    post_root
}

/// Find the `Address` whose `keccak256(addr) == hashed_addr`.
fn find_address_by_hash(
    accounts: &HashMap<Address, AccountState>,
    hashed_addr: &B256,
) -> Option<Address> {
    accounts
        .keys()
        .find(|addr| keccak256(*addr) == *hashed_addr)
        .copied()
}

// ── Block construction ────────────────────────────────────────────────────────

/// Build an execution-ready `RecoveredBlock<Block>` containing `tx` as the sole transaction.
///
/// All pre-execution header fields are set correctly (`base_fee_per_gas`, `transactions_root`,
/// hardfork-specific fields, etc.). Output-derived fields (`state_root`, `gas_used`,
/// `receipts_root`, `logs_bloom`) are left as zero-value placeholders; call
/// [`finalize_block`] after execution to fill them in.
fn build_exec_block(
    tx: TransactionSigned,
    signer: Address,
    parent: &SealedHeader,
    block_number: u64,
    chain_spec: &ChainSpec,
) -> Result<RecoveredBlock<Block>> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .max(parent.timestamp() + 1);

    let base_fee_per_gas = if chain_spec.is_london_active_at_block(block_number) {
        chain_spec.next_block_base_fee(parent.header(), timestamp)
    } else {
        None
    };

    let withdrawals_root = if chain_spec.is_shanghai_active_at_timestamp(timestamp) {
        Some(EMPTY_ROOT_HASH)
    } else {
        None
    };

    let (blob_gas_used, excess_blob_gas, parent_beacon_block_root) =
        if chain_spec.is_cancun_active_at_timestamp(timestamp) {
            (
                Some(0u64),
                Some(parent.header().excess_blob_gas.unwrap_or(0)),
                Some(B256::ZERO),
            )
        } else {
            (None, None, None)
        };

    // Prague: sha256("") = EMPTY_REQUESTS_HASH
    let requests_hash = if chain_spec.is_prague_active_at_timestamp(timestamp) {
        Some(alloy_primitives::b256!(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ))
    } else {
        None
    };

    let transactions_root = calculate_transaction_root(std::slice::from_ref(&tx));

    let header = Header {
        parent_hash: parent.hash(),
        ommers_hash: alloy_primitives::b256!(
            "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
        ),
        beneficiary: Address::ZERO,
        // Placeholders — filled in by `finalize_block` after execution.
        state_root: B256::ZERO,
        receipts_root: B256::ZERO,
        logs_bloom: Default::default(),
        gas_used: 0,
        transactions_root,
        withdrawals_root,
        difficulty: U256::ZERO,
        number: block_number,
        gas_limit: parent.gas_limit(),
        timestamp,
        mix_hash: B256::ZERO,
        nonce: alloy_primitives::B64::ZERO,
        base_fee_per_gas,
        blob_gas_used,
        excess_blob_gas,
        parent_beacon_block_root,
        extra_data: Default::default(),
        requests_hash,
    };

    let body = BlockBody {
        transactions: vec![tx],
        ommers: vec![],
        withdrawals: withdrawals_root.map(|_| Default::default()),
    };

    let block = Block { header, body };
    Ok(RecoveredBlock::new_unhashed(block, vec![signer]))
}

/// Patch output-derived header fields into a block after EVM execution.
///
/// Computes `receipts_root` and `logs_bloom` using the same functions as reth's
/// `EthBlockAssembler`, then sets `state_root` and `gas_used` from the provided values.
fn finalize_block(
    exec_block: RecoveredBlock<Block>,
    receipts: &[Receipt],
    gas_used: u64,
    state_root: B256,
) -> RecoveredBlock<Block> {
    let receipts_root = calculate_receipt_root(
        &receipts
            .iter()
            .map(|r| r.with_bloom_ref())
            .collect::<Vec<_>>(),
    );
    let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| r.logs.iter()));

    let signers = exec_block.senders().to_vec();
    let mut block = exec_block.into_block();
    block.header.state_root = state_root;
    block.header.gas_used = gas_used;
    block.header.receipts_root = receipts_root;
    block.header.logs_bloom = logs_bloom;
    RecoveredBlock::new_unhashed(block, signers)
}

/// Build the genesis block header.
fn build_genesis_header(genesis: &Genesis, state_root: B256) -> Header {
    Header {
        number: 0,
        state_root,
        gas_limit: genesis.gas_limit,
        timestamp: genesis.timestamp,
        base_fee_per_gas: genesis.base_fee_per_gas.map(|f| f as u64),
        difficulty: genesis.difficulty,
        extra_data: genesis.extra_data.clone(),
        ommers_hash: alloy_primitives::b256!(
            "1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347"
        ),
        ..Default::default()
    }
}

// ── Write segment helper ──────────────────────────────────────────────────────

/// Write a length-prefixed segment (4-byte LE u32 + data) into `buf`.
fn write_segment(buf: &mut Vec<u8>, data: &[u8]) {
    let len = data.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(data);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_network::TxSignerSync;
    use alloy_primitives::{TxKind, U256};
    use alloy_signer_local::PrivateKeySigner;

    // Hardhat/Anvil account 0 — funded in TEST_GENESIS.
    pub const SIGNER_KEY: &str = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    // Pre-Merge (London only) genesis — keeps EVM execution simple: no prevrandao,
    // no withdrawals root, no beacon block root.
    pub const TEST_GENESIS: &str = r#"{
        "config": {
            "chainId": 1337,
            "homesteadBlock": 0,
            "eip150Block": 0,
            "eip155Block": 0,
            "eip158Block": 0,
            "byzantiumBlock": 0,
            "constantinopleBlock": 0,
            "petersburgBlock": 0,
            "istanbulBlock": 0,
            "berlinBlock": 0,
            "londonBlock": 0
        },
        "nonce": "0x0",
        "timestamp": "0x0",
        "extraData": "0x",
        "gasLimit": "0x1c9c380",
        "difficulty": "0x20000",
        "baseFeePerGas": "0x3B9ACA00",
        "alloc": {
            "0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266": {
                "balance": "0x10000000000000000000"
            }
        }
    }"#;

    /// Build a minimal signed EIP-1559 ETH-transfer (21 000 gas) from `SIGNER_KEY`.
    pub fn make_signed_transfer(nonce: u64) -> Vec<u8> {
        let signer: PrivateKeySigner = SIGNER_KEY.parse().unwrap();
        let mut tx = TxEip1559 {
            chain_id: 1337,
            nonce,
            gas_limit: 21_000,
            max_fee_per_gas: 2_000_000_000,
            max_priority_fee_per_gas: 1_000_000_000,
            to: TxKind::Call(alloy_primitives::address!(
                "0000000000000000000000000000000000000001"
            )),
            value: U256::from(1u64),
            ..Default::default()
        };
        let sig = signer.sign_transaction_sync(&mut tx).unwrap();
        let envelope = alloy_consensus::TxEnvelope::Eip1559(tx.into_signed(sig));
        let mut buf = Vec::new();
        envelope.encode_2718(&mut buf);
        buf
    }

    #[test]
    fn speculative_apply_advances_block_number() {
        let mut state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();
        assert_eq!(state.block_number, 0);

        let raw = make_signed_transfer(0);
        state.apply_transaction_speculative(&raw).unwrap();

        assert_eq!(state.block_number, 1);
        assert_eq!(state.header_history.len(), 2); // genesis + block 1
    }

    #[test]
    fn speculative_apply_changes_state_root() {
        let mut state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();
        let root_before = state.state_root;

        let raw = make_signed_transfer(0);
        state.apply_transaction_speculative(&raw).unwrap();

        // Balance was transferred so the trie root must change.
        assert_ne!(state.state_root, root_before);
    }

    #[test]
    fn snapshot_clone_restores_state() {
        let mut state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();
        let snapshot = state.clone();

        let raw = make_signed_transfer(0);
        state.apply_transaction_speculative(&raw).unwrap();

        assert_ne!(state.block_number, snapshot.block_number);
        assert_ne!(state.state_root, snapshot.state_root);

        // Roll back.
        state = snapshot.clone();
        assert_eq!(state.block_number, snapshot.block_number);
        assert_eq!(state.state_root, snapshot.state_root);
    }

    #[test]
    fn rollback_removes_receipt_and_header() {
        let mut state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();
        let raw_a = make_signed_transfer(0);
        let raw_b = make_signed_transfer(1);

        // Apply A speculatively, snapshot, then apply B.
        state.apply_transaction_speculative(&raw_a).unwrap();
        state.record_settled_receipt(&raw_a, true);
        let snapshot_before_b = state.clone();

        state.apply_transaction_speculative(&raw_b).unwrap();
        state.record_settled_receipt(&raw_b, true);

        assert_eq!(state.block_number, 2);
        let hash_b: [u8; 32] = *alloy_primitives::keccak256(&raw_b);
        assert!(state.settled_receipts.contains_key(&hash_b));

        // Roll back to before B.
        state = snapshot_before_b;

        assert_eq!(state.block_number, 1);
        assert!(
            !state.settled_receipts.contains_key(&hash_b),
            "B's receipt must be gone after rollback"
        );
        let hash_a: [u8; 32] = *alloy_primitives::keccak256(&raw_a);
        assert!(
            state.settled_receipts.contains_key(&hash_a),
            "A's receipt must survive rollback"
        );
    }

    #[test]
    fn sequential_speculative_applies_build_correct_chain() {
        let mut state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();

        for nonce in 0..3u64 {
            let raw = make_signed_transfer(nonce);
            state.apply_transaction_speculative(&raw).unwrap();
        }

        assert_eq!(state.block_number, 3);
        // genesis + 3 blocks
        assert_eq!(state.header_history.len(), 4);
        // Each block chains correctly.
        let headers: Vec<_> = state.header_history.iter().collect();
        for i in 1..headers.len() {
            assert_eq!(
                headers[i].parent_hash(),
                headers[i - 1].hash(),
                "block {i} parent_hash must equal block {}'s hash",
                i - 1
            );
        }
    }
}
