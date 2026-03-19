pub mod eth_chain_state;
pub mod handlers;
pub mod types;

use anyhow::{Context, Result};
use axum::{extract::State, http::Method, routing::post, Json, Router};
use eth_chain_state::{extract_pending_proof, submit_reth_proof, EthChainState, PendingProofsMap};
use hyli_modules::{
    bus::SharedMessageBus,
    module_bus_client, module_handle_messages,
    modules::{
        contract_listener::{ContractChangeData, ContractListenerEvent, ContractTx},
        BuildApiContextInner, Module,
    },
};
use sdk::{api::TransactionStatusDb, ContractName, ProgramId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info, warn};

use handlers::RouterCtx;
use types::{JsonRpcRequest, JsonRpcResponse};

module_bus_client! {
    struct RpcProxyBusClient {
        receiver(ContractListenerEvent),
    }
}

pub struct RethModuleCtx {
    pub port: u16,
    pub node_url: String,
    pub hyli_chain_id: u64,
    pub bridge_cn: ContractName,
    pub hyperlane_cn: ContractName,
    pub relayer_identity: sdk::Identity,
    pub api: Arc<BuildApiContextInner>,
    /// Genesis JSON for the EVM chain-spec. The initial state root is derived from it.
    pub evm_config_json: Vec<u8>,
}

pub struct RethModule {
    port: u16,
    bus: RpcProxyBusClient,
    node_client: Arc<client_sdk::rest_client::NodeApiHttpClient>,
    contract_name: ContractName,
    program_id: ProgramId,
    eth_chain_state: Arc<RwLock<EthChainState>>,
    pending_proofs: PendingProofsMap,
    catching_up: bool,
    /// Ordered list of tx IDs that have been sequenced but not yet settled.
    tx_chain: Vec<sdk::TxId>,
    /// EthChainState snapshot taken just *before* each tx in `tx_chain` was applied
    /// speculatively.  Used to roll back and replay if a tx fails or times out.
    state_snapshots: indexmap::IndexMap<sdk::TxId, EthChainState>,
}

impl Module for RethModule {
    type Context = RethModuleCtx;

    async fn build(bus: SharedMessageBus, ctx: Self::Context) -> Result<Self> {
        let eth_chain_state = Arc::new(RwLock::new(
            EthChainState::new(&ctx.evm_config_json)
                .context("Initializing EthChainState from genesis JSON")?,
        ));

        let router_ctx = RouterCtx::new(
            ctx.node_url.clone(),
            ctx.hyli_chain_id,
            ctx.bridge_cn.clone(),
            ctx.hyperlane_cn.clone(),
            ctx.relayer_identity,
            Arc::clone(&eth_chain_state),
        )
        .context("Building RPC proxy router context")?;

        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(vec![Method::GET, Method::POST])
            .allow_headers(Any);

        let api = Router::new()
            .route("/rpc", post(rpc_handler))
            .with_state(router_ctx.clone())
            .layer(cors);

        if let Ok(mut guard) = ctx.api.router.lock() {
            if let Some(router) = guard.take() {
                guard.replace(router.merge(api));
            }
        }

        let node_client = Arc::new(
            client_sdk::rest_client::NodeApiHttpClient::new(ctx.node_url)
                .context("Creating node HTTP client for reth prover")?,
        );

        // Derive the program_id for the hyperlane reth contract from the bridge contract name
        // (matches `init.rs::derive_program_pubkey` and `reth.rs::derive_program_pubkey`).
        let hyperlane_program_id = derive_program_pubkey(&ctx.bridge_cn);

        Ok(RethModule {
            port: ctx.port,
            bus: RpcProxyBusClient::new_from_bus(bus.new_handle()).await,
            node_client,
            contract_name: ctx.hyperlane_cn,
            program_id: hyperlane_program_id,
            eth_chain_state,
            pending_proofs: PendingProofsMap::new(),
            catching_up: true,
            tx_chain: Vec::new(),
            state_snapshots: indexmap::IndexMap::new(),
        })
    }

    async fn run(&mut self) -> Result<()> {
        self.serve().await
    }
}

