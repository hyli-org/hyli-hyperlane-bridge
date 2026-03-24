pub mod eth_chain_state;
pub mod handlers;
pub mod types;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{Method, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use eth_chain_state::{extract_pending_proofs, submit_reth_proof, EthChainState, PendingProofsMap};
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
use std::sync::atomic::{AtomicBool, Ordering};
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
    /// Settled EVM state: advances only when a transaction is confirmed on Hyli.
    /// This is the state exposed via RPC so clients never observe speculative changes.
    settled_eth_chain_state: Arc<RwLock<EthChainState>>,
    /// Genesis state — fallback base for `get_state_of_prev_tx` when no history entry exists.
    base_state: EthChainState,
    pending_proofs: PendingProofsMap,
    catching_up: bool,
    is_ready: Arc<AtomicBool>,
    /// Ordered list of tx IDs that have been sequenced but not yet settled.
    tx_chain: Vec<sdk::TxId>,
    /// Post-state (and success flag) recorded *after* each tx in `tx_chain` was speculatively
    /// applied.  Used to find the correct pre-state for rollback/replay (same pattern as
    /// `AutoProver::state_history`).
    state_history: indexmap::IndexMap<sdk::TxId, (EthChainState, bool)>,
}

impl Module for RethModule {
    type Context = RethModuleCtx;

