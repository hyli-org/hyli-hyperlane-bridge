use anyhow::{Context, Result};
use axum::Router;
use clap::Parser;
use client_sdk::{helpers::risc0::Risc0Prover, rest_client::NodeApiHttpClient};
use hyli_modules::{
    bus::SharedMessageBus,
    modules::{
        contract_listener::{ContractListener, ContractListenerConf},
        prover::{AutoProver, AutoProverCtx},
        rest::{RestApi, RestApiRunContext},
        BuildApiContextInner, ModulesHandler, ModulesHandlerOptions,
    },
};
use hyperlane_bridge::client::tx_executor_handler::TxExecutorHandler as BridgeTxHandler;
use sdk::{api::NodeInfo, ContractName, Identity};
use server::{
    conf, init,
    reth_module::{RethModule, RethModuleCtx},
};
use std::{collections::HashSet, sync::Arc, time::Duration};
use tracing::info;

#[tokio::main]
async fn main() {
    if let Err(e) = actual_main().await {
        eprintln!("bridge-server failed: {e:#}");
        std::process::exit(1);
    }
}

async fn actual_main() -> Result<()> {
    let args = conf::Args::parse();
    let conf = conf::Conf::new(args.config_file).context("Reading config")?;

    hyli_modules::utils::logger::setup_tracing(&conf.log_format, "bridge-server".to_string())
        .context("Setting up tracing")?;

    let server_port = args.server_port.unwrap_or(conf.server_port);

    info!("Starting Hyperlane Bridge Server");
    info!("  node_url     = {}", conf.node_url);
    info!("  server_port  = {}", server_port);
    info!("  chain_id     = {}", conf.hyli_chain_id);
    info!("  bridge_cn    = {}", conf.bridge_cn);
    info!("  hyperlane_cn = {}", conf.hyperlane_cn);
    info!("  data_dir     = {}", conf.data_directory);

    check_relayer(&conf.relayer_health_url).await;

    let node_client = Arc::new(
        NodeApiHttpClient::new(conf.node_url.clone()).context("Creating node HTTP client")?,
    );

    if !conf.noinit {
        init::init_contracts(&conf, node_client.clone()).await?;
    }

    let relayer_identity = parse_relayer_identity(&conf);

    let data_dir = std::path::PathBuf::from(&conf.data_directory);
    std::fs::create_dir_all(&data_dir).context("Creating data directory")?;
    let bus = SharedMessageBus::new();

    let mut handler =
        ModulesHandler::new(&bus, data_dir.clone(), ModulesHandlerOptions::default())?;

    let api_ctx = Arc::new(BuildApiContextInner {
        router: std::sync::Mutex::new(Some(Router::new())),
        openapi: Default::default(),
    });

    let evm_config_json = conf.evm_config_json.as_bytes().to_vec();

    handler
        .build_module::<RethModule>(RethModuleCtx {
            api: api_ctx.clone(),
            port: server_port,
            node_url: conf.node_url.clone(),
            hyli_chain_id: conf.hyli_chain_id,
            bridge_cn: ContractName(conf.bridge_cn.clone()),
            hyperlane_cn: ContractName(conf.hyperlane_cn.clone()),
            relayer_identity,
            evm_config_json,
        })
        .await?;

    // Listen to both the RISC0 bridge contract (for AutoProver) and the reth hyperlane
    // contract (for HylaneRpcProxyModule to track EVM state and submit reth proofs).
    let listened_contracts = HashSet::from([
        conf.bridge_cn.clone().into(),
        conf.hyperlane_cn.clone().into(),
    ]);

    handler
        .build_module::<ContractListener>(ContractListenerConf {
            database_url: conf.indexer_database_url.clone(),
            data_directory: data_dir.clone(),
            contracts: listened_contracts,
            poll_interval: Duration::from_secs(conf.contract_listener_poll_interval_secs),
            replay_settled_from_start: true,
        })
        .await?;

    handler
        .build_module::<AutoProver<BridgeTxHandler, Risc0Prover>>(Arc::new(AutoProverCtx {
            data_directory: data_dir.clone(),
            prover: Arc::new(Risc0Prover::new(
                hyperlane_bridge::client::tx_executor_handler::metadata::HYPERLANE_BRIDGE_ELF
                    .to_vec(),
                hyperlane_bridge::client::tx_executor_handler::metadata::HYPERLANE_BRIDGE_PROGRAM_ID,
            )),
            contract_name: conf.bridge_cn.clone().into(),
            node: node_client.clone(),
            api: Some(api_ctx.clone()),
            tx_buffer_size: conf.prover_tx_buffer_size,
            max_txs_per_proof: conf.prover_max_txs_per_proof,
            tx_working_window_size: conf.prover_tx_working_window_size,
            idle_flush_interval: Duration::from_secs(conf.prover_idle_flush_interval_secs),
        }))
        .await?;

    info!(
        "ContractListener and AutoProver modules started for '{}'",
        conf.bridge_cn
    );

    // Should come last so the other modules have nested their own routes.
    #[allow(clippy::expect_used, reason = "Fail on misconfiguration")]
    let router = api_ctx
        .router
        .lock()
        .expect("Context router should be available.")
        .take()
        .expect("Context router should be available.");
    #[allow(clippy::expect_used, reason = "Fail on misconfiguration")]
    let openapi = api_ctx
        .openapi
        .lock()
        .expect("OpenAPI should be available")
        .clone();

    handler
        .build_module::<RestApi>(RestApiRunContext::new(
            args.server_port.unwrap_or(conf.server_port),
            NodeInfo {
                id: "bridge-server".to_string(),
                pubkey: None,
                da_address: String::new(),
            },
            router,
            10 * 1024 * 1024,
            openapi,
        ))
        .await?;

    _ = handler.start_modules().await;
    handler.exit_process().await?;

    Ok(())
}

async fn check_relayer(url: &str) {
    info!("Checking Hyperlane relayer at {url}...");
    match reqwest::get(url).await {
        Ok(_) => info!("Hyperlane relayer is up"),
        Err(e) => tracing::warn!(
            "Hyperlane relayer not reachable at {url}: {e}\n\
            To start the relayer:\n\
            \t mkdir -p hyperlane_db && docker run --rm --network host \\\n\
            \t   -e CONFIG_FILES=/relayer-config.json \\\n\
            \t   --mount type=bind,source=$(pwd)/relayer-config.json,target=/relayer-config.json,readonly \\\n\
            \t   --mount type=bind,source=$(pwd)/hyperlane_db,target=/hyperlane_db \\\n\
            \t   ghcr.io/hyperlane-xyz/hyperlane-agent:agents-v2.1.0 \\\n\
            \t   ./relayer --db /hyperlane_db --defaultSigner.key <your-key>"
        ),
    }
}

fn parse_relayer_identity(conf: &conf::Conf) -> Identity {
    if let Some(key_hex) = &conf.relayer_key {
        let key_hex = key_hex.trim_start_matches("0x");
        if let Ok(key_bytes) = hex::decode(key_hex) {
            if let Ok(sk) = k256::ecdsa::SigningKey::from_slice(&key_bytes) {
                let vk = sk.verifying_key();
                let pk_bytes = vk.to_encoded_point(true);
                let pk_hex = hex::encode(pk_bytes.as_bytes());
                return Identity(format!("0x{pk_hex}@hyperlane-bridge"));
            }
        }
    }
    Identity("relayer@hyperlane-bridge".to_string())
}
