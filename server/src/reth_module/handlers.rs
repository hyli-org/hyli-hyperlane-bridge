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
use tracing::info;

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
    // Always report one block ahead of the last settled block so that callers
    // waiting for N confirmations (e.g. ethers.js transaction.wait(1)) see
    // current_block >= receipt_block + 1 and proceed without a new tx needed.
    let reported = block_number + 1;
    info!(block_number, reported, "eth_blockNumber");
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", reported)))
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

pub fn eth_get_code(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let addr = parse_address_param(params, 0);
    let code_hex = ctx
        .eth_chain_state
        .read()
        .map(|s| {
            s.accounts
                .get(&addr)
                .map(|a| format!("0x{}", hex::encode(&a.code)))
                .unwrap_or_else(|| "0x".to_string())
        })
        .unwrap_or_else(|_| "0x".to_string());
    JsonRpcResponse::ok(id, json!(code_hex))
}

pub fn eth_estimate_gas(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let call = params.get(0).cloned().unwrap_or(json!({}));

    let from = call
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            hex::decode(s.trim_start_matches("0x"))
                .ok()
                .filter(|b| b.len() == 20)
                .map(|b| alloy_primitives::Address::from_slice(&b))
        })
        .unwrap_or(alloy_primitives::Address::ZERO);

    let to = call
        .get("to")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty() && *s != "null" && *s != "0x")
        .and_then(|s| {
            hex::decode(s.trim_start_matches("0x"))
                .ok()
                .filter(|b| b.len() == 20)
                .map(|b| alloy_primitives::Address::from_slice(&b))
        });

    let data = call
        .get("data")
        .or_else(|| call.get("input"))
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
        .map(alloy_primitives::Bytes::from)
        .unwrap_or_default();

    let value = call
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(|s| u128::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .map(alloy_primitives::U256::from)
        .unwrap_or_default();

    let gas = ctx
        .eth_chain_state
        .read()
        .map(|s| s.estimate_gas(from, to, data, value))
        .unwrap_or(5_000_000);

    info!(gas, "eth_estimateGas");
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", gas)))
}

pub fn eth_get_storage_at(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let addr = parse_address_param(params, 0);
    let slot_hex = params
        .get(1)
        .and_then(|v| v.as_str())
        .unwrap_or("0x0")
        .trim_start_matches("0x");
    let slot_bytes = hex::decode(slot_hex).unwrap_or_default();
    let mut slot_arr = [0u8; 32];
    let copy_start = 32usize.saturating_sub(slot_bytes.len());
    slot_arr[copy_start..].copy_from_slice(&slot_bytes[slot_bytes.len().saturating_sub(32)..]);
    let slot = alloy_primitives::U256::from_be_bytes(slot_arr);

    let value = ctx
        .eth_chain_state
        .read()
        .map(|s| {
            s.accounts
                .get(&addr)
                .and_then(|a| a.storage.get(&slot).copied())
                .unwrap_or_default()
        })
        .unwrap_or_default();

    let value_bytes: [u8; 32] = value.to_be_bytes();
    JsonRpcResponse::ok(id, json!(format!("0x{}", hex::encode(value_bytes))))
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

pub fn eth_call(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    use alloy_primitives::{Bytes, U256};

    let call_obj = match params.get(0) {
        Some(v) => v,
        None => return JsonRpcResponse::err(id, -32602, "Missing call object"),
    };

    let from: alloy_primitives::Address = call_obj
        .get("from")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(alloy_primitives::Address::ZERO);

    let to: Option<alloy_primitives::Address> = call_obj
        .get("to")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse().ok());

    let data: Bytes = call_obj
        .get("data")
        .or_else(|| call_obj.get("input"))
        .and_then(|v| v.as_str())
        .and_then(|s| hex::decode(s.trim_start_matches("0x")).ok())
        .map(Bytes::from)
        .unwrap_or_default();

    let value: U256 = call_obj
        .get("value")
        .and_then(|v| v.as_str())
        .and_then(|s| {
            let s = s.trim_start_matches("0x");
            u128::from_str_radix(s, 16).ok()
        })
        .map(U256::from)
        .unwrap_or(U256::ZERO);

    let state = match ctx.eth_chain_state.read() {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };

    let (success, output) = state.execute_call(from, to, data, value);
    let output_hex = format!("0x{}", hex::encode(&output));

    info!(
        "eth_call to={} success={} output_len={}",
        to.map(|a| format!("{a:?}")).unwrap_or_default(),
        success,
        output.len()
    );

    if success {
        JsonRpcResponse::ok(id, json!(output_hex))
    } else {
        // Return execution revert error (code 3) with revert data
        JsonRpcResponse::err(id, 3, format!("execution reverted: {output_hex}"))
    }
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

    // Compute the EVM tx hash before raw_bytes is moved into the blob.
    // ethers.js v5 verifies the returned hash == keccak256(raw_eip2718_bytes).
    let evm_tx_hash = alloy_primitives::keccak256(&raw_bytes);

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
    if let Err(e) = ctx.node_client.send_tx_blob(blob_tx).await {
        return JsonRpcResponse::internal_error(id, format!("Failed to send tx: {e}"));
    }

    JsonRpcResponse::ok(id, json!(format!("0x{}", hex::encode(*evm_tx_hash))))
}