impl RethModule {
    async fn serve(&mut self) -> Result<()> {
        info!(
            "📡  Starting Hyperlane JSON-RPC proxy on port {}",
            self.port
        );

        let _ = module_handle_messages! {
            on_self self,
            listen<ContractListenerEvent> event => {
                if let Err(e) = self.handle_contract_listener_event(event).await {
                    error!("Error handling ContractListenerEvent: {e:#}");
                }
            }
        };
        Ok(())
    }

    async fn handle_contract_listener_event(&mut self, event: ContractListenerEvent) -> Result<()> {
        match event {
            ContractListenerEvent::SequencedTx(contract_tx) => {
                self.handle_sequenced_tx(contract_tx).await?;
            }
            ContractListenerEvent::SettledTx(contract_tx) => {
                if let Err(e) = self.handle_settled_tx(contract_tx).await {
                    error!("Error handling SettledTx: {e:#}");
                }
            }
            ContractListenerEvent::BackfillComplete(contract_name) => {
                if contract_name == self.contract_name {
                    self.handle_backfill_complete().await;
                }
            }
        }
        Ok(())
    }

    /// Handle a newly sequenced transaction.
    ///
    /// During catch-up: buffer the proof, add to `tx_chain`.
    /// In live mode: snapshot → prove → speculative apply → record receipt.
    async fn handle_sequenced_tx(&mut self, contract_tx: ContractTx) -> Result<()> {
        let ContractTx {
            tx_id, tx, tx_ctx, ..
        } = contract_tx;

        let Some(pending) = extract_pending_proof(tx_id.clone(), tx, tx_ctx, &self.contract_name)
        else {
            return Ok(());
        };

        self.tx_chain.push(tx_id.clone());

        if self.catching_up {
            self.pending_proofs.insert(tx_id, pending);
            return Ok(());
        }

        // Live mode ──────────────────────────────────────────────────────────

        // 1. Snapshot BEFORE speculative execution (used for rollback).
        let snapshot = match self.eth_chain_state.read() {
            Ok(s) => s.clone(),
            Err(e) => {
                warn!("EthChainState read lock was poisoned — recovering");
                e.into_inner().clone()
            }
        };
        self.state_snapshots.insert(tx_id.clone(), snapshot);

        // 2. Prove against the current (pre-speculative) state.
        self.try_prove(pending.clone()).await;

        // 3. Advance state speculatively so the next tx's proof uses the correct pre-state.
        let mut state = match self.eth_chain_state.write() {
            Ok(g) => g,
            Err(e) => {
                warn!("EthChainState write lock was poisoned — recovering");
                e.into_inner()
            }
        };
        match state.apply_transaction_speculative(&pending.raw_eip2718) {
            Ok(()) => {
                // Record the receipt now while block_number is correct for this tx.
                state.record_settled_receipt(&pending.raw_eip2718, true);
                info!(
                    tx_id =% tx_id,
                    block_number = state.block_number,
                    "Speculatively applied tx"
                );
            }
            Err(e) => {
                warn!(
                    tx_id =% tx_id,
                    "Speculative apply failed, using fallback header: {e:#}"
                );
                state.block_number += 1;
                state.push_fallback_header();
            }
        }
        drop(state);

        self.pending_proofs.insert(tx_id, pending);
        Ok(())
    }

    /// Transition out of catch-up mode: start live proving for any txs still pending settlement.
    async fn handle_backfill_complete(&mut self) {
        if !self.catching_up {
            return;
        }
        info!(
            contract_name =% self.contract_name,
            pending = self.pending_proofs.len(),
            "Backfill complete for hyperlane reth contract, starting live proving"
        );
        self.catching_up = false;

        // For txs still pending settlement: snapshot → prove → speculative apply.
        let pending_ids: Vec<sdk::TxId> = self.pending_proofs.keys().cloned().collect();
        for tx_id in &pending_ids {
            let Some(proof) = self.pending_proofs.get(tx_id).cloned() else {
                continue;
            };

            let snapshot = match self.eth_chain_state.read() {
                Ok(s) => s.clone(),
                Err(e) => {
                    warn!("EthChainState read lock was poisoned — recovering");
                    e.into_inner().clone()
                }
            };
            self.state_snapshots.insert(tx_id.clone(), snapshot);

            self.try_prove(proof.clone()).await;

            let mut state = match self.eth_chain_state.write() {
                Ok(g) => g,
                Err(e) => {
                    warn!("EthChainState write lock was poisoned — recovering");
                    e.into_inner()
                }
            };
            match state.apply_transaction_speculative(&proof.raw_eip2718) {
                Ok(()) => {
                    state.record_settled_receipt(&proof.raw_eip2718, true);
                }
                Err(e) => {
                    warn!(tx_id =% tx_id, "Post-backfill speculative apply failed: {e:#}");
                    state.block_number += 1;
                    state.push_fallback_header();
                }
            }
            drop(state);
        }
    }

