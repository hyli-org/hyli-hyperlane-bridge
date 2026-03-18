use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::{keccak256, Address};
use alloy_sol_types::SolCall;
use anyhow::Result;
use client_sdk::rest_client::{IndexerApiHttpClient, NodeApiClient, NodeApiHttpClient};
use sdk::{Blob, BlobData, BlobTransaction, ContractName, Identity};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

use crate::reth_module::eth_chain_state::EthChainState;
use crate::reth_module::types::JsonRpcResponse;
use hyperlane_bridge::{transferRemoteCall, HyperlaneBridgeAction};

#[derive(Clone)]
pub struct RouterCtx {
    pub hyli_chain_id: u64,
    pub bridge_cn: ContractName,
    pub hyperlane_cn: ContractName,
    pub relayer_identity: Identity,
    pub node_client: Arc<NodeApiHttpClient>,
    pub indexer_client: Arc<IndexerApiHttpClient>,
    /// Shared EVM chain state, updated by the module's ContractListener event handler.
    pub eth_chain_state: Arc<RwLock<EthChainState>>,
}

impl RouterCtx {
    pub fn new(
        node_url: String,
        hyli_chain_id: u64,
        bridge_cn: ContractName,
        hyperlane_cn: ContractName,
        relayer_identity: Identity,
        eth_chain_state: Arc<RwLock<EthChainState>>,
    ) -> Result<Self> {
        let node_client = Arc::new(NodeApiHttpClient::new(node_url.clone())?);
        let indexer_client = Arc::new(IndexerApiHttpClient::new(node_url.clone())?);
        Ok(Self {
            hyli_chain_id,
            bridge_cn,
            hyperlane_cn,
            relayer_identity,
            node_client,
            indexer_client,
            eth_chain_state,
        })
    }
}

// ── Dispatch event topic0 ─────────────────────────────────────────────────────
fn dispatch_topic0() -> [u8; 32] {
    *keccak256("Dispatch(address,uint32,bytes32,bytes)")
}

// ── Block helpers ─────────────────────────────────────────────────────────────

pub fn eth_block_number(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let block_number = ctx
        .eth_chain_state
        .read()
        .map(|s| s.block_number)
        .unwrap_or(0);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", block_number)))
}

pub fn eth_chain_id(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let chain_id = ctx
        .eth_chain_state
        .read()
        .map(|s| s.chain_id())
        .unwrap_or(0);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", chain_id)))
}

pub fn net_version(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let chain_id = ctx
        .eth_chain_state
        .read()
        .map(|s| s.chain_id())
        .unwrap_or(0);
    JsonRpcResponse::ok(id, json!(chain_id.to_string()))
}

pub fn eth_get_block_by_number(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let block_tag = params.get(0).and_then(|v| v.as_str()).unwrap_or("latest");

    let header = ctx
        .eth_chain_state
        .read()
        .ok()
        .and_then(|state| match block_tag {
            "latest" | "pending" => state.latest_header(),
            "earliest" => state.get_header_by_number(0),
            tag => {
                let n_str = tag.trim_start_matches("0x");
                u64::from_str_radix(n_str, 16)
                    .ok()
                    .and_then(|n| state.get_header_by_number(n))
            }
        });

    match header {
        Some(h) => JsonRpcResponse::ok(id, block_json_from_header(&h)),
        None => JsonRpcResponse::ok(id, Value::Null),
    }
}

fn block_json_from_header(h: &reth_primitives_traits::SealedHeader) -> Value {
    json!({
        "number": format!("0x{:x}", h.number()),
        "hash": format!("0x{}", hex::encode(h.hash())),
        "parentHash": format!("0x{}", hex::encode(h.parent_hash())),
        "stateRoot": format!("0x{}", hex::encode(h.state_root())),
        "timestamp": format!("0x{:x}", h.timestamp()),
        "gasLimit": format!("0x{:x}", h.gas_limit()),
        "gasUsed": format!("0x{:x}", h.gas_used()),
        "baseFeePerGas": h.base_fee_per_gas()
            .map(|f| format!("0x{:x}", f))
            .unwrap_or_else(|| "0x0".to_string()),
        "transactions": [],
        "logsBloom": "0x".to_string() + &"0".repeat(512),
        "miner": "0x0000000000000000000000000000000000000000",
        "difficulty": "0x0",
        "totalDifficulty": "0x0",
        "nonce": "0x0000000000000000",
        "sha3Uncles": "0x1dcc4de8dec75d7aab85b567b6ccd41ad312451b948a7413f0a142fd40d49347",
        "uncles": [],
        "size": "0x1",
        "receiptsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "transactionsRoot": "0x0000000000000000000000000000000000000000000000000000000000000000",
        "extraData": "0x",
    })
}