    async fn build(bus: SharedMessageBus, ctx: Self::Context) -> Result<Self> {
        let base_state = EthChainState::new(&ctx.evm_config_json)
            .context("Initializing base EthChainState from genesis JSON")?;

        let settled_eth_chain_state = Arc::new(RwLock::new(
            EthChainState::new(&ctx.evm_config_json)
                .context("Initializing settled EthChainState from genesis JSON")?,
        ));

        let is_ready = Arc::new(AtomicBool::new(false));

        let router_ctx = RouterCtx::new(
            ctx.node_url.clone(),
            ctx.hyli_chain_id,
            ctx.bridge_cn.clone(),
            ctx.hyperlane_cn.clone(),
            ctx.relayer_identity,
            Arc::clone(&settled_eth_chain_state),
            Arc::clone(&is_ready),
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
            settled_eth_chain_state,
            base_state,
            pending_proofs: PendingProofsMap::new(),
            catching_up: true,
            is_ready,
            tx_chain: Vec::new(),
            state_history: indexmap::IndexMap::new(),
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
    /// During catch-up: buffer the proofs, add to `tx_chain`.
    /// In live mode: for each reth blob in order — snapshot pre-state → prove → speculative apply.
    /// `state_history[tx_id]` always holds the post-state of the *last* blob so that subsequent
    /// transactions build on the correct pre-state.
    async fn handle_sequenced_tx(&mut self, contract_tx: ContractTx) -> Result<()> {
        let ContractTx {
            tx_id, tx, tx_ctx, ..
        } = contract_tx;

        let proofs = extract_pending_proofs(tx_id.clone(), tx, tx_ctx, &self.contract_name);
        if proofs.is_empty() {
            return Ok(());
        }

        self.tx_chain.push(tx_id.clone());

        // Index messageId for every reth blob so the frontend can poll before settlement.
        if let Ok(mut state) = self.settled_eth_chain_state.write() {
            for p in &proofs {
                state.index_process_message_id(&p.raw_eip2718, hex::encode(&tx_id.1 .0));
            }
        }

        if self.catching_up {
            self.pending_proofs.insert(tx_id, proofs);
            return Ok(());
        }

        // Live mode ──────────────────────────────────────────────────────────
        //
        // Prove each blob in order, advancing the speculative state between blobs so that
        // blob[i+1] is proved against the post-state of blob[i].
        for pending in &proofs {
            // 1. Snapshot the current pre-state and submit the proof.
            let pre_state = self.get_current_state();
            self.try_prove(pre_state, pending.clone()).await;

            // 2. Advance state speculatively.
            let mut state = self.get_current_state();
            let success = match state.apply_transaction_speculative(&pending.raw_eip2718) {
                Ok(logs) => {
                    state.record_settled_receipt(&pending.raw_eip2718, true, logs);
                    info!(
                        tx_id =% tx_id,
                        blob_index = pending.blob_index.0,
                        block_number = state.block_number,
                        "Speculatively applied reth blob"
                    );
                    true
                }
                Err(e) => {
                    warn!(
                        tx_id =% tx_id,
                        blob_index = pending.blob_index.0,
                        "Speculative apply failed, using fallback header: {e:#}"
                    );
                    state.block_number += 1;
                    state.push_fallback_header();
                    false
                }
            };
            // 3. Overwrite state_history[tx_id] with the latest intermediate (or final) state.
            //    After the loop this entry holds the post-state of the last blob — the correct
            //    anchor for subsequent transactions and rollback.
            self.state_history.insert(tx_id.clone(), (state, success));
        }

        self.pending_proofs.insert(tx_id, proofs);
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
        self.is_ready.store(true, Ordering::Relaxed);

        // For txs still pending settlement: prove each blob in order → speculative apply → record.
        let pending_ids: Vec<sdk::TxId> = self.pending_proofs.keys().cloned().collect();
        for tx_id in &pending_ids {
            let Some(proofs) = self.pending_proofs.get(tx_id).cloned() else {
                continue;
            };

            // Index messageId for every blob buffered during catch-up.
            if let Ok(mut state) = self.settled_eth_chain_state.write() {
                for p in &proofs {
                    state.index_process_message_id(&p.raw_eip2718, hex::encode(&tx_id.1 .0));
                }
            }

            for pending in &proofs {
                let pre_state = self.get_current_state();
                self.try_prove(pre_state, pending.clone()).await;

                let mut state = self.get_current_state();
                let success = match state.apply_transaction_speculative(&pending.raw_eip2718) {
                    Ok(logs) => {
                        state.record_settled_receipt(&pending.raw_eip2718, true, logs);
                        true
                    }
                    Err(e) => {
                        warn!(
                            tx_id =% tx_id,
                            blob_index = pending.blob_index.0,
                            "Post-backfill speculative apply failed: {e:#}"
                        );
                        state.block_number += 1;
                        state.push_fallback_header();
                        false
                    }
                };
                self.state_history.insert(tx_id.clone(), (state, success));
            }
        }
    }

    /// Handle a settled transaction (success, failure, or timeout).
    async fn handle_settled_tx(&mut self, contract_tx: ContractTx) -> Result<()> {
        let ContractTx {
            tx_id,
            tx,
            tx_ctx,
            contract_changes,
            status,
        } = contract_tx;

        // During catch-up, txs that were already settled before this server instance started
        // will have a SettledTx event but no prior SequencedTx event (because
        // query_sequenced_txs only returns txs still in 'sequenced' state).
        // Extract proofs directly from the blob data so the EVM state and receipts are
        // replayed correctly.
        if self.catching_up && !self.tx_chain.contains(&tx_id) {
            let proofs = eth_chain_state::extract_pending_proofs(
                tx_id.clone(),
                tx,
                tx_ctx,
                &self.contract_name,
            );
            if proofs.is_empty() {
                return Ok(());
            }
            self.tx_chain.push(tx_id.clone());
            self.pending_proofs.insert(tx_id.clone(), proofs);
        }

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
    ///
    /// All reth blobs are applied speculatively in order; the final state root is then overridden
    /// with the canonical value from Hyli (which is the root after *all* blobs in the tx).
    fn settle_success_catching_up(
        &mut self,
        tx_id: &sdk::TxId,
        contract_changes: &HashMap<ContractName, ContractChangeData>,
    ) {
        let proofs: Vec<_> = self.pending_proofs.get(tx_id).cloned().unwrap_or_default();

        if let Some(change) = contract_changes.get(&self.contract_name) {
            let new_root_bytes = &change.state_commitment;
            if new_root_bytes.len() == 32 {
                let mut new_root = [0u8; 32];
                new_root.copy_from_slice(new_root_bytes);
                let canonical = alloy_primitives::B256::from(new_root);

                let mut state = self.get_current_state();
                if proofs.is_empty() {
                    // No reth blobs for this tx — advance state with a fallback header and set root.
                    state.state_root = canonical;
                    state.block_number += 1;
                    state.push_fallback_header();
                    warn!(tx_id =% tx_id, "no raw EIP-2718 — receipt NOT stored");
                } else {
                    // Apply each blob in order.
                    //
                    // Intermediate blobs: apply speculatively (no per-blob canonical root).
                    // Last blob: apply with the canonical root so the sealed header pushed to
                    //   `header_history` embeds the canonical root — matching the old single-blob
                    //   behaviour of `apply_transaction(raw, new_root)`.  This keeps
                    //   `header_history.back().state_root == self.state_root == canonical`
                    //   as the invariant for subsequent proof building.
                    let last_idx = proofs.len() - 1;
                    for (i, pending) in proofs.iter().enumerate() {
                        let logs = if i < last_idx {
                            match state.apply_transaction_speculative(&pending.raw_eip2718) {
                                Ok(logs) => logs,
                                Err(e) => {
                                    warn!(
                                        tx_id =% tx_id,
                                        blob_index = pending.blob_index.0,
                                        "apply_transaction (speculative) failed, using fallback: {e:#}"
                                    );
                                    state.block_number += 1;
                                    state.push_fallback_header();
                                    vec![]
                                }
                            }
                        } else {
                            // Last blob: seal the header with the canonical root.
                            match state.apply_transaction(&pending.raw_eip2718, new_root) {
                                Ok(logs) => logs,
                                Err(e) => {
                                    warn!(
                                        tx_id =% tx_id,
                                        blob_index = pending.blob_index.0,
                                        "apply_transaction (canonical) failed, using fallback: {e:#}"
                                    );
                                    state.state_root = canonical;
                                    state.block_number += 1;
                                    state.push_fallback_header();
                                    vec![]
                                }
                            }
                        };
                        state.record_settled_receipt(&pending.raw_eip2718, true, logs);
                    }
                }
                info!(
                    tx_id =% tx_id,
                    block_number = state.block_number,
                    "✅ Settled hyperlane tx (catch-up)"
                );
                // Insert into state_history so subsequent catch-up txs build on this state.
                self.state_history
                    .insert(tx_id.clone(), (state.clone(), true));
                // Mirror the settled state to the RPC-facing settled state.
                match self.settled_eth_chain_state.write() {
                    Ok(mut s) => *s = state,
                    Err(e) => warn!("settled_eth_chain_state write lock poisoned: {:?}", e),
                }
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

                // Advance the settled (RPC-facing) state using the post-state from history,
                // correcting the state root to the canonical value from Hyli.
                if let Some((hist_state, _success)) = self.state_history.get_mut(tx_id) {
                    if hist_state.state_root != canonical {
                        warn!(
                            tx_id =% tx_id,
                            speculative =% hist_state.state_root,
                            %canonical,
                            "Speculative root differs from canonical — correcting"
                        );
                        hist_state.state_root = canonical;
                    }
                    let settled = hist_state.clone();
                    match self.settled_eth_chain_state.write() {
                        Ok(mut s) => *s = settled,
                        Err(e) => warn!("settled_eth_chain_state write lock poisoned: {:?}", e),
                    }
                }

                // Prune previous tx's history entry (it's no longer needed as a rollback base).
                // The settled tx becomes the new anchor, mirroring AutoProver::settle_tx_success.
                let prev_id = self
                    .tx_chain
                    .iter()
                    .position(|id| id == tx_id)
                    .and_then(|pos| {
                        if pos > 0 {
                            self.tx_chain.get(pos - 1)
                        } else {
                            None
                        }
                    })
                    .cloned();
                if let Some(prev) = prev_id {
                    self.state_history.shift_remove(&prev);
                }

                // Keep tx_id in tx_chain as an anchor for subsequent rollbacks.
                if let Some(pos) = self.tx_chain.iter().position(|id| id == tx_id) {
                    self.tx_chain = self.tx_chain.split_off(pos);
                }

                info!(tx_id =% tx_id, "✅ Settled hyperlane tx (live)");
            }
        }

        self.pending_proofs.shift_remove(tx_id);
    }

    /// Remove a failed/timed-out tx from tracking during catch-up (no speculative state to roll back).
    fn settle_failed_catching_up(&mut self, tx_id: &sdk::TxId) {
        info!(tx_id =% tx_id, "🔥 Failed/timed-out tx during catch-up — removing from chain");
        self.tx_chain.retain(|id| id != tx_id);
        self.pending_proofs.shift_remove(tx_id);
    }

    /// Find the `EthChainState` that existed *before* `tx_id` was applied.
    ///
    /// Mirrors `AutoProver::get_state_of_prev_tx`: walks `tx_chain` backwards from `tx_id`'s
    /// position and returns the most recent entry in `state_history`, falling back to
    /// `base_state` (genesis) when no prior history exists.
    fn get_state_of_prev_tx(&self, tx_id: &sdk::TxId) -> EthChainState {
        let pos = self.tx_chain.iter().position(|id| id == tx_id);
        let Some(pos) = pos else {
            warn!(
                tx_id =% tx_id,
                "tx not in tx_chain — returning base state"
            );
            return self.base_state.clone();
        };
        for idx in (0..pos).rev() {
            let prev = &self.tx_chain[idx];
            if let Some((state, _)) = self.state_history.get(prev) {
                return state.clone();
            }
        }
        self.base_state.clone()
    }

    /// Roll back `EthChainState` to the state before `tx_id` was applied (using `state_history`),
    /// then re-prove every subsequent pending tx against the corrected state.
    ///
    /// Mirrors `AutoProver::settle_tx_failed` + `clear_state_history_after_failed`.
    async fn rollback_and_replay(&mut self, tx_id: &sdk::TxId) {
        let Some(pos) = self.tx_chain.iter().position(|id| id == tx_id) else {
            warn!(tx_id =% tx_id, "Failed tx not in tx_chain — skipping rollback");
            self.pending_proofs.shift_remove(tx_id);
            return;
        };

        // Derive pre-state from history instead of a stored snapshot.
        let pre_state = self.get_state_of_prev_tx(tx_id);

        info!(
            tx_id =% tx_id,
            rollback_block = pre_state.block_number,
            subsequent = self.tx_chain.len().saturating_sub(pos + 1),
            "🔄 Rolling back EthChainState and replaying subsequent txs"
        );

        // Remove the failed tx from history and chain.
        // No explicit state restore needed: clearing state_history entries back to the anchor
        // means get_current_state() will return the correct pre-state for replay.
        self.state_history.shift_remove(tx_id);
        self.tx_chain.remove(pos);
        self.pending_proofs.shift_remove(tx_id);

        // Clear state_history for all subsequent txs — they must be re-executed and re-proved
        // against the corrected pre-state (mirrors AutoProver::clear_state_history_after_failed).
        let subsequent: Vec<sdk::TxId> = self.tx_chain[pos..].to_vec();
        for sub_id in &subsequent {
            self.state_history.shift_remove(sub_id);
        }

        // Re-prove and re-apply each subsequent tx in order.
        // For txs with multiple reth blobs, iterate through each blob in order so that every
        // blob is re-proved against the correct intermediate pre-state.
        for sub_id in &subsequent {
            let Some(proofs) = self.pending_proofs.get(sub_id).cloned() else {
                continue;
            };

            for pending in &proofs {
                // Re-prove against the now-correct pre-state.
                let pre_state = self.get_current_state();
                self.try_prove(pre_state, pending.clone()).await;

                // Re-advance state speculatively and overwrite state_history[sub_id] with latest.
                let mut state = self.get_current_state();
                let success = match state.apply_transaction_speculative(&pending.raw_eip2718) {
                    Ok(logs) => {
                        state.record_settled_receipt(&pending.raw_eip2718, true, logs);
                        true
                    }
                    Err(e) => {
                        warn!(
                            tx_id =% sub_id,
                            blob_index = pending.blob_index.0,
                            "Re-speculative apply failed, using fallback: {e:#}"
                        );
                        state.block_number += 1;
                        state.push_fallback_header();
                        false
                    }
                };
                // sub_id not in state_history (was shift_removed above), so insert appends at the end.
                // Subsequent blobs update in-place, keeping sub_id as next_back() for the next iteration.
                self.state_history.insert(sub_id.clone(), (state, success));
            }
        }
    }

    /// Returns the current speculative EVM state: the post-state of the latest tx in
    /// `state_history`, or `base_state` (genesis) if no history exists yet.
    fn get_current_state(&self) -> EthChainState {
        self.state_history
            .values()
            .next_back()
            .map(|(s, _)| s.clone())
            .unwrap_or_else(|| self.base_state.clone())
    }

    /// Attempt to build and submit a reth proof for a pending transaction.
    ///
    /// `pre_state` must be the EVM state that existed *before* `pending` is applied — the caller
    /// is responsible for snapshotting it at the right moment, especially when multiple blobs
    /// within one Hyli tx are proved sequentially.
    async fn try_prove(
        &self,
        pre_state: EthChainState,
        pending: eth_chain_state::PendingRethProof,
    ) {
        let tx_id = pending.tx_id.clone();

        match pre_state.build_proof_payload(&pending) {
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
) -> impl IntoResponse {
    if !ctx.is_ready.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(JsonRpcResponse::err(
                req.id,
                -32000,
                "Server is still syncing",
            )),
        )
            .into_response();
    }

    let id = req.id.clone();
    let params = &req.params;

    info!(method = %req.method, params = %params, "→ RPC request");

    let resp = match req.method.as_str() {
        "eth_blockNumber" => handlers::eth_block_number(&ctx, id),
        "eth_chainId" => handlers::eth_chain_id(&ctx, id),
        "net_version" => handlers::net_version(&ctx, id),
        "eth_getBlockByNumber" => handlers::eth_get_block_by_number(&ctx, id, params),
        "eth_getLogs" => handlers::eth_get_logs(&ctx, id, params),
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
        "hyli_getTxByMessageId" => handlers::hyli_get_tx_by_message_id(&ctx, id, params),
        other => JsonRpcResponse::method_not_found(id, other),
    };

    if let Some(err) = resp.error.as_ref() {
        warn!(method = %req.method, code = err.code, message = %err.message, "← RPC error response");
    } else {
        info!(method = %req.method, "← RPC ok");
    }

    Json(resp).into_response()
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
    use sdk::BlobData;
    use sdk::{BlobTransaction, Identity, TxId};
    use std::sync::Arc; // used indirectly via sdk::BlobData in make_contract_tx

    fn make_tx_id(n: u8) -> TxId {
        TxId(sdk::DataProposalHash(vec![n; 32]), sdk::TxHash(vec![n; 32]))
    }

    /// Build a `ContractTx` carrying one reth blob per entry in `raws`.
    ///
    /// Calling `m.handle_sequenced_tx(make_contract_tx(...)).await.unwrap()` goes through the
    /// exact same production path — `extract_pending_proofs`, `try_prove`, speculative apply —
    /// so tests track any future changes to that logic automatically.
    /// `try_prove` will fail to reach the node but logs the error and returns; the speculative
    /// apply still runs, which is all the tests need.
    fn make_contract_tx(tx_id: TxId, raws: &[Vec<u8>], contract_name: &ContractName) -> ContractTx {
        let blobs = raws
            .iter()
            .map(|raw| sdk::Blob {
                contract_name: contract_name.clone(),
                data: BlobData(raw.clone()),
            })
            .collect();
        ContractTx {
            tx_id,
            tx: BlobTransaction::new(Identity("test".into()), blobs),
            tx_ctx: Arc::new(sdk::TxContext::default()),
            status: sdk::api::TransactionStatusDb::Success,
            contract_changes: HashMap::new(),
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

        let base_state = EthChainState::new(TEST_GENESIS.as_bytes()).unwrap();
        let settled_eth_chain_state = Arc::new(RwLock::new(
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
            settled_eth_chain_state,
            base_state,
            pending_proofs: PendingProofsMap::new(),
            catching_up: false,
            is_ready: Arc::new(AtomicBool::new(true)),
            tx_chain: Vec::new(),
            state_history: IndexMap::new(),
        }
    }

    #[tokio::test]
    async fn rollback_restores_state_before_failed_tx() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        m.handle_sequenced_tx(make_contract_tx(
            id_a.clone(),
            &[make_signed_transfer(0)],
            &cn,
        ))
        .await
        .unwrap();
        m.handle_sequenced_tx(make_contract_tx(
            id_b.clone(),
            &[make_signed_transfer(1)],
            &cn,
        ))
        .await
        .unwrap();

        assert_eq!(m.get_current_state().block_number, 2);
        assert_eq!(m.tx_chain.len(), 2);

        // B fails — roll back to after A (block_number = 1).
        m.rollback_and_replay(&id_b).await;

        assert_eq!(m.tx_chain.len(), 1, "only A remains in tx_chain");
        assert!(!m.tx_chain.contains(&id_b));
        assert_eq!(m.get_current_state().block_number, 1);
    }

    #[tokio::test]
    async fn rollback_middle_tx_replays_subsequent() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        let id_c = make_tx_id(3);
        m.handle_sequenced_tx(make_contract_tx(
            id_a.clone(),
            &[make_signed_transfer(0)],
            &cn,
        ))
        .await
        .unwrap();
        m.handle_sequenced_tx(make_contract_tx(
            id_b.clone(),
            &[make_signed_transfer(1)],
            &cn,
        ))
        .await
        .unwrap();
        m.handle_sequenced_tx(make_contract_tx(
            id_c.clone(),
            &[make_signed_transfer(2)],
            &cn,
        ))
        .await
        .unwrap();

        assert_eq!(m.get_current_state().block_number, 3);

        // B fails — A is gone, C must be re-proved (re-applied) against the post-A state.
        m.rollback_and_replay(&id_b).await;

        // tx_chain should still have A and C.
        assert_eq!(m.tx_chain, vec![id_a.clone(), id_c.clone()]);

        // EthChainState should be at block 2 (A + re-applied C).
        assert_eq!(m.get_current_state().block_number, 2);

        // C's post-state (in state_history) must be block 2 (re-applied on top of A).
        let (c_post, _) = m.state_history.get(&id_c).unwrap();
        assert_eq!(c_post.block_number, 2);
    }

    #[tokio::test]
    async fn rollback_first_tx_resets_to_genesis() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        m.handle_sequenced_tx(make_contract_tx(
            id_a.clone(),
            &[make_signed_transfer(0)],
            &cn,
        ))
        .await
        .unwrap();
        m.handle_sequenced_tx(make_contract_tx(
            id_b.clone(),
            &[make_signed_transfer(1)],
            &cn,
        ))
        .await
        .unwrap();

        m.rollback_and_replay(&id_a).await;

        // A rolled back → state goes to block 0 (genesis), then B is re-applied.
        assert_eq!(m.tx_chain, vec![id_b.clone()]);
        assert_eq!(m.get_current_state().block_number, 1);
        // B's post-state (re-applied on genesis) should be block 1.
        let (b_post, _) = m.state_history.get(&id_b).unwrap();
        assert_eq!(
            b_post.block_number, 1,
            "B's post-state should be block 1 after re-apply on genesis"
        );
    }

    #[tokio::test]
    async fn rollback_removes_stale_receipt() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let id_b = make_tx_id(2);
        let raw_b = make_signed_transfer(1);
        let hash_b: [u8; 32] = *alloy_primitives::keccak256(&raw_b);

        m.handle_sequenced_tx(make_contract_tx(
            id_a.clone(),
            &[make_signed_transfer(0)],
            &cn,
        ))
        .await
        .unwrap();
        m.handle_sequenced_tx(make_contract_tx(id_b.clone(), &[raw_b], &cn))
            .await
            .unwrap();

        assert!(m.get_current_state().settled_receipts.contains_key(&hash_b));

        // B fails — its speculative receipt must be removed via rollback.
        m.rollback_and_replay(&id_b).await;

        assert!(
            !m.get_current_state().settled_receipts.contains_key(&hash_b),
            "B's receipt must be removed after rollback"
        );
    }

    #[tokio::test]
    async fn catch_up_failure_removes_from_chain() {
        let mut m = make_module().await;
        m.catching_up = true;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        // In catch-up mode handle_sequenced_tx just buffers (no proving, no EVM execution).
        m.handle_sequenced_tx(make_contract_tx(
            id_a.clone(),
            &[make_signed_transfer(0)],
            &cn,
        ))
        .await
        .unwrap();

        m.settle_failed_catching_up(&id_a);

        assert!(m.tx_chain.is_empty());
        assert!(m.pending_proofs.is_empty());
    }

    // ── Multi-blob tests ──────────────────────────────────────────────────────

    /// Scenario 1: a single Hyli tx carries two reth blobs.  Both must be proved (applied)
    /// in order, advancing the block number by 2 and recording both receipts.
    #[tokio::test]
    async fn multi_blob_sequence_advances_state_per_blob() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();
        let id_a = make_tx_id(1);

        let raw_0 = make_signed_transfer(0);
        let raw_1 = make_signed_transfer(1);
        let hash_0: [u8; 32] = *alloy_primitives::keccak256(&raw_0);
        let hash_1: [u8; 32] = *alloy_primitives::keccak256(&raw_1);

        m.handle_sequenced_tx(make_contract_tx(id_a.clone(), &[raw_0, raw_1], &cn))
            .await
            .unwrap();

        // Both blobs applied sequentially — block number advances by 2.
        assert_eq!(m.get_current_state().block_number, 2);

        // Both EVM tx receipts are recorded.
        let st = m.get_current_state();
        assert!(
            st.settled_receipts.contains_key(&hash_0),
            "blob 0 receipt missing"
        );
        assert!(
            st.settled_receipts.contains_key(&hash_1),
            "blob 1 receipt missing"
        );

        // state_history holds the post-last-blob state (not the intermediate one).
        let (hist, success) = m.state_history.get(&id_a).unwrap();
        assert_eq!(hist.block_number, 2);
        assert!(success);
    }