    /// Handle a settled transaction (success, failure, or timeout).
    async fn handle_settled_tx(&mut self, contract_tx: ContractTx) -> Result<()> {
        let ContractTx {
            tx_id,
            contract_changes,
            status,
            ..
        } = contract_tx;

        // Only process txs that carried a reth blob for our contract.
        if !self.tx_chain.contains(&tx_id) {
            return Ok(());
        }

        info!(
            tx_id =% tx_id,
            ?status,
            has_hyperlane_change = contract_changes.contains_key(&self.contract_name),
            "handle_settled_tx"
        );

        match status {
            TransactionStatusDb::Success => {
                if self.catching_up {
                    self.settle_success_catching_up(&tx_id, &contract_changes);
                } else {
                    self.settle_success_live(&tx_id, &contract_changes);
                }
            }
            TransactionStatusDb::Failure | TransactionStatusDb::TimedOut => {
                if self.catching_up {
                    self.settle_failed_catching_up(&tx_id);
                } else {
                    self.rollback_and_replay(&tx_id).await;
                }
            }
            _ => {}
        }

        Ok(())
    }

    // ── Settlement helpers ────────────────────────────────────────────────────

    /// Apply the canonical state for a successfully-settled tx during catch-up.
    fn settle_success_catching_up(
        &mut self,
        tx_id: &sdk::TxId,
        contract_changes: &HashMap<ContractName, ContractChangeData>,
    ) {
        let raw_eip2718 = self
            .pending_proofs
            .get(tx_id)
            .map(|p| p.raw_eip2718.clone());

        if let Some(change) = contract_changes.get(&self.contract_name) {
            let new_root_bytes = &change.state_commitment;
            if new_root_bytes.len() == 32 {
                let mut new_root = [0u8; 32];
                new_root.copy_from_slice(new_root_bytes);

                let mut state = match self.eth_chain_state.write() {
                    Ok(g) => g,
                    Err(e) => {
                        warn!("EthChainState write lock was poisoned — recovering");
                        e.into_inner()
                    }
                };
                if let Some(ref raw) = raw_eip2718 {
                    if let Err(e) = state.apply_transaction(raw, new_root) {
                        warn!(tx_id =% tx_id, "apply_transaction failed, using fallback: {e:#}");
                        state.state_root = alloy_primitives::B256::from(new_root);
                        state.block_number += 1;
                        state.push_fallback_header();
                    }
                    state.record_settled_receipt(raw, true);
                } else {
                    state.state_root = alloy_primitives::B256::from(new_root);
                    state.block_number += 1;
                    state.push_fallback_header();
                    warn!(tx_id =% tx_id, "no raw EIP-2718 — receipt NOT stored");
                }
                info!(
                    tx_id =% tx_id,
                    block_number = state.block_number,
                    "✅ Settled hyperlane tx (catch-up)"
                );
            } else {
                warn!(
                    tx_id =% tx_id,
                    len = new_root_bytes.len(),
                    "Unexpected state_commitment length (expected 32)"
                );
            }
        }

        self.tx_chain.retain(|id| id != tx_id);
        self.pending_proofs.shift_remove(tx_id);
    }

