use alloy_consensus::BlockHeader as AlloyBlockHeader;
use alloy_consensus::Transaction as _;
use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::Address;
use anyhow::Result;
use client_sdk::rest_client::{IndexerApiHttpClient, NodeApiClient, NodeApiHttpClient};
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use sdk::{Blob, BlobData, BlobTransaction, ContractName, Identity};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use tracing::info;

use crate::reth_module::eth_chain_state::{EthChainState, EthTxReceipt};
use crate::reth_module::types::JsonRpcResponse;
use hyperlane_bridge::HyperlaneBridgeAction;

#[derive(Clone, Debug)]
pub struct SubmittedTx {
    pub raw_eip2718: Vec<u8>,
    pub hyli_tx_hash: String,
}

#[derive(Clone)]
pub struct RouterCtx {
    pub hyli_chain_id: u64,
    pub bridge_cn: ContractName,
    pub hyperlane_cn: ContractName,
    pub relayer_identity: Identity,
    pub node_client: Arc<NodeApiHttpClient>,
    pub indexer_client: Arc<IndexerApiHttpClient>,
    /// Settled EVM chain state, updated once Hyli confirms a transaction.
    pub settled_eth_chain_state: Arc<RwLock<EthChainState>>,
    /// Published pending/speculative EVM snapshot exposed via RPC.
    pub pending_eth_chain_state: Arc<RwLock<EthChainState>>,
    /// Locally submitted EVM transactions keyed by EVM tx hash.
    pub submitted_txs: Arc<RwLock<HashMap<[u8; 32], SubmittedTx>>>,
    /// Set to true once backfilling is complete; RPC requests are rejected until then.
    pub is_ready: Arc<AtomicBool>,
}

impl RouterCtx {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node_url: String,
        hyli_chain_id: u64,
        bridge_cn: ContractName,
        hyperlane_cn: ContractName,
        relayer_identity: Identity,
        settled_eth_chain_state: Arc<RwLock<EthChainState>>,
        pending_eth_chain_state: Arc<RwLock<EthChainState>>,
        submitted_txs: Arc<RwLock<HashMap<[u8; 32], SubmittedTx>>>,
        is_ready: Arc<AtomicBool>,
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
            settled_eth_chain_state,
            pending_eth_chain_state,
            submitted_txs,
            is_ready,
        })
    }
}

// ── Block helpers ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockView {
    Latest,
    Pending,
    Earliest,
    Number(u64),
}

fn parse_block_view(tag: Option<&str>) -> BlockView {
    match tag {
        Some("pending") => BlockView::Pending,
        None | Some("latest") => BlockView::Latest,
        Some("earliest") => BlockView::Earliest,
        Some(s) => u64::from_str_radix(s.trim_start_matches("0x"), 16)
            .map(BlockView::Number)
            .unwrap_or(BlockView::Latest),
    }
}

fn current_pending_state(ctx: &RouterCtx) -> Result<EthChainState, JsonRpcResponse> {
    match ctx.pending_eth_chain_state.read() {
        Ok(state) => Ok(state.clone()),
        Err(_) => Err(JsonRpcResponse::err(
            json!(null),
            -32603,
            "State lock poisoned",
        )),
    }
}

fn state_for_view(ctx: &RouterCtx, view: BlockView) -> Result<EthChainState, JsonRpcResponse> {
    match view {
        BlockView::Pending => current_pending_state(ctx),
        BlockView::Latest | BlockView::Earliest | BlockView::Number(_) => ctx
            .settled_eth_chain_state
            .read()
            .map(|s| s.clone())
            .map_err(|_| JsonRpcResponse::err(json!(null), -32603, "State lock poisoned")),
    }
}

fn tx_type_hex(raw_eip2718: &[u8]) -> String {
    match raw_eip2718.first().copied() {
        Some(0x01..=0x04) => format!("0x{:x}", raw_eip2718[0]),
        _ => "0x0".to_string(),
    }
}

