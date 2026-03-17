pub mod handlers;
pub mod types;

use anyhow::{Context, Result};
use axum::{extract::State, http::Method, routing::post, Json, Router};
use hyli_modules::{
    bus::SharedMessageBus,
    module_bus_client, module_handle_messages,
    modules::{BuildApiContextInner, Module},
};
use sdk::ContractName;
use std::sync::Arc;
use tower_http::cors::{Any, CorsLayer};
use tracing::info;

use handlers::RouterCtx;
use types::{JsonRpcRequest, JsonRpcResponse};

module_bus_client! {
    struct RpcProxyBusClient {}
}

pub struct HylaneRpcProxyCtx {
    pub port: u16,
    pub node_url: String,
    pub hyli_chain_id: u64,
    pub bridge_cn: ContractName,
    pub hyperlane_cn: ContractName,
    pub relayer_identity: sdk::Identity,
    pub api: Arc<BuildApiContextInner>,
}

pub struct HylaneRpcProxyModule {
    port: u16,
    bus: RpcProxyBusClient,
}

impl Module for HylaneRpcProxyModule {
    type Context = HylaneRpcProxyCtx;

    async fn build(bus: SharedMessageBus, ctx: Self::Context) -> Result<Self> {
        let router_ctx = RouterCtx::new(
            ctx.node_url,
            ctx.hyli_chain_id,
            ctx.bridge_cn,
            ctx.hyperlane_cn,
            ctx.relayer_identity,
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

        Ok(HylaneRpcProxyModule {
            port: ctx.port,
            bus: RpcProxyBusClient::new_from_bus(bus.new_handle()).await,
        })
    }

    async fn run(&mut self) -> Result<()> {
        self.serve().await
    }
}

impl HylaneRpcProxyModule {
    async fn serve(&mut self) -> Result<()> {
        info!(
            "📡  Starting Hyperlane JSON-RPC proxy on port {}",
            self.port
        );

        // We keep never resolving loop to keep the module alive
        let _ = module_handle_messages! {
            on_self self,
        };
        Ok(())
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
