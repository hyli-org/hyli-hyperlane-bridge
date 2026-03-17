use anyhow::{Context, Result};
use borsh::{BorshDeserialize, BorshSerialize};
use client_sdk::transaction_builder::{
    ProvableBlobTx, TxExecutorHandler as TxExecutorHandlerTrait,
};

pub mod metadata {
    pub const HYPERLANE_BRIDGE_ELF: &[u8] = include_bytes!("../../hyperlane-bridge.img");
    pub const HYPERLANE_BRIDGE_PROGRAM_ID: [u8; 32] =
        sdk::str_to_u8(include_str!("../../hyperlane-bridge.txt"));
}
use sdk::{utils::as_hyli_output, Calldata, ContractName, StateCommitment, ZkContract};

use crate::{HyperlaneBridgeAction, HyperlaneBridgeState, BRIDGE_CONTRACT_NAME};

/// Client-side state handler for the hyperlane-bridge policy contract.
#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct TxExecutorHandler {
    pub state: HyperlaneBridgeState,
}

impl TxExecutorHandlerTrait for TxExecutorHandler {
    type Contract = HyperlaneBridgeState;

    fn handle(&mut self, calldata: &Calldata) -> Result<sdk::HyliOutput> {
        let initial_state = self.state.commit();
        let mut res = self.state.execute(calldata);
        let next_state = self.state.commit();
        Ok(as_hyli_output(
            initial_state,
            next_state,
            calldata,
            &mut res,
        ))
    }

    fn build_commitment_metadata(&self, _calldata: &Calldata) -> Result<Vec<u8>> {
        borsh::to_vec(&self.state).context("Failed to serialize HyperlaneBridgeState")
    }

    fn construct_state(
        _contract_name: &ContractName,
        _contract: &sdk::Contract,
        metadata: &Option<Vec<u8>>,
    ) -> Result<Self> {
        let state = match metadata {
            Some(m) => borsh::from_slice::<HyperlaneBridgeState>(m)
                .context("Failed to deserialize HyperlaneBridgeState")?,
            None => HyperlaneBridgeState::default(),
        };
        Ok(Self { state })
    }

    fn get_state_commitment(&self) -> StateCommitment {
        self.state.commit()
    }
}

impl TxExecutorHandler {
    /// Add a `VerifyTransaction` blob to the builder.
    ///
    /// This must be called after the reth blob for the `hyperlane` contract has
    /// already been pushed to `builder.blobs`, since the RISC0 proof will scan
    /// that blob to run policy checks.
    pub fn verify_transaction(&self, builder: &mut ProvableBlobTx) -> Result<()> {
        builder.add_action(
            ContractName(BRIDGE_CONTRACT_NAME.to_string()),
            HyperlaneBridgeAction::VerifyTransaction,
            None,
            None,
            None,
        )?;
        Ok(())
    }
}
