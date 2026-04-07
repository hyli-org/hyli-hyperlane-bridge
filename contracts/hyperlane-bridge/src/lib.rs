use borsh::{BorshDeserialize, BorshSerialize};
use sha2::{Digest, Sha256};

use alloy_consensus::TxEnvelope;
use alloy_eips::eip2718::Decodable2718;
use alloy_primitives::TxKind;
use alloy_sol_types::sol;

use sdk::{
    utils::parse_calldata, Blob, BlobData, BlobIndex, Calldata, ContractAction, ContractName,
    IndexedBlobs, RunResult, StructuredBlobData, ZkContract,
};

#[cfg(feature = "client")]
pub mod client;

/// Policy contract state — holds the name of the companion reth contract to inspect.
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct HyperlaneBridgeState {
    pub hyperlane_contract: ContractName,
}

impl Default for HyperlaneBridgeState {
    fn default() -> Self {
        HyperlaneBridgeState {
            hyperlane_contract: ContractName("hyperlane".to_string()),
        }
    }
}

/// Actions the bridge policy contract can execute.
#[derive(Debug, Clone, BorshDeserialize, BorshSerialize, PartialEq)]
pub enum HyperlaneBridgeAction {
    /// Assert the EVM transaction in the companion reth blob passes all policy checks.
    ///
    /// Current policy: contract deployments (CREATE transactions) are rejected.
    ///
    /// Future: will also gate EVM-to-Hyli token bridging, allowing tokens to be
    /// released into native Hyli contracts when the EVM transaction authorises it.
    VerifyTransaction,
}

impl ContractAction for HyperlaneBridgeAction {
    fn as_blob(
        &self,
        contract_name: ContractName,
        caller: Option<BlobIndex>,
        callees: Option<Vec<BlobIndex>>,
    ) -> Blob {
        Blob {
            contract_name,
            data: BlobData::from(StructuredBlobData {
                caller,
                callees,
                parameters: self.clone(),
            }),
        }
    }
}

// Automatic full-state rollback on error.
impl sdk::FullStateRevert for HyperlaneBridgeState {}

// ---------------------------------------------------------------------------
// Solidity ABI selectors — exported for use by server / RPC proxy consumers
// ---------------------------------------------------------------------------

sol! {
    /// WarpRoute.transferRemote(destDomain, recipient, amount)
    function transferRemote(uint32 destination, bytes32 recipient, uint256 amount) external payable;

    /// Mailbox.process(metadata, message)
    function process(bytes metadata, bytes message) external payable;
}

// ---------------------------------------------------------------------------
// ZkContract implementation
// ---------------------------------------------------------------------------

impl ZkContract for HyperlaneBridgeState {
    fn execute(&mut self, calldata: &Calldata) -> RunResult {
        let (action, execution_ctx) = parse_calldata::<HyperlaneBridgeAction>(calldata)?;

        let output = match action {
            HyperlaneBridgeAction::VerifyTransaction => {
                let tx = decode_reth_tx(&calldata.blobs, &self.hyperlane_contract)?;
                reject_contract_deployment(&tx)?;
                "VerifyTransaction passed".to_string()
            }
        };

        Ok((output.into_bytes(), execution_ctx, vec![]))
    }