fn encode_tx_json(
    raw_eip2718: &[u8],
    tx_hash: &[u8; 32],
    receipt: Option<&EthTxReceipt>,
) -> Option<Value> {
    let tx = TransactionSigned::decode_2718(&mut &*raw_eip2718).ok()?;
    let from = tx.recover_signer().ok()?;

    // Get v/r/s via TxEnvelope which exposes signature() directly.
    // v is yParity (0 or 1) for EIP-1559; ethers-rs Transaction struct requires these fields.
    let envelope = TxEnvelope::decode_2718(&mut &*raw_eip2718).ok()?;
    let sig = envelope.signature();
    let v_val: u64 = sig.v() as u64;
    let r_bytes = sig.r().to_be_bytes::<32>();
    let s_bytes = sig.s().to_be_bytes::<32>();

    Some(json!({
        "hash": format!("0x{}", hex::encode(tx_hash)),
        "blockHash": receipt.map(|r| format!("0x{}", hex::encode(r.block_hash))),
        "blockNumber": receipt.map(|r| format!("0x{:x}", r.block_number)),
        "transactionIndex": receipt.map(|_| "0x0"),
        "from": format!("{from:?}"),
        "to": tx.to().map(|addr| format!("{addr:?}")),
        "value": format!("{:#x}", tx.value()),
        "gas": format!("0x{:x}", tx.gas_limit()),
        "gasPrice": tx.gas_price().map(|v| format!("0x{:x}", v)),
        "maxFeePerGas": format!("0x{:x}", tx.max_fee_per_gas()),
        "maxPriorityFeePerGas": tx.max_priority_fee_per_gas().map(|v| format!("0x{:x}", v)),
        "input": format!("0x{}", hex::encode(tx.input())),
        "nonce": format!("0x{:x}", tx.nonce()),
        "chainId": tx.chain_id().map(|v| format!("0x{:x}", v)),
        "type": tx_type_hex(raw_eip2718),
        "v": format!("0x{:x}", v_val),
        "r": format!("0x{}", hex::encode(r_bytes)),
        "s": format!("0x{}", hex::encode(s_bytes)),
    }))
}

pub fn eth_block_number(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let block_number = ctx
        .settled_eth_chain_state
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
    // Fall back to the configured chain ID so we never return "0x0" (ethers.js
    // treats chainId=0 as "no network" and throws NETWORK_ERROR).
    let chain_id = ctx
        .settled_eth_chain_state
        .read()
        .map(|s| s.chain_id())
        .unwrap_or(ctx.hyli_chain_id);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", chain_id)))
}

pub fn net_version(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let chain_id = ctx
        .settled_eth_chain_state
        .read()
        .map(|s| s.chain_id())
        .unwrap_or(ctx.hyli_chain_id);
    JsonRpcResponse::ok(id, json!(chain_id.to_string()))
}

