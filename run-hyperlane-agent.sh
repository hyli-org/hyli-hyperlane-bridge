#!/bin/bash
set -euo pipefail

usage() {
  echo "usage: $0 relayer [--rm] | validator <hyli|sepolia> [--rm]" >&2
  exit 1
}

[[ $# -ge 1 ]] || usage

agent_type="$1"
shift

chain=""
if [[ "$agent_type" == "validator" ]]; then
  [[ $# -ge 1 ]] || usage
  chain="$1"
  shift
fi

rm_requested=false
if [[ "${1:-}" == "--rm" ]]; then
  rm_requested=true
  shift
fi

[[ $# -eq 0 ]] || usage

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
config_file="${HYP_AGENT_CONFIG_FILE:-$repo_root/configs/agent-config.json}"
data_root="${HYPERLANE_DATA_ROOT:-$repo_root/data/hyperlane}"
container_data_root="/data/hyperlane"

mkdir -p \
  "$data_root/relayer" \
  "$data_root/validator-hyli" \
  "$data_root/validator-sepolia" \
  "$data_root/validator-signatures/hyli" \
  "$data_root/validator-signatures/sepolia"

docker_image="${HYPERLANE_AGENT_IMAGE:-ghcr.io/hyperlane-xyz/hyperlane-agent:agents-v2.1.0}"
docker_args=(
  --rm
  --network host
  -e CONFIG_FILES=/etc/hyperlane/agent-config.json
  -e "HYP_CHAINS_SEPOLIA_BLOCKEXPLORERS_0_APIKEY=${ETHERSCAN_API_KEY:-}"
  --mount "type=bind,source=$config_file,target=/etc/hyperlane/agent-config.json,readonly"
  --mount "type=bind,source=$data_root,target=$container_data_root"
)

case "$agent_type" in
  relayer)
    : "${RELAYER_KEY:?RELAYER_KEY must be set}"
    db_dir="$data_root/relayer"
    $rm_requested && rm -rf "$db_dir"
    mkdir -p "$db_dir"

    exec docker run "${docker_args[@]}" \
      --rm \
      -e "RELAYER_KEY=$RELAYER_KEY" \
      "$docker_image" \
      ./relayer --db "$container_data_root/relayer" \
        --defaultSigner.key "$RELAYER_KEY" \
        --relayChains hyli,sepolia \
        --allowLocalCheckpointSyncers true \
        --gasPaymentEnforcement '[{"type":"none"}]'
    ;;
  validator)
    : "${VALIDATOR_KEY:?VALIDATOR_KEY must be set}"

    case "$chain" in
      hyli)
        signer_env_name="HYP_CHAINS_HYLI_SIGNER_KEY"
        db_dir="$data_root/validator-hyli"
        checkpoint_dir="$container_data_root/validator-signatures/hyli"
        metrics_port=9091
        ;;
      sepolia)
        signer_env_name="HYP_CHAINS_SEPOLIA_SIGNER_KEY"
        db_dir="$data_root/validator-sepolia"
        checkpoint_dir="$container_data_root/validator-signatures/sepolia"
        metrics_port=9092
        ;;
      *)
        usage
        ;;
    esac

    $rm_requested && rm -rf "$db_dir"
    mkdir -p "$db_dir"

    exec docker run "${docker_args[@]}" \
      -e "VALIDATOR_KEY=$VALIDATOR_KEY" \
      -e "$signer_env_name=$VALIDATOR_KEY" \
      "$docker_image" \
      ./validator --db "$container_data_root/${db_dir##"$data_root"/}" \
        --validator.key "$VALIDATOR_KEY" \
        --originChainName "$chain" \
        --checkpointSyncer.type localStorage \
        --checkpointSyncer.path "$checkpoint_dir" \
        --metrics-port "$metrics_port"
    ;;
  *)
    usage
    ;;
esac
