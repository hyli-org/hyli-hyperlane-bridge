#!/bin/bash
set -e

if [[ "$1" == "--rm" ]]; then
  rm -rf hyperlane_db/
fi
mkdir -p hyperlane_db
mkdir -p validator-signatures

docker run --rm --network host \
  -e CONFIG_FILES=/agent-config.json \
  --mount type=bind,source=$(pwd)/configs/agent-config.json,target=/agent-config.json,readonly \
  --mount type=bind,source=$(pwd)/hyperlane_db,target=/hyperlane_db \
  --mount type=bind,source=$(pwd)/validator-signatures,target=/tmp/validator-signatures \
  ghcr.io/hyperlane-xyz/hyperlane-agent:agents-v2.1.0 \
  ./relayer --db /hyperlane_db \
    --defaultSigner.key $RELAYER_KEY \
    --relayChains hyli,sepolia \
    --allowLocalCheckpointSyncers true
