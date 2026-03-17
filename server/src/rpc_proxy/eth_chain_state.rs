use anyhow::Result;
use client_sdk::rest_client::NodeApiClient;
use indexmap::IndexMap;
use sdk::{
    BlobIndex, BlobTransaction, ContractName, ProgramId, ProofData, ProofTransaction, TxContext,
    TxId, Verifier,
};
use std::sync::Arc;
use tracing::debug;

/// Current tracked state of the Hyli-hosted Ethereum chain (the `hyperlane` reth contract).
/// Updated from `ContractListenerEvent::SettledTx` events.
#[derive(Debug, Clone)]
pub struct EthChainState {
    /// Current EVM state root (updated on each settled reth transaction).
    pub state_root: [u8; 32],
    /// Number of settled transactions (used as the EVM block number).
    pub block_number: u64,
}

impl EthChainState {
    pub fn new(initial_state_root: [u8; 32]) -> Self {
        Self {
            state_root: initial_state_root,
            block_number: 0,
        }
    }
}

/// A transaction sequenced on the hyperlane reth contract, awaiting a reth proof.
#[derive(Debug, Clone)]
pub struct PendingRethProof {
    pub tx_id: TxId,
    pub hyli_tx: BlobTransaction,
    pub tx_ctx: Arc<TxContext>,
    /// Index of the hyperlane reth blob within the transaction blobs.
    pub blob_index: BlobIndex,
    /// Raw EIP-2718 encoded transaction bytes (the actual EVM transaction).
    pub raw_eip2718: Vec<u8>,
}

/// Pending proofs indexed by `TxId`.
pub type PendingProofsMap = IndexMap<TxId, PendingRethProof>;

/// Extract raw EIP-2718 bytes from a reth blob, unwrapping any `StructuredBlobData` wrapper.
fn extract_raw_eip2718(data: &[u8]) -> Vec<u8> {
    if let Ok(structured) =
        sdk::StructuredBlobData::<Vec<u8>>::try_from(sdk::BlobData(data.to_vec()))
    {
        structured.parameters
    } else {
        data.to_vec()
    }
}

/// Extract the reth blob from a transaction and build a `PendingRethProof`.
/// Returns `None` if the transaction has no blob for `hyperlane_cn`.
pub fn extract_pending_proof(
    tx_id: TxId,
    hyli_tx: BlobTransaction,
    tx_ctx: Arc<TxContext>,
    hyperlane_cn: &ContractName,
) -> Option<PendingRethProof> {
    let (blob_index, raw_eip2718) = hyli_tx
        .blobs
        .iter()
        .enumerate()
        .find(|(_, b)| b.contract_name == *hyperlane_cn)
        .map(|(i, b)| (BlobIndex(i), extract_raw_eip2718(&b.data.0)))?;

    Some(PendingRethProof {
        tx_id,
        hyli_tx,
        tx_ctx,
        blob_index,
        raw_eip2718,
    })
}

/// Build and submit a reth `ProofTransaction` for a sequenced reth blob.
///
/// The reth proof payload format (as expected by the reth verifier in `hyli-verifiers`):
/// ```text
/// [4-byte LE u32: calldata_len] [borsh(Calldata)]
/// [4-byte LE u32: stateless_len] [bincode(StatelessInput)]
/// [4-byte LE u32: evm_config_len] [JSON chain-spec bytes]
/// ```
pub async fn build_reth_proof_payload(
    pending: &PendingRethProof,
    current_state_root: [u8; 32],
    evm_config_json: &[u8],
) -> Result<Vec<u8>> {
    use borsh::to_vec as borsh_to_vec;
    use sdk::Calldata;

    // Build the Hyli calldata for this blob.
    // TxId is (DataProposalHash, TxHash); .1 gives the TxHash.
    let tx_hash = pending.tx_id.1.clone();
    let calldata = Calldata {
        identity: pending.hyli_tx.identity.clone(),
        tx_hash,
        private_input: vec![],
        blobs: pending.hyli_tx.blobs.clone().into(),
        index: pending.blob_index,
        tx_ctx: Some((*pending.tx_ctx).clone()),
        tx_blob_count: pending.hyli_tx.blobs.len(),
    };

    let calldata_bytes = borsh_to_vec(&calldata)
        .map_err(|e| anyhow::anyhow!("Failed to borsh-encode calldata: {e}"))?;

    // Build the StatelessInput for this transaction.
    let stateless_bytes = build_stateless_input(
        &pending.raw_eip2718,
        current_state_root,
        pending.tx_ctx.block_height.0,
        evm_config_json,
    )
    .await?;

    // Assemble the reth proof payload: each segment is length-prefixed (4-byte LE u32).
    let mut payload = Vec::new();
    write_segment(&mut payload, &calldata_bytes);
    write_segment(&mut payload, &stateless_bytes);
    write_segment(&mut payload, evm_config_json);

    Ok(payload)
}

/// Write a length-prefixed segment (4-byte LE u32 + data) into `buf`.
fn write_segment(buf: &mut Vec<u8>, data: &[u8]) {
    let len = data.len() as u32;
    buf.extend_from_slice(&len.to_le_bytes());
    buf.extend_from_slice(data);
}

/// Build the bincode-encoded `StatelessInput` for a single EIP-2718 transaction.
///
/// This is where a Reth execution instance is needed: given the raw transaction bytes and
/// the current EVM state root, execute the transaction against the EVM state and collect
/// the execution witness (sparse state trie nodes, bytecodes, ancestor headers).
///
/// # TODO – EVM executor integration
/// Implement using one of:
/// 1. **In-process revm + trie** – maintain the full EVM state locally (add `revm`,
///    `alloy-trie` or `reth-trie-sparse` deps). Execute the tx with `revm`'s `EVM` builder,
///    collect touched accounts / storage slots, compute sparse trie proofs, and assemble
///    `ExecutionWitness { state, storage, codes, keys, headers }` + wrap in `StatelessInput`.
/// 2. **External reth node** – forward the raw tx to a reth JSON-RPC node with a custom
///    `engine_getExecutionWitness` or `debug_*` endpoint that returns a `StatelessInput`.
/// 3. **Block assembler service** – a dedicated micro-service that wraps a reth node
///    and accepts `(raw_eip2718, state_root)` → returns `bincode(StatelessInput)`.
async fn build_stateless_input(
    _raw_eip2718: &[u8],
    _current_state_root: [u8; 32],
    _block_number: u64,
    _evm_config_json: &[u8],
) -> Result<Vec<u8>> {
    anyhow::bail!(
        "Reth StatelessInput building is not yet implemented. \
         Wire up an EVM executor (revm in-process or external reth node) to \
         execute the EIP-2718 transaction and produce the execution witness."
    )
}

/// Submit a reth `ProofTransaction` to the Hyli node.
pub async fn submit_reth_proof(
    node: &client_sdk::rest_client::NodeApiHttpClient,
    hyperlane_cn: &ContractName,
    program_id: &ProgramId,
    proof_bytes: Vec<u8>,
) -> Result<sdk::TxHash> {
    let proof_tx = ProofTransaction {
        contract_name: hyperlane_cn.clone(),
        program_id: program_id.clone(),
        verifier: Verifier(hyli_model::verifiers::RETH.to_string()),
        proof: ProofData(proof_bytes),
    };

    debug!(
        contract_name =% hyperlane_cn,
        "Submitting reth ProofTransaction"
    );

    let tx_hash = node.send_tx_proof(proof_tx).await?;
    Ok(tx_hash)
}