    /// Scenario 2: a multi-blob Hyli tx fails / times out.  All blob state changes must
    /// be rolled back atomically — as if neither blob was ever applied.
    #[tokio::test]
    async fn multi_blob_rollback_resets_all_blobs() {
        let mut m = make_module().await;
        let cn = m.contract_name.clone();
        let id_a = make_tx_id(1);

        let raw_0 = make_signed_transfer(0);
        let raw_1 = make_signed_transfer(1);
        let hash_0: [u8; 32] = *alloy_primitives::keccak256(&raw_0);
        let hash_1: [u8; 32] = *alloy_primitives::keccak256(&raw_1);

        m.handle_sequenced_tx(make_contract_tx(id_a.clone(), &[raw_0, raw_1], &cn))
            .await
            .unwrap();
        assert_eq!(m.get_current_state().block_number, 2);

        // tx_a fails — both blobs must be fully rolled back.
        m.rollback_and_replay(&id_a).await;

        assert!(
            m.tx_chain.is_empty(),
            "tx_chain must be empty after rollback"
        );
        assert_eq!(
            m.get_current_state().block_number,
            0,
            "state must roll back to genesis"
        );
        let st = m.get_current_state();
        assert!(
            !st.settled_receipts.contains_key(&hash_0),
            "blob 0 receipt must be gone"
        );
        assert!(
            !st.settled_receipts.contains_key(&hash_1),
            "blob 1 receipt must be gone"
        );
    }