    /// Confirm the canonical state root for a successfully-settled tx in live mode.
    ///
    /// The EVM state diff was already applied speculatively at sequencing time; only
    /// `state_root` is updated here in case our local computation differed.
    fn settle_success_live(
        &mut self,
        tx_id: &sdk::TxId,
        contract_changes: &HashMap<ContractName, ContractChangeData>,
    ) {
        if let Some(change) = contract_changes.get(&self.contract_name) {
            let new_root_bytes = &change.state_commitment;
            if new_root_bytes.len() == 32 {
                let mut new_root = [0u8; 32];
                new_root.copy_from_slice(new_root_bytes);
                let canonical = alloy_primitives::B256::from(new_root);

                let mut state = match self.eth_chain_state.write() {
                    Ok(g) => g,
                    Err(e) => {
                        warn!("EthChainState write lock was poisoned — recovering");
                        e.into_inner()
                    }
                };
                if state.state_root != canonical {
                    warn!(
                        tx_id =% tx_id,
                        speculative =% state.state_root,
                        %canonical,
                        "Speculative root differs from canonical — correcting"
                    );
                    state.state_root = canonical;
                }
                info!(tx_id =% tx_id, "✅ Settled hyperlane tx (live)");
            }
        }

        self.tx_chain.retain(|id| id != tx_id);
        self.state_snapshots.shift_remove(tx_id);
        self.pending_proofs.shift_remove(tx_id);
    }

    /// Remove a failed/timed-out tx from tracking during catch-up (no speculative state to roll back).
    fn settle_failed_catching_up(&mut self, tx_id: &sdk::TxId) {
        info!(tx_id =% tx_id, "🔥 Failed/timed-out tx during catch-up — removing from chain");
        self.tx_chain.retain(|id| id != tx_id);
        self.pending_proofs.shift_remove(tx_id);
    }

    /// Roll back `EthChainState` to the snapshot taken before `tx_id` was applied,
    /// then re-prove every subsequent pending tx against the corrected state.
    async fn rollback_and_replay(&mut self, tx_id: &sdk::TxId) {
        let Some(pos) = self.tx_chain.iter().position(|id| id == tx_id) else {
            warn!(tx_id =% tx_id, "Failed tx not in tx_chain — skipping rollback");
            self.pending_proofs.shift_remove(tx_id);
            return;
        };

        let Some(rollback_state) = self.state_snapshots.shift_remove(tx_id) else {
            warn!(tx_id =% tx_id, "No snapshot for failed tx — cannot roll back");
            self.tx_chain.remove(pos);
            self.pending_proofs.shift_remove(tx_id);
            return;
        };

        info!(
            tx_id =% tx_id,
            rollback_block = rollback_state.block_number,
            subsequent = self.tx_chain.len().saturating_sub(pos + 1),
            "🔄 Rolling back EthChainState and replaying subsequent txs"
        );

        // Restore state — also removes stale speculative receipts and headers pushed after this tx.
        {
            let mut state = match self.eth_chain_state.write() {
                Ok(g) => g,
                Err(e) => {
                    warn!("EthChainState write lock was poisoned — recovering");
                    e.into_inner()
                }
            };
            *state = rollback_state;
        }

        self.tx_chain.remove(pos);
        self.pending_proofs.shift_remove(tx_id);

        // Re-prove and re-apply each subsequent tx in order.
        let subsequent: Vec<sdk::TxId> = self.tx_chain[pos..].to_vec();
        for sub_id in &subsequent {
            let Some(pending) = self.pending_proofs.get(sub_id).cloned() else {
                continue;
            };

            // Update snapshot — the old one assumed the failed tx had succeeded.
            let new_snapshot = match self.eth_chain_state.read() {
                Ok(s) => s.clone(),
                Err(e) => {
                    warn!("EthChainState read lock was poisoned — recovering");
                    e.into_inner().clone()
                }
            };
            self.state_snapshots.insert(sub_id.clone(), new_snapshot);

            // Re-prove against the now-correct pre-state.
            self.try_prove(pending.clone()).await;

            // Re-advance state speculatively for the next iteration.
            let mut state = match self.eth_chain_state.write() {
                Ok(g) => g,
                Err(e) => {
                    warn!("EthChainState write lock was poisoned — recovering");
                    e.into_inner()
                }
            };
            match state.apply_transaction_speculative(&pending.raw_eip2718) {
                Ok(()) => {
                    state.record_settled_receipt(&pending.raw_eip2718, true);
                }
                Err(e) => {
                    warn!(
                        tx_id =% sub_id,
                        "Re-speculative apply failed, using fallback: {e:#}"
                    );
                    state.block_number += 1;
                    state.push_fallback_header();
                }
            }
            drop(state);
        }
    }

