use alloy_consensus::proofs::calculate_transaction_root;
use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_consensus::Header;
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
use reth_chainspec::{ChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Block, BlockBody, TransactionSigned};
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
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
use tracing::{debug, warn};

// ── EthChainState ─────────────────────────────────────────────────────────────

/// In-memory per-account EVM state (flat; trie is rebuilt as needed).
#[derive(Debug, Clone, Default)]
pub struct AccountState {
    pub balance: U256,
    pub nonce: u64,
    /// Raw bytecode (empty for EOAs).
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

        // Execute with placeholder gas_used=0 first to get actual gas consumed.
        let block_placeholder = build_synthetic_block_inner(
            tx.clone(),
            signer,
            &parent,
            self.block_number + 1,
            &self.chain_spec,
            new_root,
            0,
        )?;

        let mut db = self.build_cache_db();
        let evm_config = EthEvmConfig::new(self.chain_spec.clone());
        let output = evm_config
            .executor(&mut db)
            .execute(&block_placeholder)
            .context("EVM execution failed in apply_transaction")?;

        // Rebuild the block with the real gas_used so the stored header is valid.
        let block = build_synthetic_block_inner(
            tx,
            signer,
            &parent,
            self.block_number + 1,
            &self.chain_spec,
            new_root,
            output.gas_used,
        )?;

        self.merge_bundle(&output.state);

        self.state_root = new_root;
        self.block_number += 1;

        let new_header = SealedHeader::seal_slow(block.header().clone());
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

        let mut db = self.build_cache_db();
        let evm_config = EthEvmConfig::new(self.chain_spec.clone());

        // First pass with placeholder state_root/gas_used to get the execution output.
        let block_placeholder = build_synthetic_block_inner(
            tx.clone(),
            signer,
            &parent,
            self.block_number + 1,
            &self.chain_spec,
            B256::ZERO,
            0,
        )?;

        let mut witness_record = ExecutionWitnessRecord::default();
        let output = evm_config
            .executor(&mut db)
            .execute_with_state_closure(&block_placeholder, |state_db| {
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

        // ── Step 4: Build final block with correct post-state root + gas_used ───

        let final_block = build_synthetic_block_inner(
            tx,
            signer,
            &parent,
            self.block_number + 1,
            &self.chain_spec,
            post_state_root,
            output.gas_used,
        )?;

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

        let witness = ExecutionWitness {
            state: witness_nodes,
            codes: witness_record.codes,
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

/// Construct a `RecoveredBlock<Block>` containing `tx` as the sole transaction.
fn build_synthetic_block_inner(
    tx: TransactionSigned,
    signer: Address,
    parent: &SealedHeader,
    block_number: u64,
    chain_spec: &ChainSpec,
    state_root: B256,
    gas_used: u64,
) -> Result<RecoveredBlock<Block>> {
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .max(parent.timestamp() + 1);

    let base_fee_per_gas = if chain_spec.is_london_active_at_block(block_number) {
        Some(calc_next_base_fee(parent.header(), chain_spec))
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
        state_root,
        transactions_root,
        receipts_root: EMPTY_ROOT_HASH,
        withdrawals_root,
        logs_bloom: Default::default(),
        difficulty: U256::ZERO,
        number: block_number,
        gas_limit: parent.gas_limit(),
        gas_used,
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

/// Calculate EIP-1559 next base fee.
fn calc_next_base_fee(parent: &Header, _chain_spec: &ChainSpec) -> u64 {
    const DENOM: u64 = 8;
    const TARGET_RATIO: u64 = 2;

    let parent_base_fee = parent.base_fee_per_gas.unwrap_or(1_000_000_000);
    let gas_used = parent.gas_used;
    let gas_target = parent.gas_limit / TARGET_RATIO;

    if gas_used == gas_target {
        parent_base_fee
    } else if gas_used > gas_target {
        let delta = parent_base_fee.saturating_mul(gas_used - gas_target) / gas_target / DENOM;
        parent_base_fee.saturating_add(delta.max(1))
    } else {
        let delta = parent_base_fee.saturating_mul(gas_target - gas_used) / gas_target / DENOM;
        parent_base_fee.saturating_sub(delta)
    }
}

// ── Write segment helper ──────────────────────────────────────────────────────

/// Write a length-prefixed segment (4-byte LE u32 + data) into `buf`.
fn write_segment(buf: &mut Vec<u8>, data: &[u8]) {
    let len = data.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(data);
}
