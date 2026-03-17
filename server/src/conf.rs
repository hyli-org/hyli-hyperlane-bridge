use anyhow::Result;
use clap::Parser;
use config::{Config, Environment, File, FileFormat};
use serde::Deserialize;

/// CLI arguments — only meta-arguments + overrides live here.
/// Everything else comes from the config file or environment variables.
#[derive(Parser, Debug)]
#[command(name = "bridge-server", about = "Hyperlane Bridge Server for Hyli")]
pub struct Args {
    /// Path(s) to a TOML config file (can be repeated; later files override earlier ones)
    #[arg(long, default_value = "bridge-server.toml")]
    pub config_file: Vec<String>,

    /// Server port (overrides config)
    /// Argument used by hylix tests commands
    #[arg(long)]
    pub server_port: Option<u16>,
}

/// Fully-resolved configuration for the bridge server.
#[derive(Debug, Deserialize, Clone)]
pub struct Conf {
    pub node_url: String,
    /// Server API port
    pub server_port: u16,
    /// Log format: "plain", "json", or "node"
    pub log_format: String,
    /// Domain ID shown to Hyperlane agents
    pub hyli_chain_id: u64,
    /// 32-byte hex state root of the Ethereum chain with Hyperlane contracts deployed
    pub eth_state_root: String,
    pub bridge_cn: String,
    pub hyperlane_cn: String,
    /// Hex-encoded secp256k1 private key used to sign Hyli transactions (relayer path)
    pub relayer_key: Option<String>,
    /// Directory name to store module state
    pub data_directory: String,
    /// Skip contract deployment on startup
    pub noinit: bool,

    // ── ContractListener / AutoProver ─────────────────────────────────────────
    /// PostgreSQL connection URL for the ContractListener (e.g. postgres://user:pass@host/db)
    pub indexer_database_url: String,
    /// How often (seconds) the ContractListener polls for missed events
    pub contract_listener_poll_interval_secs: u64,
    /// Minimum number of transactions to buffer before flushing a proof
    pub prover_tx_buffer_size: usize,
    /// Maximum number of transactions per proof batch
    pub prover_max_txs_per_proof: usize,
    /// Size of the working window for in-flight transactions
    pub prover_tx_working_window_size: usize,
    /// Flush buffered transactions if idle for this many seconds
    pub prover_idle_flush_interval_secs: u64,
}

impl Conf {
    pub fn new(config_files: Vec<String>) -> Result<Self> {
        let mut builder = Config::builder().add_source(File::from_str(
            include_str!("conf_defaults.toml"),
            FileFormat::Toml,
        ));

        for file in config_files {
            builder = builder.add_source(File::with_name(&file).required(false));
        }

        let conf: Self = builder
            .add_source(
                Environment::with_prefix("BRIDGE")
                    .separator("__")
                    .prefix_separator("_")
                    .try_parsing(true),
            )
            .build()?
            .try_deserialize()?;

        Ok(conf)
    }
}