// ── eth_getTransactionByHash ──────────────────────────────────────────────────

pub fn eth_get_transaction_by_hash(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let hash_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(h) => h.trim_start_matches("0x"),
        None => return JsonRpcResponse::ok(id, Value::Null),
    };

    let evm_hash_bytes: Option<[u8; 32]> =
        hex::decode(hash_hex).ok().and_then(|b| b.try_into().ok());

    if let Some(hash_bytes) = evm_hash_bytes {
        let receipt = ctx
            .eth_chain_state
            .read()
            .map(|s| s.settled_receipts.get(&hash_bytes).cloned())
            .unwrap_or(None);

        if let Some(r) = receipt {
            return JsonRpcResponse::ok(
                id,
                json!({
                    "hash": format!("0x{hash_hex}"),
                    "blockHash": format!("0x{}", hex::encode(r.block_hash)),
                    "blockNumber": format!("0x{:x}", r.block_number),
                    "transactionIndex": "0x0",
                    "from": "0x0000000000000000000000000000000000000000",
                    "to": null,
                    "value": "0x0",
                    "gas": "0x0",
                    "gasPrice": "0x0",
                    "input": "0x",
                    "nonce": "0x0",
                    "type": "0x2",
                }),
            );
        }
    }

    JsonRpcResponse::ok(id, Value::Null)
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

    // Check in-memory settled receipts first (indexed by EVM tx hash = keccak256(raw_eip2718)).
    // The Hyli indexer stores transactions under the Hyli blob tx hash, not the EVM tx hash,
    // so querying it directly would always return 404.
    let evm_hash_bytes: Option<[u8; 32]> =
        hex::decode(hash_hex).ok().and_then(|b| b.try_into().ok());

    if let Some(hash_bytes) = evm_hash_bytes {
        let (receipt, settled_count) = ctx
            .eth_chain_state
            .read()
            .map(|s| {
                (
                    s.settled_receipts.get(&hash_bytes).cloned(),
                    s.settled_receipts.len(),
                )
            })
            .unwrap_or((None, 0));

        info!(
            tx = hash_hex,
            settled_receipts = settled_count,
            found = receipt.is_some(),
            "eth_getTransactionReceipt"
        );

        if let Some(r) = receipt {
            info!(
                tx = hash_hex,
                block_number = r.block_number,
                success = r.success,
                "→ returning receipt"
            );
            return JsonRpcResponse::ok(
                id,
                json!({
                    "transactionHash": format!("0x{hash_hex}"),
                    "transactionIndex": "0x0",
                    "blockHash": format!("0x{}", hex::encode(r.block_hash)),
                    "blockNumber": format!("0x{:x}", r.block_number),
                    "from": "0x0000000000000000000000000000000000000000",
                    "to": "0x0000000000000000000000000000000000000001",
                    "cumulativeGasUsed": "0x0",
                    "gasUsed": "0x0",
                    "logs": [],
                    "logsBloom": "0x".to_string() + &"0".repeat(512),
                    "status": if r.success { "0x1" } else { "0x0" },
                    "type": "0x2",
                }),
            );
        }
    }

    JsonRpcResponse::ok(id, Value::Null)
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
