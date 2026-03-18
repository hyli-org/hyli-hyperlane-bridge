pub mod eth_chain_state;
pub mod handlers;
pub mod types;

use anyhow::{Context, Result};
use axum::{extract::State, http::Method, routing::post, Json, Router};
use eth_chain_state::{
    extract_pending_proof, submit_reth_proof, EthChainState, PendingProofsMap,
};
use hyli_modules::{
    bus::SharedMessageBus,
    module_bus_client, module_handle_messages,
    modules::{
        contract_listener::{ContractListenerEvent, ContractTx},
        BuildApiContextInner, Module,
    },
};
use sdk::{ContractName, ProgramId};
use std::sync::{Arc, RwLock};
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};

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
    /// Initial EVM state root for the hyperlane reth contract (32 bytes).
    pub initial_eth_state_root: [u8; 32],
    /// Genesis JSON for the EVM chain-spec (required).
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
}

impl Module for RethModule {
    type Context = RethModuleCtx;

    async fn build(bus: SharedMessageBus, ctx: Self::Context) -> Result<Self> {
        let eth_chain_state = Arc::new(RwLock::new(
            EthChainState::new(ctx.initial_eth_state_root, &ctx.evm_config_json)
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
                self.handle_settled_tx(contract_tx);
            }
            ContractListenerEvent::BackfillComplete(contract_name) => {
                if contract_name == self.contract_name {
                    info!(
                        contract_name =% contract_name,
                        "Backfill complete for hyperlane reth contract, starting live proving"
                    );
                    self.catching_up = false;
                    // Drain any proofs that were buffered during catch-up.
                    let pending: Vec<_> = self.pending_proofs.drain(..).collect();
                    for (_, proof) in pending {
                        self.try_prove(proof).await;
                    }
                }
            }
        }
        Ok(())
    }

    /// Handle a newly sequenced transaction: if it contains a hyperlane reth blob,
    /// queue it for reth proof generation.
    async fn handle_sequenced_tx(&mut self, contract_tx: ContractTx) -> Result<()> {
        let ContractTx {
            tx_id, tx, tx_ctx, ..
        } = contract_tx;

        let Some(pending) = extract_pending_proof(tx_id.clone(), tx, tx_ctx, &self.contract_name)
        else {
            return Ok(());
        };

        debug!(
            tx_id =% tx_id,
            "Sequenced reth tx for hyperlane contract, queueing for proof"
        );

        if self.catching_up {
            // Buffer until backfill is complete so we don't prove stale state.
            self.pending_proofs.insert(tx_id, pending);
        } else {
            self.try_prove(pending).await;
        }

        Ok(())
    }

    /// Handle a settled transaction: update the current EVM state root and apply
    /// state changes from the `ContractChangeData.state_commitment`.
    fn handle_settled_tx(&mut self, contract_tx: ContractTx) {
        let ContractTx {
            tx_id,
            contract_changes,
            ..
        } = contract_tx;

        if let Some(change) = contract_changes.get(&self.contract_name) {
            let new_root_bytes = &change.state_commitment;
            if new_root_bytes.len() == 32 {
                let mut new_root = [0u8; 32];
                new_root.copy_from_slice(new_root_bytes);

                // Try to re-execute and apply state diff; fall back to root-only update.
                let raw_eip2718 = self
                    .pending_proofs
                    .get(&tx_id)
                    .map(|p| p.raw_eip2718.clone());

                if let Ok(mut state) = self.eth_chain_state.write() {
                    if let Some(raw) = raw_eip2718 {
                        if let Err(e) = state.apply_transaction(&raw, new_root) {
                            warn!(
                                tx_id =% tx_id,
                                "apply_transaction failed, falling back to root-only update: {e:#}"
                            );
                            state.state_root = alloy_primitives::B256::from(new_root);
                            state.block_number += 1;
                        }
                    } else {
                        state.state_root = alloy_primitives::B256::from(new_root);
                        state.block_number += 1;
                    }
                    debug!(
                        tx_id =% tx_id,
                        block_number = state.block_number,
                        state_root = hex::encode(new_root),
                        "Updated EVM state root from settled hyperlane tx"
                    );
                }
            } else {
                warn!(
                    tx_id =% tx_id,
                    len = new_root_bytes.len(),
                    "Unexpected state_commitment length for hyperlane contract (expected 32)"
                );
            }
        }

        // Remove from pending (it's now settled, either proven or timed out).
        self.pending_proofs.shift_remove(&tx_id);
    }

    /// Attempt to build and submit a reth proof for a pending transaction.
    async fn try_prove(&self, pending: eth_chain_state::PendingRethProof) {
        let eth_state_snapshot = self
            .eth_chain_state
            .read()
            .map(|s| s.clone())
            .unwrap_or_else(|_| panic!("EthChainState lock poisoned"));

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

    let resp = match req.method.as_str() {
        "eth_blockNumber" => handlers::eth_block_number(&ctx, id).await,
        "eth_chainId" => handlers::eth_chain_id(&ctx, id).await,
        "net_version" => handlers::net_version(&ctx, id).await,
        "eth_getBlockByNumber" => handlers::eth_get_block_by_number(&ctx, id, params).await,
        "eth_getLogs" => handlers::eth_get_logs(&ctx, id, params).await,
        "eth_call" => handlers::eth_call(&ctx, id, params).await,
        "eth_sendTransaction" => handlers::eth_send_raw_transaction(&ctx, id, params).await,
        "eth_sendRawTransaction" => handlers::eth_send_raw_transaction(&ctx, id, params).await,
        "eth_getTransactionReceipt" => {
            handlers::eth_get_transaction_receipt(&ctx, id, params).await
        }
        "eth_estimateGas" => JsonRpcResponse::ok(id, serde_json::json!("0x186a0")),
        "eth_getTransactionCount" => JsonRpcResponse::ok(id, serde_json::json!("0x0")),
        "eth_gasPrice" => JsonRpcResponse::ok(id, serde_json::json!("0x1")),
        "eth_getBalance" => JsonRpcResponse::ok(
            id,
            serde_json::json!("0xde0b6b3a7640000"), // 1 ETH
        ),
        other => JsonRpcResponse::method_not_found(id, other),
    };

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
