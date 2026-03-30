#!/bin/bash
set -e

if [[ "$1" == "--rm" ]]; then
  rm -rf hyperlane_db_validator_hyli/
fi
mkdir -p hyperlane_db_validator_hyli
mkdir -p validator-signatures

docker run --rm --network host \
  -e CONFIG_FILES=/agent-config.json \
  -e HYP_CHAINS_SEPOLIA_BLOCKEXPLORERS_0_APIKEY="${ETHERSCAN_API_KEY:-}" \
  --mount type=bind,source=$(pwd)/configs/agent-config.json,target=/agent-config.json,readonly \
  --mount type=bind,source=$(pwd)/hyperlane_db_validator_hyli,target=/hyperlane_db_validator_hyli \
  --mount type=bind,source=$(pwd)/validator-signatures,target=/tmp/validator-signatures \
  ghcr.io/hyperlane-xyz/hyperlane-agent:agents-v2.1.0 \
  ./validator --db /hyperlane_db_validator_hyli \
    --validator.key $VALIDATOR_KEY \
    --originChainName hyli \
    --checkpointSyncer.type localStorage \
    --checkpointSyncer.path /tmp/validator-signatures \
    --metrics-port 9091