    /// Scenario 3: during catch-up a multi-blob Hyli tx settles successfully.
    /// `settle_success_catching_up` must:
    ///   a) apply intermediate blobs speculatively,
    ///   b) apply the last blob via `apply_transaction` so the sealed header carries the
    ///      canonical root (not just the field),
    ///   c) record both receipts and insert the correct state into `state_history`.
    #[tokio::test]
    async fn multi_blob_catchup_settle_canonical_root_and_header() {
        use hyli_modules::modules::contract_listener::ContractChangeData;

        let mut m = make_module().await;
        m.catching_up = true;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let raw_0 = make_signed_transfer(0);
        let raw_1 = make_signed_transfer(1);
        let hash_0: [u8; 32] = *alloy_primitives::keccak256(&raw_0);
        let hash_1: [u8; 32] = *alloy_primitives::keccak256(&raw_1);

        // In catch-up mode handle_sequenced_tx just buffers — no EVM execution yet.
        m.handle_sequenced_tx(make_contract_tx(id_a.clone(), &[raw_0, raw_1], &cn))
            .await
            .unwrap();

        // Use a distinctive canonical root so we can verify it propagates to the header.
        let canonical_root = [0x42u8; 32];
        let mut contract_changes = HashMap::new();
        contract_changes.insert(
            m.contract_name.clone(),
            ContractChangeData {
                change_types: vec![],
                metadata: None,
                verifier: "reth".into(),
                program_id: vec![],
                state_commitment: canonical_root.to_vec(),
                soft_timeout: None,
                hard_timeout: None,
                deleted_at_height: None,
            },
        );

        m.settle_success_catching_up(&id_a, &contract_changes);

        // state_history entry must exist with the canonical root.
        let (hist, success) = m
            .state_history
            .get(&id_a)
            .expect("state_history must have entry for tx_a");
        assert!(success);
        assert_eq!(
            hist.state_root,
            alloy_primitives::B256::from(canonical_root),
            "state_root must equal canonical root"
        );
        assert_eq!(hist.block_number, 2, "both blobs applied = 2 blocks");

        // The last sealed header must also carry the canonical root (apply_transaction path).
        let last_root = hist.header_history.back().unwrap().header().state_root;
        assert_eq!(
            last_root,
            alloy_primitives::B256::from(canonical_root),
            "last sealed header must be sealed with canonical root, not speculative"
        );

        // Both EVM receipts must be stored.
        assert!(
            hist.settled_receipts.contains_key(&hash_0),
            "blob 0 receipt missing"
        );
        assert!(
            hist.settled_receipts.contains_key(&hash_1),
            "blob 1 receipt missing"
        );

        // Tracking cleaned up.
        assert!(m.pending_proofs.is_empty());
        assert!(!m.tx_chain.contains(&id_a));
    }