pub fn eth_get_block_by_number(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let block_view = parse_block_view(params.get(0).and_then(|v| v.as_str()));

    let state = match state_for_view(ctx, block_view) {
        Ok(state) => state,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };
    // For a specific block number request, track whether we need to override the
    // reported number (the synthetic block_number+1 case uses the real header but
    // must advertise the requested number so ethers-rs doesn't see a fake reorg).
    let number_override = match block_view {
        BlockView::Number(n)
            if state.get_header_by_number(n).is_none() && n == state.block_number + 1 =>
        {
            Some(n)
        }
        _ => None,
    };

    let header = match block_view {
        BlockView::Latest | BlockView::Pending => state.latest_header(),
        BlockView::Earliest => state.get_header_by_number(0),
        BlockView::Number(n) => state.get_header_by_number(n).or_else(|| {
            if n == state.block_number + 1 {
                state.latest_header()
            } else {
                None
            }
        }),
    };

    match header {
        Some(h) => {
            let mut block = block_json_from_header(&h);
            if let Some(n) = number_override {
                block["number"] = json!(format!("0x{:x}", n));
            }
            JsonRpcResponse::ok(id, block)
        }
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
    let block_view = parse_block_view(params.get(1).and_then(|v| v.as_str()));
    let code_hex = state_for_view(ctx, block_view)
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

    let block_view = parse_block_view(params.get(1).and_then(|v| v.as_str()));
    let gas = state_for_view(ctx, block_view)
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

    let block_view = parse_block_view(params.get(2).and_then(|v| v.as_str()));
    let value = state_for_view(ctx, block_view)
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
    let block_view = parse_block_view(params.get(1).and_then(|v| v.as_str()));
    let balance = state_for_view(ctx, block_view)
        .map(|s| s.account_balance(&addr))
        .unwrap_or_default();
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", balance)))
}

pub fn eth_get_transaction_count(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let addr = parse_address_param(params, 0);
    let block_view = parse_block_view(params.get(1).and_then(|v| v.as_str()));
    let state_nonce = state_for_view(ctx, block_view)
        .map(|s| s.account_nonce(&addr))
        .unwrap_or(0);
    let submitted_nonce = if block_view == BlockView::Pending {
        ctx.submitted_txs
            .read()
            .ok()
            .map(|txs| {
                txs.iter()
                    .filter_map(|(hash, submitted)| {
                        let settled = ctx
                            .settled_eth_chain_state
                            .read()
                            .ok()
                            .map(|s| s.settled_receipts.contains_key(hash))
                            .unwrap_or(false);
                        if settled {
                            return None;
                        }
                        let tx =
                            TransactionSigned::decode_2718(&mut submitted.raw_eip2718.as_slice())
                                .ok()?;
                        let signer = tx.recover_signer().ok()?;
                        (signer == addr).then_some(tx.nonce() + 1)
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    } else {
        0
    };
    let nonce = state_nonce.max(submitted_nonce);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", nonce)))
}

pub fn eth_gas_price(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let gas_price = ctx
        .settled_eth_chain_state
        .read()
        .map(|s| s.gas_price())
        .unwrap_or(1);
    JsonRpcResponse::ok(id, json!(format!("0x{:x}", gas_price)))
}

pub fn eth_max_priority_fee_per_gas(_ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    // This chain uses zero priority fees (no mempool competition).
    JsonRpcResponse::ok(id, json!("0x0"))
}

pub fn eth_fee_history(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    // eth_feeHistory(blockCount, newestBlock, rewardPercentiles)
    // ethers.js v5 uses this for EIP-1559 fee estimation.
    let block_count = params
        .get(0)
        .and_then(|v| v.as_str())
        .and_then(|s| u64::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .unwrap_or(1)
        .max(1);

    let base_fee = ctx
        .settled_eth_chain_state
        .read()
        .map(|s| s.gas_price())
        .unwrap_or(1_000_000_000);

    let base_fee_hex = format!("0x{:x}", base_fee);
    let base_fees: Vec<String> = (0..=block_count).map(|_| base_fee_hex.clone()).collect();
    let gas_used_ratios: Vec<f64> = (0..block_count).map(|_| 0.5).collect();

    let oldest_block = ctx
        .settled_eth_chain_state
        .read()
        .map(|s| s.block_number.saturating_sub(block_count))
        .unwrap_or(0);

    JsonRpcResponse::ok(
        id,
        json!({
            "oldestBlock": format!("0x{:x}", oldest_block),
            "baseFeePerGas": base_fees,
            "gasUsedRatio": gas_used_ratios,
            "reward": []
        }),
    )
}

// ── eth_getLogs ───────────────────────────────────────────────────────────────

pub fn eth_get_logs(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let filter = match params.get(0) {
        Some(f) => f.clone(),
        None => json!({}),
    };

    let state = match state_for_view(
        ctx,
        parse_block_view(filter.get("toBlock").and_then(|v| v.as_str())),
    ) {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };

    let latest_block = state.block_number + 1;
    let from_block = parse_block_number(filter.get("fromBlock").and_then(|v| v.as_str()), 0);
    let to_block = parse_block_number(filter.get("toBlock").and_then(|v| v.as_str()), latest_block);

    // Optional address filter (single address or array).
    let address_filter: Option<Vec<Address>> = if let Some(addr_val) = filter.get("address") {
        if let Some(s) = addr_val.as_str() {
            s.parse::<Address>().ok().map(|a| vec![a])
        } else if let Some(arr) = addr_val.as_array() {
            let addrs: Vec<Address> = arr
                .iter()
                .filter_map(|v| v.as_str().and_then(|s| s.parse().ok()))
                .collect();
            if addrs.is_empty() {
                None
            } else {
                Some(addrs)
            }
        } else {
            None
        }
    } else {
        None
    };

    // Optional topics filter: array of (topic | null | array-of-topics).
    let topics_filter: Vec<Option<Vec<[u8; 32]>>> = filter
        .get("topics")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|pos| {
                    if pos.is_null() {
                        None
                    } else if let Some(s) = pos.as_str() {
                        let h = hex::decode(s.trim_start_matches("0x")).ok()?;
                        let mut b = [0u8; 32];
                        let len = h.len().min(32);
                        b[32 - len..].copy_from_slice(&h[h.len() - len..]);
                        Some(vec![b])
                    } else if let Some(options) = pos.as_array() {
                        let v: Vec<[u8; 32]> = options
                            .iter()
                            .filter_map(|o| {
                                let s = o.as_str()?;
                                let h = hex::decode(s.trim_start_matches("0x")).ok()?;
                                let mut b = [0u8; 32];
                                let len = h.len().min(32);
                                b[32 - len..].copy_from_slice(&h[h.len() - len..]);
                                Some(b)
                            })
                            .collect();
                        if v.is_empty() {
                            None
                        } else {
                            Some(v)
                        }
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default();

    // Collect and sort receipts by block number for deterministic log ordering.
    // Use reported block numbers (actual + 1) to stay consistent with eth_blockNumber,
    // which adds 1 so the relayer always sees a "new" block after each settled tx.
    let mut receipts: Vec<(&[u8; 32], _)> = state
        .settled_receipts
        .iter()
        .filter(|(_, r)| r.block_number + 1 >= from_block && r.block_number < to_block)
        .collect();
    receipts.sort_by_key(|(_, r)| r.block_number);

    let mut result = Vec::new();
    let mut global_log_index: usize = 0;

    for (tx_hash, receipt) in receipts {
        for log in receipt.logs.iter() {
            // Address filter.
            if let Some(ref addrs) = address_filter {
                if !addrs.contains(&log.address) {
                    global_log_index += 1;
                    continue;
                }
            }

            // Topics filter: each position must match.
            let mut topic_match = true;
            for (pos, filter_pos) in topics_filter.iter().enumerate() {
                if let Some(allowed) = filter_pos {
                    let log_topic = log.topics().get(pos).map(|t| t.0);
                    match log_topic {
                        Some(t) if allowed.contains(&t) => {}
                        _ => {
                            topic_match = false;
                            break;
                        }
                    }
                }
            }
            if !topic_match {
                global_log_index += 1;
                continue;
            }

            let topics_json: Vec<String> = log
                .topics()
                .iter()
                .map(|t| format!("0x{}", hex::encode(t.0)))
                .collect();

            result.push(json!({
                "address": format!("{:?}", log.address),
                "topics": topics_json,
                "data": format!("0x{}", hex::encode(log.data.data.as_ref())),
                "blockNumber": format!("0x{:x}", receipt.block_number + 1),
                "transactionHash": format!("0x{}", hex::encode(tx_hash)),
                "blockHash": format!("0x{}", hex::encode(receipt.block_hash.0)),
                // Each block contains exactly one transaction (one tx per apply_transaction call).
                "transactionIndex": "0x0",
                "logIndex": format!("0x{:x}", global_log_index),
                "removed": false,
            }));

            global_log_index += 1;
        }
    }

    JsonRpcResponse::ok(id, json!(result))
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

// ── hyli_getTxByMessageId ─────────────────────────────────────────────────────

/// Custom method: look up the Hyli blob tx hash for a given Hyperlane `messageId`.
/// Returns `{"hyliTxHash": "<hex>"}` or `null` if not yet indexed.
pub fn hyli_get_tx_by_message_id(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let message_id_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(s) => s.trim_start_matches("0x"),
        None => return JsonRpcResponse::err(id, -32602, "Missing messageId param"),
    };

    let message_id_bytes: [u8; 32] = match hex::decode(message_id_hex)
        .ok()
        .and_then(|b| b.try_into().ok())
    {
        Some(b) => b,
        None => {
            return JsonRpcResponse::err(id, -32602, "Invalid messageId (expected 32-byte hex)")
        }
    };

    let state = match ctx.settled_eth_chain_state.read() {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };

    match state.message_id_index.get(&message_id_bytes) {
        Some(hyli_tx_hash) => JsonRpcResponse::ok(id, json!({ "hyliTxHash": hyli_tx_hash })),
        None => JsonRpcResponse::ok(id, Value::Null),
    }
}

// ── hyli_getHyliHash ──────────────────────────────────────────────────────────

/// Custom method: return the Hyli blob tx hash for a given EVM tx hash.
/// Returns `{"hyliTxHash": "<hex>"}` or `null` if not found.
pub fn hyli_get_hyli_hash(ctx: &RouterCtx, id: Value, params: &Value) -> JsonRpcResponse {
    let evm_hash_hex = match params.get(0).and_then(|v| v.as_str()) {
        Some(s) => s.trim_start_matches("0x"),
        None => return JsonRpcResponse::err(id, -32602, "Missing evmTxHash param"),
    };

    let evm_hash_bytes: [u8; 32] = match hex::decode(evm_hash_hex)
        .ok()
        .and_then(|b| b.try_into().ok())
    {
        Some(b) => b,
        None => {
            return JsonRpcResponse::err(id, -32602, "Invalid evmTxHash (expected 32-byte hex)")
        }
    };

    if let Ok(submitted) = ctx.submitted_txs.read() {
        if let Some(tx) = submitted.get(&evm_hash_bytes) {
            return JsonRpcResponse::ok(id, json!({ "hyliTxHash": tx.hyli_tx_hash }));
        }
    } else {
        return JsonRpcResponse::err(id, -32603, "State lock poisoned");
    }

    match ctx.settled_eth_chain_state.read() {
        Ok(state) => match state.evm_to_hyli_hash.get(&evm_hash_bytes) {
            Some(hyli_hash) => JsonRpcResponse::ok(id, json!({ "hyliTxHash": hyli_hash })),
            None => JsonRpcResponse::ok(id, Value::Null),
        },
        Err(_) => JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    }
}

// ── debug_dumpGenesis ─────────────────────────────────────────────────────────

pub fn debug_dump_genesis(ctx: &RouterCtx, id: Value) -> JsonRpcResponse {
    let state = match ctx.settled_eth_chain_state.read() {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };
    let pending_state = match current_pending_state(ctx) {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };
    // Parse the original genesis JSON and replace its alloc with the current state.
    // This produces a complete valid genesis JSON (config, gasLimit, etc. preserved)
    // that can be pasted directly as evm_config_json in conf_defaults.toml.
    let mut genesis: serde_json::Value = match serde_json::from_slice(&state.genesis_json) {
        Ok(v) => v,
        Err(e) => {
            return JsonRpcResponse::err(id, -32603, format!("Failed to parse genesis JSON: {e}"))
        }
    };
    let alloc = state.dump_genesis_alloc();
    let n = alloc.as_object().map(|m| m.len()).unwrap_or(0);
    genesis["alloc"] = alloc;
    genesis["pendingStateRoot"] = json!(format!("0x{}", hex::encode(pending_state.state_root)));
    info!(accounts = n, "debug_dumpGenesis");
    JsonRpcResponse::ok(id, genesis)
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

    let state = match state_for_view(
        ctx,
        parse_block_view(params.get(1).and_then(|v| v.as_str())),
    ) {
        Ok(s) => s,
        Err(_) => return JsonRpcResponse::err(id, -32603, "State lock poisoned"),
    };

    let selector = if data.len() >= 4 {
        format!("0x{}", hex::encode(&data[..4]))
    } else {
        "none".to_string()
    };
    let (success, output) = state.execute_call(from, to, data, value);
    let output_hex = format!("0x{}", hex::encode(&output));

    info!(
        "eth_call to={} selector={} success={} output={}",
        to.map(|a| format!("{a:?}")).unwrap_or_default(),
        selector,
        success,
        output_hex
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
    let submitted_raw = raw_bytes.clone();
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
    let hyli_tx_hash = match ctx.node_client.send_tx_blob(blob_tx).await {
        Ok(h) => h,
        Err(e) => return JsonRpcResponse::internal_error(id, format!("Failed to send tx: {e}")),
    };

    // Store EVM hash → Hyli blob hash so the frontend can build correct explorer links.
    let hyli_hash_hex = hex::encode(&hyli_tx_hash.0);
    if let Ok(mut submitted) = ctx.submitted_txs.write() {
        submitted.insert(
            *evm_tx_hash,
            SubmittedTx {
                raw_eip2718: submitted_raw,
                hyli_tx_hash: hyli_hash_hex,
            },
        );
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
            .settled_eth_chain_state
            .read()
            .map(|s| s.settled_receipts.get(&hash_bytes).cloned())
            .unwrap_or(None);
        let submitted = ctx
            .submitted_txs
            .read()
            .ok()
            .and_then(|txs| txs.get(&hash_bytes).cloned());

        // Only return a response when we have a receipt (tx is confirmed).
        // If the tx is in submitted_txs but has no receipt yet (still pending on Hyli),
        // fall through to return null — this matches pre-commit behaviour and avoids
        // confusing ethers-rs's PendingTransaction state machine with a "pending in
        // mempool" response that it never sees transition to confirmed.
        if let Some(receipt) = receipt.as_ref() {
            // Prefer submitted.raw_eip2718 (exact bytes from eth_sendRawTransaction) when
            // available, so the `from` address and other fields are accurate.
            let raw = submitted
                .as_ref()
                .map(|s| s.raw_eip2718.as_slice())
                .unwrap_or(receipt.raw_eip2718.as_slice());
            if let Some(tx_json) = encode_tx_json(raw, &hash_bytes, Some(receipt)) {
                return JsonRpcResponse::ok(id, tx_json);
            }
            // encode_tx_json failed (e.g. unsupported tx type) — fall back to a minimal
            // response that still carries the block info ethers-rs needs.
            return JsonRpcResponse::ok(
                id,
                json!({
                    "hash": format!("0x{hash_hex}"),
                    "blockHash": format!("0x{}", hex::encode(receipt.block_hash)),
                    "blockNumber": format!("0x{:x}", receipt.block_number),
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
            .settled_eth_chain_state
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

            // Decode from/to from raw tx bytes; prefer submitted_txs (exact bytes
            // from eth_sendRawTransaction) over receipt bytes so the signer is accurate.
            let submitted_raw = ctx
                .submitted_txs
                .read()
                .ok()
                .and_then(|txs| txs.get(&hash_bytes).map(|s| s.raw_eip2718.clone()));
            let raw = submitted_raw.as_deref().unwrap_or(r.raw_eip2718.as_slice());
            let (from_str, to_val) = if let Ok(tx) = TransactionSigned::decode_2718(&mut &*raw) {
                let from = tx
                    .recover_signer()
                    .ok()
                    .map(|a| format!("{a:?}"))
                    .unwrap_or_else(|| "0x0000000000000000000000000000000000000000".to_string());
                let to = tx
                    .to()
                    .map(|addr| Value::String(format!("{addr:?}")))
                    .unwrap_or(Value::Null);
                (from, to)
            } else {
                (
                    "0x0000000000000000000000000000000000000000".to_string(),
                    Value::Null,
                )
            };

            let logs_json: Vec<Value> = r
                .logs
                .iter()
                .enumerate()
                .map(|(i, log)| {
                    let topics_json: Vec<String> = log
                        .topics()
                        .iter()
                        .map(|t| format!("0x{}", hex::encode(t.0)))
                        .collect();
                    json!({
                        "address": format!("{:?}", log.address),
                        "topics": topics_json,
                        "data": format!("0x{}", hex::encode(log.data.data.as_ref())),
                        "blockNumber": format!("0x{:x}", r.block_number),
                        "transactionHash": format!("0x{hash_hex}"),
                        "blockHash": format!("0x{}", hex::encode(r.block_hash)),
                        "transactionIndex": "0x0",
                        "logIndex": format!("0x{:x}", i),
                        "removed": false,
                    })
                })
                .collect();
            return JsonRpcResponse::ok(
                id,
                json!({
                    "transactionHash": format!("0x{hash_hex}"),
                    "transactionIndex": "0x0",
                    "blockHash": format!("0x{}", hex::encode(r.block_hash)),
                    "blockNumber": format!("0x{:x}", r.block_number),
                    "from": from_str,
                    "to": to_val,
                    "cumulativeGasUsed": "0x0",
                    "gasUsed": "0x0",
                    "logs": logs_json,
                    "logsBloom": "0x".to_string() + &"0".repeat(512),
                    "status": if r.success { "0x1" } else { "0x0" },
                    "type": "0x2",
                }),
            );
        }
    }

    JsonRpcResponse::ok(id, Value::Null)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reth_module::eth_chain_state::tests::{make_signed_transfer, TEST_GENESIS};
    use sdk::Identity;

    fn make_ctx() -> RouterCtx {
        RouterCtx {
            hyli_chain_id: 1213811785,
            bridge_cn: ContractName("bridge".into()),
            hyperlane_cn: ContractName("reth".into()),
            relayer_identity: Identity("test".into()),
            node_client: Arc::new(
                NodeApiHttpClient::new("http://localhost:1".to_string()).unwrap(),
            ),
            indexer_client: Arc::new(
                IndexerApiHttpClient::new("http://localhost:1".to_string()).unwrap(),
            ),
            settled_eth_chain_state: Arc::new(RwLock::new(
                EthChainState::new(TEST_GENESIS.as_bytes()).unwrap(),
            )),
            pending_eth_chain_state: Arc::new(RwLock::new(
                EthChainState::new(TEST_GENESIS.as_bytes()).unwrap(),
            )),
            submitted_txs: Arc::new(RwLock::new(HashMap::new())),
            is_ready: Arc::new(AtomicBool::new(true)),
        }
    }

    #[test]
    fn pending_nonce_includes_submitted_transactions() {
        let ctx = make_ctx();
        let raw = make_signed_transfer(0);
        let tx_hash: [u8; 32] = *alloy_primitives::keccak256(&raw);

        ctx.submitted_txs.write().unwrap().insert(
            tx_hash,
            SubmittedTx {
                raw_eip2718: raw,
                hyli_tx_hash: "deadbeef".into(),
            },
        );

        let latest = eth_get_transaction_count(
            &ctx,
            json!(1),
            &json!(["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266", "latest"]),
        );
        let pending = eth_get_transaction_count(
            &ctx,
            json!(1),
            &json!(["0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266", "pending"]),
        );

        assert_eq!(latest.result, Some(json!("0x0")));
        assert_eq!(pending.result, Some(json!("0x1")));
    }

    #[test]
    fn transaction_by_hash_returns_null_for_pending_transaction() {
        // A tx that is in submitted_txs but not yet settled should return null so that
        // ethers-rs PendingTransaction keeps polling rather than entering a "pending in
        // mempool" state that may never transition to confirmed.
        let ctx = make_ctx();
        let raw = make_signed_transfer(0);
        let tx_hash: [u8; 32] = *alloy_primitives::keccak256(&raw);

        ctx.submitted_txs.write().unwrap().insert(
            tx_hash,
            SubmittedTx {
                raw_eip2718: raw,
                hyli_tx_hash: "deadbeef".into(),
            },
        );

        let resp = eth_get_transaction_by_hash(
            &ctx,
            json!(1),
            &json!([format!("0x{}", hex::encode(tx_hash))]),
        );
        // No receipt yet → null, not a "pending" object.
        assert_eq!(resp.result, Some(Value::Null));
    }

    #[test]
    fn transaction_by_hash_returns_settled_transaction_without_submitted_cache() {
        let ctx = make_ctx();
        let raw = make_signed_transfer(0);
        let tx_hash: [u8; 32] = *alloy_primitives::keccak256(&raw);
        {
            let mut state = ctx.settled_eth_chain_state.write().unwrap();
            state.apply_transaction_speculative(&raw).unwrap();
            state.record_settled_receipt(&raw, true, vec![]);
        }

        let resp = eth_get_transaction_by_hash(
            &ctx,
            json!(1),
            &json!([format!("0x{}", hex::encode(tx_hash))]),
        );
        let tx = resp.result.expect("settled tx result");

        assert_ne!(tx["blockHash"], Value::Null);
        assert_ne!(tx["blockNumber"], Value::Null);
        assert_eq!(tx["nonce"], json!("0x0"));
    }

    #[test]
    fn pending_block_view_uses_pending_state() {
        let ctx = make_ctx();
        let raw = make_signed_transfer(0);
        ctx.pending_eth_chain_state
            .write()
            .unwrap()
            .apply_transaction_speculative(&raw)
            .unwrap();

        let latest = eth_get_block_by_number(&ctx, json!(1), &json!(["latest", false]));
        let pending = eth_get_block_by_number(&ctx, json!(1), &json!(["pending", false]));

        assert_eq!(latest.result.unwrap()["number"], json!("0x0"));
        assert_eq!(pending.result.unwrap()["number"], json!("0x1"));
    }

    #[test]
    fn hyli_hash_lookup_falls_back_to_submitted_cache() {
        let ctx = make_ctx();
        let raw = make_signed_transfer(0);
        let tx_hash: [u8; 32] = *alloy_primitives::keccak256(&raw);

        ctx.submitted_txs.write().unwrap().insert(
            tx_hash,
            SubmittedTx {
                raw_eip2718: raw,
                hyli_tx_hash: "deadbeef".into(),
            },
        );

        let resp = hyli_get_hyli_hash(
            &ctx,
            json!(1),
            &json!([format!("0x{}", hex::encode(tx_hash))]),
        );

        assert_eq!(resp.result, Some(json!({ "hyliTxHash": "deadbeef" })));
    }
}