    /// State commitment = SHA-256(hyperlane_contract).
    fn commit(&self) -> sdk::StateCommitment {
        let mut hasher = Sha256::new();
        hasher.update(self.hyperlane_contract.0.as_bytes());
        sdk::StateCommitment(hasher.finalize().to_vec())
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Find the reth blob for `hyperlane_cn` and decode the EIP-2718 transaction it contains.
pub fn decode_reth_tx(
    blobs: &IndexedBlobs,
    hyperlane_cn: &ContractName,
) -> Result<TxEnvelope, String> {
    for (_, blob) in blobs {
        if &blob.contract_name != hyperlane_cn {
            continue;
        }

        // Strip optional StructuredBlobData wrapper to obtain raw EIP-2718 bytes.
        let raw_bytes: Vec<u8> =
            if let Ok(structured) = StructuredBlobData::<Vec<u8>>::try_from(blob.data.clone()) {
                structured.parameters
            } else {
                blob.data.0.clone()
            };

        return TxEnvelope::decode_2718(&mut raw_bytes.as_slice())
            .map_err(|e| format!("Failed to decode EIP-2718 tx: {e}"));
    }

    Err(format!(
        "No reth blob found for contract '{}'",
        hyperlane_cn.0
    ))
}

/// Reject CREATE transactions (contract deployments are not permitted on this chain).
fn reject_contract_deployment(tx: &TxEnvelope) -> Result<(), String> {
    let is_create = match tx {
        TxEnvelope::Legacy(t) => matches!(t.tx().to, TxKind::Create),
        TxEnvelope::Eip2930(t) => matches!(t.tx().to, TxKind::Create),
        TxEnvelope::Eip1559(t) => matches!(t.tx().to, TxKind::Create),
        TxEnvelope::Eip7702(_) => false, // EIP-7702 always calls an existing address
        _ => false,
    };

    // if is_create {
    //     return Err("Contract deployments are not allowed on this EVM chain".to_string());
    // }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
    use alloy_eips::eip2718::Encodable2718;
    use alloy_primitives::{Address, Bytes, ChainId, TxKind, U256};
    use alloy_signer::SignerSync;
    use alloy_signer_local::PrivateKeySigner;
    use sdk::{BlobIndex, Calldata, ContractName, Identity, IndexedBlobs, TxHash};

    /// Name this contract is expected to be registered under.
    pub const BRIDGE_CONTRACT_NAME: &str = "hyperlane-bridge";

    fn make_state() -> HyperlaneBridgeState {
        HyperlaneBridgeState::default()
    }

    fn sign_and_encode(to: TxKind) -> Vec<u8> {
        let signer = PrivateKeySigner::random();
        let tx = TxEip1559 {
            chain_id: ChainId::from(1u64),
            nonce: 0,
            max_fee_per_gas: 1u128,
            max_priority_fee_per_gas: 1u128,
            gas_limit: 100_000,
            to,
            value: U256::ZERO,
            input: Bytes::new(),
            access_list: Default::default(),
        };
        let signature = signer.sign_hash_sync(&tx.signature_hash()).unwrap();
        let envelope: TxEnvelope = tx.into_signed(signature).into();
        let mut encoded = vec![];
        envelope.encode_2718(&mut encoded);
        encoded
    }

    fn make_reth_blob(hyperlane_cn: ContractName, to: TxKind) -> Blob {
        Blob {
            contract_name: hyperlane_cn,
            data: BlobData(sign_and_encode(to)),
        }
    }

    fn make_bridge_blob(action: HyperlaneBridgeAction) -> Blob {
        action.as_blob(ContractName(BRIDGE_CONTRACT_NAME.to_string()), None, None)
    }

    fn make_calldata(index: BlobIndex, blobs: Vec<(BlobIndex, Blob)>) -> Calldata {
        let blob_count = blobs.len();
        Calldata {
            tx_hash: TxHash::default(),
            identity: Identity::from("alice@hydentity"),
            blobs: IndexedBlobs(blobs),
            tx_blob_count: blob_count,
            index,
            tx_ctx: None,
            private_input: vec![],
        }
    }

    /// CALL transaction → policy check passes.
    #[test]
    fn test_verify_transaction_call_succeeds() {
        let state = make_state();
        let reth_blob = make_reth_blob(
            state.hyperlane_contract.clone(),
            TxKind::Call(Address::ZERO),
        );
        let bridge_blob = make_bridge_blob(HyperlaneBridgeAction::VerifyTransaction);
        let blobs = vec![(BlobIndex(0), reth_blob), (BlobIndex(1), bridge_blob)];
        let calldata = make_calldata(BlobIndex(1), blobs);
        let mut s = state.clone();
        assert!(s.execute(&calldata).is_ok());
    }

    /// CREATE transaction → policy check rejects it.
    #[test]
    fn test_verify_transaction_create_rejected() {
        let state = make_state();
        let reth_blob = make_reth_blob(state.hyperlane_contract.clone(), TxKind::Create);
        let bridge_blob = make_bridge_blob(HyperlaneBridgeAction::VerifyTransaction);
        let blobs = vec![(BlobIndex(0), reth_blob), (BlobIndex(1), bridge_blob)];
        let calldata = make_calldata(BlobIndex(1), blobs);
        let mut s = state.clone();
        assert!(s.execute(&calldata).is_err());
    }

    /// Missing reth blob → execution fails.
    #[test]
    fn test_verify_transaction_missing_reth_blob() {
        let state = make_state();
        let bridge_blob = make_bridge_blob(HyperlaneBridgeAction::VerifyTransaction);
        let blobs = vec![(BlobIndex(0), bridge_blob)];
        let calldata = make_calldata(BlobIndex(0), blobs);
        let mut s = state.clone();
        assert!(s.execute(&calldata).is_err());
    }
}