    /// Attempt to build and submit a reth proof for a pending transaction.
    async fn try_prove(&self, pending: eth_chain_state::PendingRethProof) {
        let eth_state_snapshot = match self.eth_chain_state.read() {
            Ok(s) => s.clone(),
            Err(e) => {
                warn!("EthChainState read lock was poisoned — recovering");
                e.into_inner().clone()
            }
        };

        let tx_id = pending.tx_id.clone();

        match eth_state_snapshot.build_proof_payload(&pending) {
            Ok(proof_bytes) => {
                match submit_reth_proof(
                    self.node_client.as_ref(),
                    &self.contract_name,
                    &self.program_id,
                    proof_bytes,
                )
                .await
                {
                    Ok(hash) => {
                        info!(
                            tx_id =% tx_id,
                            proof_hash =% hash,
                            "Submitted reth ProofTransaction for hyperlane tx"
                        );
                    }
                    Err(e) => {
                        error!(
                            tx_id =% tx_id,
                            "Failed to submit reth ProofTransaction: {e:#}"
                        );
                    }
                }
            }
            Err(e) => {
                error!(
                    tx_id =% tx_id,
                    "Failed to build reth proof payload: {e:#}"
                );
            }
        }
    }
}

// ── Axum handler ──────────────────────────────────────────────────────────────

