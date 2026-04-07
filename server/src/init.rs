use alloy_primitives::keccak256;
use anyhow::{Context, Result};
use client_sdk::rest_client::{NodeApiClient, NodeApiHttpClient};
use hyperlane_bridge::client::tx_executor_handler::metadata;
use hyperlane_bridge::HyperlaneBridgeState;
use k256::ecdsa::SigningKey;
use sdk::{
    api::APIRegisterContract, ContractName, ProgramId, StateCommitment, Verifier, ZkContract,
};
use std::sync::Arc;
use std::time::Duration;
use tracing::info;

use crate::conf::Conf;
use crate::reth_module::eth_chain_state::genesis_state_root;

/// Derives a 65-byte uncompressed secp256k1 public key from a contract name.
/// Matches the `derive_program_pubkey` logic in the reth-verifier.
fn derive_program_pubkey(contract_name: &ContractName) -> ProgramId {
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

pub async fn init_contracts(conf: &Conf, node: Arc<NodeApiHttpClient>) -> Result<()> {
    let bridge_cn = ContractName(conf.bridge_cn.clone());
    let hyperlane_cn = ContractName(conf.hyperlane_cn.clone());

    // ── Contract A: HyperlaneBridgeState (risc0) ──────────────────────────────
    let bridge_state = HyperlaneBridgeState {
        hyperlane_contract: hyperlane_cn.clone(),
    };

    match node.get_contract(bridge_cn.clone()).await {
        Ok(_) => info!(
            "Contract '{}' already exists, skipping registration",
            bridge_cn
        ),
        Err(_) => {
            info!("Registering contract '{}'...", bridge_cn);
            node.register_contract(APIRegisterContract {
                verifier: Verifier("risc0-3".to_string()),
                program_id: ProgramId(metadata::HYPERLANE_BRIDGE_PROGRAM_ID.to_vec()),
                state_commitment: bridge_state.commit(),
                contract_name: bridge_cn.clone(),
                timeout_window: Some((50, 50)),
                constructor_metadata: None,
            })
            .await
            .context("Registering hyperlane-bridge contract")?;

            wait_for_contract(node.clone(), &bridge_cn).await?;
            info!("Contract '{}' registered successfully", bridge_cn);
        }
    }

    // ── Contract B: hyperlane (reth verifier) ─────────────────────────────────
    let eth_state_root = genesis_state_root(conf.evm_config_json.as_bytes())
        .context("Computing genesis state root from evm_config_json")?
        .to_vec();

    match node.get_contract(hyperlane_cn.clone()).await {
        Ok(_) => info!(
            "Contract '{}' already exists, skipping registration",
            hyperlane_cn
        ),
        Err(_) => {
            info!("Registering contract '{}'...", hyperlane_cn);

            // Hyperlane program_id is derived from the hyperlane-bridge contract name,
            // matching the reth-verifier's derive_program_pubkey logic.
            let program_id = derive_program_pubkey(&bridge_cn);

            node.register_contract(APIRegisterContract {
                verifier: Verifier(hyli_model::verifiers::RETH.to_string()),
                program_id,
                state_commitment: StateCommitment(eth_state_root),
                contract_name: hyperlane_cn.clone(),
                timeout_window: Some((5, 5)),
                constructor_metadata: None,
            })
            .await
            .context("Registering hyperlane contract")?;

            wait_for_contract(node.clone(), &hyperlane_cn).await?;
            info!("Contract '{}' registered successfully", hyperlane_cn);
        }
    }

    Ok(())
}

async fn wait_for_contract(node: Arc<NodeApiHttpClient>, name: &ContractName) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if node.get_contract(name.clone()).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .with_context(|| format!("Timeout waiting for contract '{name}'"))
}