    /// Scenario 4: multi-blob tx buffered during catch-up, then `handle_backfill_complete`
    /// transitions to live mode.  Both blobs must be proved and applied in order — the same
    /// loop structure as the live-mode sequencing path, but triggered by backfill completion.
    #[tokio::test]
    async fn multi_blob_backfill_complete_applies_blobs_in_order() {
        let mut m = make_module().await;
        m.catching_up = true;
        let cn = m.contract_name.clone();

        let id_a = make_tx_id(1);
        let raw_0 = make_signed_transfer(0);
        let raw_1 = make_signed_transfer(1);
        let hash_0: [u8; 32] = *alloy_primitives::keccak256(&raw_0);
        let hash_1: [u8; 32] = *alloy_primitives::keccak256(&raw_1);

        // Buffer the two-blob tx during catch-up — no state advancement yet.
        m.handle_sequenced_tx(make_contract_tx(id_a.clone(), &[raw_0, raw_1], &cn))
            .await
            .unwrap();

        assert_eq!(
            m.get_current_state().block_number,
            0,
            "no apply during catch-up"
        );
        assert!(m.state_history.is_empty(), "no history during catch-up");

        // Backfill completes: blobs proved and applied in order.
        m.handle_backfill_complete().await;

        assert!(!m.catching_up);
        assert_eq!(
            m.get_current_state().block_number,
            2,
            "both blobs applied = 2 blocks"
        );

        // Both receipts recorded.
        let st = m.get_current_state();
        assert!(
            st.settled_receipts.contains_key(&hash_0),
            "blob 0 receipt missing"
        );
        assert!(
            st.settled_receipts.contains_key(&hash_1),
            "blob 1 receipt missing"
        );

        // state_history holds the post-last-blob state.
        let (hist, success) = m.state_history.get(&id_a).unwrap();
        assert_eq!(hist.block_number, 2);
        assert!(success);
    }
}