pub fn eth_get_balance(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let addr = parse_address_param(params, 0);
    let balance = ctx
        .eth_chain_state
        .read()
        .map(|s| s.account_balance(&addr))
        .unwrap_or_default();
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", balance)))
}

pub fn eth_get_transaction_count(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let addr = parse_address_param(params, 0);
    let nonce = ctx
        .eth_chain_state
        .read()
        .map(|s| s.account_nonce(&addr))
        .unwrap_or(0);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", nonce)))
}

pub fn eth_gas_price(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let gas_price = ctx
        .eth_chain_state
        .read()
        .map(|s| s.gas_price())
        .unwrap_or(1);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", gas_price)))
}

// ── eth_getLogs ───────────────────────────────────────────────────────────────

pub async fn eth_get_logs(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let filter = match params.get(0) {
        Some(f) => f.clone(),
        None => json!({}),
    };

    let from_block = parse_block_number(filter.get("fromBlock").and_then(|v| v.as_str()), 0);
    let to_block_tag = filter
        .get("toBlock")
        .and_then(|v| v.as_str())
        .unwrap_or("latest");

    let latest_height = match ctx.indexer_client.get_last_block().await {
        Ok(b) => b.height,
        Err(e) => {
            return JsonRpcResponse::internal_error(id, format!("Failed to get latest block: {e}"))
        }
    };

    let to_block = parse_block_number(Some(to_block_tag), latest_height);

    // Fetch all blob transactions for the hyperlane reth contract.
    // Each proven reth blob is one EVM transaction on the Hyli-hosted EVM chain.
    let txs = match ctx
        .indexer_client
        .get_blob_transactions_by_contract(&ctx.hyperlane_cn)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            return JsonRpcResponse::internal_error(
                id,
                format!("Failed to query hyperlane transactions: {e}"),
            )
        }
    };

    let mut logs = Vec::new();
    let topic0 = dispatch_topic0();

    for (log_index, tx) in txs.iter().enumerate() {
        let block_height = tx.block_height.0;
        if block_height < from_block || block_height > to_block {
            continue;
        }

        // Find a reth blob whose EVM transaction calls transferRemote.
        let transfer_remote_blob = tx.blobs.iter().find(|b| {
            b.contract_name == ctx.hyperlane_cn.0 && is_transfer_remote_reth_blob(&b.data)
        });

        let Some(reth_blob) = transfer_remote_blob else {
            continue;
        };

        // Parse transferRemote parameters directly from the reth blob.
        let Some((eth_recipient, amount, destination)) =
            parse_transfer_remote_from_reth_blob(&reth_blob.data)
        else {
            continue;
        };

        // Build Hyperlane message bytes.
        let origin = (ctx.hyli_chain_id & 0xFFFF_FFFF) as u32;
        let nonce = (log_index & 0xFFFF_FFFF) as u32;

        // Sender: bridge contract name as 32-byte right-padded string.
        let mut sender_bytes = [0u8; 32];
        let bridge_name = ctx.bridge_cn.0.as_bytes();
        let copy_len = bridge_name.len().min(32);
        sender_bytes[32 - copy_len..].copy_from_slice(&bridge_name[..copy_len]);

        let message_bytes = build_hyperlane_message(
            3, // version
            nonce,
            origin,
            sender_bytes,
            destination,
            eth_recipient,
            amount,
        );

        let mut sender_topic = [0u8; 32];
        sender_topic[12..].copy_from_slice(&sender_bytes[20..32]);

        let mut dest_topic = [0u8; 32];
        dest_topic[28..32].copy_from_slice(&destination.to_be_bytes());

        let msg_id = keccak256(&message_bytes);

        let log = json!({
            "address": "0x0000000000000000000000000000000000000001",
            "topics": [
                format!("0x{}", hex::encode(topic0)),
                format!("0x{}", hex::encode(sender_topic)),
                format!("0x{}", hex::encode(dest_topic)),
                format!("0x{}", hex::encode(eth_recipient)),
            ],
            "data": format!("0x{}", hex::encode(&message_bytes)),
            "blockNumber": format!("0x{:x}", block_height),
            "transactionHash": format!("0x{}", tx.tx_hash),
            "transactionIndex": format!("0x{:x}", tx.index),
            "blockHash": format!("0x{}", tx.block_hash),
            "logIndex": format!("0x{:x}", log_index),
            "removed": false,
            "messageId": format!("0x{}", hex::encode(*msg_id)),
        });

        logs.push(log);
    }

    JsonRpcResponse::ok(id, json!(logs))
}