async fn rpc_handler(
    State(ctx): State<RouterCtx>,
    Json(req): Json<JsonRpcRequest>,
) -> Json<JsonRpcResponse> {
    let id = req.id.clone();
    let params = &req.params;

    info!(method = %req.method, params = %params, "→ RPC request");

    let resp = match req.method.as_str() {
        "eth_blockNumber" => handlers::eth_block_number(&ctx, id),
        "eth_chainId" => handlers::eth_chain_id(&ctx, id),
        "net_version" => handlers::net_version(&ctx, id),
        "eth_getBlockByNumber" => handlers::eth_get_block_by_number(&ctx, id, params),
        "eth_getLogs" => handlers::eth_get_logs(&ctx, id, params).await,
        "eth_call" => handlers::eth_call(&ctx, id, params),
        "eth_sendTransaction" => handlers::eth_send_raw_transaction(&ctx, id, params).await,
        "eth_sendRawTransaction" => handlers::eth_send_raw_transaction(&ctx, id, params).await,
        "eth_getTransactionByHash" => handlers::eth_get_transaction_by_hash(&ctx, id, params),
        "eth_getTransactionReceipt" => {
            handlers::eth_get_transaction_receipt(&ctx, id, params).await
        }
        "eth_estimateGas" => handlers::eth_estimate_gas(&ctx, id, params),
        "eth_getTransactionCount" => handlers::eth_get_transaction_count(&ctx, id, params),
        "eth_gasPrice" => handlers::eth_gas_price(&ctx, id),
        "eth_maxPriorityFeePerGas" => handlers::eth_max_priority_fee_per_gas(&ctx, id),
        "eth_feeHistory" => handlers::eth_fee_history(&ctx, id, params),
        "eth_getBalance" => handlers::eth_get_balance(&ctx, id, params),
        "eth_getCode" => handlers::eth_get_code(&ctx, id, params),
        "eth_getStorageAt" => handlers::eth_get_storage_at(&ctx, id, params),
        "debug_dumpGenesis" => handlers::debug_dump_genesis(&ctx, id),
        other => JsonRpcResponse::method_not_found(id, other),
    };

    if let Some(err) = resp.error.as_ref() {
        warn!(method = %req.method, code = err.code, message = %err.message, "← RPC error response");
    } else {
        info!(method = %req.method, "← RPC ok");
    }

    Json(resp)
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Derives the 65-byte uncompressed secp256k1 program ID from a contract name.
/// Mirrors `init.rs::derive_program_pubkey` and `reth.rs::derive_program_pubkey`.
fn derive_program_pubkey(contract_name: &ContractName) -> ProgramId {
    use alloy_primitives::keccak256;
    use k256::ecdsa::SigningKey;

    let mut seed: [u8; 32] = keccak256(contract_name.0.as_bytes()).into();
    let signing_key = loop {
        match SigningKey::from_slice(&seed) {
            Ok(key) => break key,
            Err(_) => {
                seed = keccak256(seed).into();
            }
        }
    };
    let encoded = signing_key.verifying_key().to_encoded_point(false);
    ProgramId(encoded.as_bytes().to_vec())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reth_module::eth_chain_state::tests::{make_signed_transfer, TEST_GENESIS};
    use eth_chain_state::PendingRethProof;
    use sdk::{BlobTransaction, Identity, TxId};
    use std::sync::Arc;

    fn make_tx_id(n: u8) -> TxId {
        TxId(sdk::DataProposalHash(vec![n; 32]), sdk::TxHash(vec![n; 32]))
    }

    fn make_pending(tx_id: TxId, nonce: u64) -> PendingRethProof {
        let raw = make_signed_transfer(nonce);
        PendingRethProof {
            tx_id: tx_id.clone(),
            hyli_tx: BlobTransaction::new(Identity("test".into()), vec![]),
            tx_ctx: Arc::new(sdk::TxContext::default()),
            blob_index: sdk::BlobIndex(0),
            raw_eip2718: raw,
        }
    }

    async fn make_module() -> RethModule {
        use hyli_modules::bus::SharedMessageBus;
        use indexmap::IndexMap;

        // Initialize a noop meter provider so SharedMessageBus::new() doesn't panic.
        static METER_INIT: std::sync::Once = std::sync::Once::new();
        METER_INIT.call_once(|| {
            hyli_turmoil_shims::init_noop_meter_provider();
        });

        let eth_chain_state = Arc::new(RwLock::new(
            EthChainState::new(TEST_GENESIS.as_bytes()).unwrap(),
        ));
        let bus = SharedMessageBus::new();
        let bus_client = RpcProxyBusClient::new_from_bus(bus.new_handle()).await;
        let node_client = Arc::new(
            client_sdk::rest_client::NodeApiHttpClient::new("http://localhost:1".to_string())
                .unwrap(),
        );
        RethModule {
            port: 0,
            bus: bus_client,
            node_client,
            contract_name: ContractName("reth".into()),
            program_id: ProgramId(vec![]),
            eth_chain_state,
            pending_proofs: PendingProofsMap::new(),
            catching_up: false,
            tx_chain: Vec::new(),
            state_snapshots: IndexMap::new(),
        }
    }

    /// Simulate the "snapshot → speculative apply" part of handle_sequenced_tx without
    /// actually sending a proof (try_prove would fail without a live node).
    fn sequence_tx(module: &mut RethModule, tx_id: TxId, pending: PendingRethProof) {
        module.tx_chain.push(tx_id.clone());

        let snapshot = module.eth_chain_state.read().map(|s| s.clone()).unwrap();
        module.state_snapshots.insert(tx_id.clone(), snapshot);

        if let Ok(mut state) = module.eth_chain_state.write() {
            match state.apply_transaction_speculative(&pending.raw_eip2718) {
                Ok(()) => {
                    state.record_settled_receipt(&pending.raw_eip2718, true);
                }
                Err(e) => {
                    state.block_number += 1;
                    state.push_fallback_header();
                    panic!("speculative apply failed in test: {e:#}");
                }
            }
        }

        module.pending_proofs.insert(tx_id, pending);
    }

    #[tokio::test]
    async fn rollback_restores_state_before_failed_tx() {
        let mut m = make_module().await;

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        let pending_a = make_pending(id_a.clone(), 0);
        let pending_b = make_pending(id_b.clone(), 1);

        sequence_tx(&mut m, id_a.clone(), pending_a);
        sequence_tx(&mut m, id_b.clone(), pending_b);

        assert_eq!(m.eth_chain_state.read().unwrap().block_number, 2);
        assert_eq!(m.tx_chain.len(), 2);

        // B fails — roll back to after A (block_number = 1).
        m.rollback_and_replay(&id_b).await;

        assert_eq!(m.tx_chain.len(), 1, "only A remains in tx_chain");
        assert!(!m.tx_chain.contains(&id_b));
        assert_eq!(m.eth_chain_state.read().unwrap().block_number, 1);
    }

    #[tokio::test]
    async fn rollback_middle_tx_replays_subsequent() {
        let mut m = make_module().await;

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        let id_c = make_tx_id(3);
        sequence_tx(&mut m, id_a.clone(), make_pending(id_a.clone(), 0));
        sequence_tx(&mut m, id_b.clone(), make_pending(id_b.clone(), 1));
        sequence_tx(&mut m, id_c.clone(), make_pending(id_c.clone(), 2));

        assert_eq!(m.eth_chain_state.read().unwrap().block_number, 3);

        // B fails — A is gone, C must be re-proved (re-applied) against the post-A state.
        m.rollback_and_replay(&id_b).await;

        // tx_chain should still have A and C.
        assert_eq!(m.tx_chain, vec![id_a.clone(), id_c.clone()]);

        // EthChainState should be at block 2 (A + re-applied C).
        assert_eq!(m.eth_chain_state.read().unwrap().block_number, 2);

        // C's snapshot must have been updated to the post-A state (block 1).
        let c_snapshot = m.state_snapshots.get(&id_c).unwrap();
        assert_eq!(c_snapshot.block_number, 1);
    }

    #[tokio::test]
    async fn rollback_first_tx_resets_to_genesis() {
        let mut m = make_module().await;

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        sequence_tx(&mut m, id_a.clone(), make_pending(id_a.clone(), 0));
        sequence_tx(&mut m, id_b.clone(), make_pending(id_b.clone(), 1));

        m.rollback_and_replay(&id_a).await;

        // A rolled back → state goes to block 0 (genesis), then B is re-applied.
        assert_eq!(m.tx_chain, vec![id_b.clone()]);
        assert_eq!(m.eth_chain_state.read().unwrap().block_number, 1);
        let b_snapshot = m.state_snapshots.get(&id_b).unwrap();
        assert_eq!(b_snapshot.block_number, 0, "B's snapshot should be genesis");
    }

    #[tokio::test]
    async fn rollback_removes_stale_receipt() {
        let mut m = make_module().await;

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        let raw_b = make_signed_transfer(1);
        let hash_b: [u8; 32] = *alloy_primitives::keccak256(&raw_b);

        sequence_tx(&mut m, id_a.clone(), make_pending(id_a.clone(), 0));
        sequence_tx(
            &mut m,
            id_b.clone(),
            PendingRethProof {
                tx_id: id_b.clone(),
                hyli_tx: BlobTransaction::new(Identity("test".into()), vec![]),
                tx_ctx: Arc::new(sdk::TxContext::default()),
                blob_index: sdk::BlobIndex(0),
                raw_eip2718: raw_b,
            },
        );

        assert!(m
            .eth_chain_state
            .read()
            .unwrap()
            .settled_receipts
            .contains_key(&hash_b));

        // B fails — its speculative receipt must be removed via rollback.
        m.rollback_and_replay(&id_b).await;

        assert!(
            !m.eth_chain_state
                .read()
                .unwrap()
                .settled_receipts
                .contains_key(&hash_b),
            "B's receipt must be removed after rollback"
        );
    }

    #[tokio::test]
    async fn catch_up_failure_removes_from_chain() {
        let mut m = make_module().await;
        m.catching_up = true;

        let id_a = make_tx_id(1);
        let pending_a = make_pending(id_a.clone(), 0);
        m.tx_chain.push(id_a.clone());
        m.pending_proofs.insert(id_a.clone(), pending_a);

        m.settle_failed_catching_up(&id_a);

        assert!(m.tx_chain.is_empty());
        assert!(m.pending_proofs.is_empty());
    }
}