/// Parse block number from a hex string tag like "0x1a3" or "latest"/"earliest".
fn parse_block_number(tag: Option<&str>, latest: u64) -> u64 {
    match tag {
        None | Some("latest") | Some("pending") => latest,
        Some("earliest") => 0,
        Some(s) => {
            let s = s.trim_start_matches("0x");
            u64::from_str_radix(s, 16).unwrap_or(0)
        }
    }
}

/// Check if a reth blob's EVM transaction calls `WarpRoute.transferRemote`.
fn is_transfer_remote_reth_blob(data: &[u8]) -> bool {
    let Some(tx) = decode_blob_as_tx(data) else {
        return false;
    };
    let input = extract_tx_input(&tx);
    input.len() >= 4 && input.starts_with(&transferRemoteCall::SELECTOR)
}

/// Extract `(eth_recipient, amount, destination_domain)` from a reth blob whose
/// EVM transaction calls `WarpRoute.transferRemote`.
fn parse_transfer_remote_from_reth_blob(data: &[u8]) -> Option<([u8; 32], u128, u32)> {
    let tx = decode_blob_as_tx(data)?;
    let input = extract_tx_input(&tx);
    if input.len() < 4 || !input.starts_with(&transferRemoteCall::SELECTOR) {
        return None;
    }
    let decoded = transferRemoteCall::abi_decode_raw(&input[4..]).ok()?;
    let amount: u128 = u128::try_from(decoded.amount).ok()?;
    let recipient: [u8; 32] = decoded.recipient.into();
    Some((recipient, amount, decoded.destination))
}

/// Decode a reth blob's raw bytes (stripping any StructuredBlobData wrapper) into a TxEnvelope.
pub fn decode_blob_as_tx(data: &[u8]) -> Option<TxEnvelope> {
    let raw_bytes = if let Ok(structured) =
        sdk::StructuredBlobData::<Vec<u8>>::try_from(sdk::BlobData(data.to_vec()))
    {
        structured.parameters
    } else {
        data.to_vec()
    };
    TxEnvelope::decode_2718(&mut raw_bytes.as_slice()).ok()
}

/// Build a Hyperlane message byte string.
fn build_hyperlane_message(
    version: u8,
    nonce: u32,
    origin: u32,
    sender: [u8; 32],
    destination: u32,
    recipient: [u8; 32],
    amount: u128,
) -> Vec<u8> {
    let mut msg = Vec::with_capacity(77 + 64);
    msg.push(version);
    msg.extend_from_slice(&nonce.to_be_bytes());
    msg.extend_from_slice(&origin.to_be_bytes());
    msg.extend_from_slice(&sender);
    msg.extend_from_slice(&destination.to_be_bytes());
    msg.extend_from_slice(&recipient);
    // Body: token recipient (32 bytes) + amount (32 bytes, big-endian)
    msg.extend_from_slice(&recipient);
    let mut amount_bytes = [0u8; 32];
    amount_bytes[16..].copy_from_slice(&amount.to_be_bytes());
    msg.extend_from_slice(&amount_bytes);
    msg
}

// ── eth_call ──────────────────────────────────────────────────────────────────

pub fn eth_call(_ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let data = params
        .get(0)
        .and_then(|obj| obj.get("data"))
        .and_then(|v| v.as_str())
        .unwrap_or("0x");

    let data_bytes = hex::decode(data.trim_start_matches("0x")).unwrap_or_default();

    if data_bytes.len() < 4 {
        return JsonRpcResponse::ok(id, json!("0x"));
    }

    let selector = &data_bytes[..4];

    // latestCheckpoint() -> (bytes32, uint32) stub
    let latest_checkpoint_sel = &keccak256("latestCheckpoint()")[..4];
    if selector == latest_checkpoint_sel {
        return JsonRpcResponse::ok(id, json!(format!("0x{}", "0".repeat(128))));
    }

    // threshold() -> uint8
    let threshold_sel = &keccak256("threshold()")[..4];
    if selector == threshold_sel {
        return JsonRpcResponse::ok(
            id,
            json!("0x0000000000000000000000000000000000000000000000000000000000000001"),
        );
    }

    JsonRpcResponse::ok(id, json!("0x"))
}

// ── eth_sendRawTransaction ────────────────────────────────────────────────────

pub async fn eth_send_raw_transaction(
    ctx: &RouterCtx,
    id: Value,
    params: &Value,
) -> JsonRpcResponse {
    let raw_hex = if let Some(s) = params.get(0).and_then(|v| v.as_str()) {
        s.trim_start_matches("0x").to_string()
    } else if let Some(data) = params
        .get(0)
        .and_then(|obj| obj.get("data"))
        .and_then(|v| v.as_str())
    {
        data.trim_start_matches("0x").to_string()
    } else {
        return JsonRpcResponse::err(id, -32602, "Missing transaction data");
    };

    let raw_bytes = match hex::decode(&raw_hex) {
        Ok(b) => b,
        Err(e) => return JsonRpcResponse::err(id, -32602, format!("Invalid hex: {e}")),
    };

    // Validate the bytes decode as a valid EIP-2718 transaction.
    if TxEnvelope::decode_2718(&mut raw_bytes.as_slice()).is_err() {
        return JsonRpcResponse::err(id, -32602, "Failed to decode EIP-2718 transaction");
    }

    // Build the Hyli blob transaction:
    //   blob[0]: hyperlane reth blob  ← raw EIP-2718 bytes (proven by reth verifier)
    //   blob[1]: hyperlane-bridge blob ← VerifyTransaction (proven by RISC0)
    let blobs = vec![
        Blob {
            contract_name: ctx.hyperlane_cn.clone(),
            data: BlobData(raw_bytes),
        },
        Blob {
            contract_name: ctx.bridge_cn.clone(),
            data: BlobData::from(sdk::StructuredBlobData {
                caller: None,
                callees: None,
                parameters: HyperlaneBridgeAction::VerifyTransaction,
            }),
        },
    ];

    let blob_tx = BlobTransaction::new(ctx.relayer_identity.clone(), blobs);
    let tx_hash = match ctx.node_client.send_tx_blob(blob_tx).await {
        Ok(h) => h,
        Err(e) => return JsonRpcResponse::internal_error(id, format!("Failed to send tx: {e}")),
    };

    let hash_str = tx_hash.to_string();
    let padded = if hash_str.len() < 64 {
        format!("{:0>64}", hash_str)
    } else {
        hash_str[..64].to_string()
    };
    JsonRpcResponse::ok(id, json!(format!("0x{}", padded)))
}

// ── eth_getTransactionReceipt ─────────────────────────────────────────────────

pub async fn eth_get_transaction_receipt(
    ctx: &RouterCtx,
    id: Value,
    params: &Value,
) -> JsonRpcResponse {
    let hash_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h.trim_start_matches("0x"),
        None => return JsonRpcResponse::ok(id, Value::Null),
    };

    let tx_hash = match hex::decode(hash_hex).map(sdk::TxHash) {
        Ok(h) => h,
        Err(_) => return JsonRpcResponse::ok(id, Value::Null),
    };

    match ctx.indexer_client.get_transaction_with_hash(&tx_hash).await {
        Ok(tx) => {
            let status = match tx.transaction_status {
                sdk::api::TransactionStatusDb::Success => "0x1",
                sdk::api::TransactionStatusDb::Failure => "0x0",
                _ => {
                    return JsonRpcResponse::ok(id, Value::Null);
                }
            };

            let block_number = tx
                .block_height
                .map(|h| format!("0x{:x}", h.0))
                .unwrap_or_default();
            let block_hash = tx
                .block_hash
                .as_ref()
                .map(|h| format!("0x{h}"))
                .unwrap_or_else(|| "0x".to_string());

            JsonRpcResponse::ok(
                id,
                json!({
                    "transactionHash": format!("0x{hash_hex}"),
                    "transactionIndex": "0x0",
                    "blockHash": block_hash,
                    "blockNumber": block_number,
                    "from": "0x0000000000000000000000000000000000000000",
                    "to": "0x0000000000000000000000000000000000000001",
                    "cumulativeGasUsed": "0x0",
                    "gasUsed": "0x0",
                    "logs": [],
                    "logsBloom": "0x".to_string() + &"0".repeat(512),
                    "status": status,
                    "type": "0x2",
                }),
            )
        }
        Err(_) => JsonRpcResponse::ok(id, Value::Null),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn extract_tx_input(tx: &TxEnvelope) -> &[u8] {
    match tx {
        TxEnvelope::Legacy(t) => t.tx().input.as_ref(),
        TxEnvelope::Eip2930(t) => t.tx().input.as_ref(),
        TxEnvelope::Eip1559(t) => t.tx().input.as_ref(),
        TxEnvelope::Eip7702(t) => t.tx().input.as_ref(),
        _ => &[],
    }
}

/// Parse an Ethereum address from `params[index]`, returning `Address::ZERO` on failure.
fn parse_address_param(params: &Value, index: usize) -> Address {
    params
        .get(index)
        .and_then(|v| v.as_str())
        .and_then(|s| {
            hex::decode(s.trim_start_matches("0x"))
                .ok()
                .filter(|b| b.len() == 20)
                .map(|b| Address::from_slice(&b))
        })
        .unwrap_or(Address::ZERO)
}
