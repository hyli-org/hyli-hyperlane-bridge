# hyli-hyperlane-bridge

A bridge between [Hyli](https://hyli.org) and Ethereum using the [Hyperlane](https://hyperlane.xyz) interoperability protocol. The bridge lets users transfer tokens in both directions: locking tokens on Hyli and releasing them on Ethereum, or the reverse.

---

## Background: Hyli EVM Compatibility via `reth-verifier`

Hyli achieves EVM compatibility through the `reth-verifier`. A contract registered with this verifier is literally a self-contained EVM blockchain living inside Hyli. Each proof transaction advances it by one block containing exactly one Ethereum transaction.

How a proof transaction works:

- The **blob** for the `reth` contract contains the raw EIP-2718 encoded Ethereum transaction.
- The **proof payload** contains three segments: the Hyli `Calldata` (with all blobs), a `StatelessInput` (the Ethereum block wrapping that single transaction plus an execution witness), and a chain spec / genesis config.
- The `reth-verifier` runs full stateless EVM block execution against the witness, checks that all receipts succeeded, and verifies that the blob data matches exactly the transaction in the block.
- State transitions as `initial_state = parent_header.state_root` → `next_state = block.header.state_root`. So the contract's Hyli state commitment IS the current Ethereum state root of its chain.

Each `reth` contract is therefore its own chain: it has a genesis (the configured initial state root), and each proven blob transaction appends one Ethereum block (one tx) to it, advancing the state root. Hyli never runs a persistent EVM node; every state transition is proven on demand from a stateless witness.

---

## Architecture

The system has two contracts and one server process.

### Contracts

| Contract | Verifier | State | Purpose |
|---|---|---|---|
| `hyperlane` | `reth` | Current Ethereum state root (32 bytes) | A self-contained EVM chain on Hyli; each proven tx advances it by one Ethereum block |
| `hyperlane-bridge` | `risc0-3` | SHA-256(hyperlane_cn \|\| token_cn) | ZK bridge logic: validates cross-chain token transfers |

**`hyperlane` contract** (`reth` verifier)
This is an EVM blockchain running inside Hyli. Its genesis state root is set to the configured `eth_state_root` at registration time. Every blob transaction that touches this contract must contain a raw EIP-2718 Ethereum transaction as its blob data, together with a proof that bundles a `StatelessInput` (a single-transaction Ethereum block + execution witness) and the chain spec. The verifier runs full stateless EVM execution, checks all receipts succeeded, and transitions the state root from `parent_header.state_root` to `block.header.state_root`. This is how the bridge proves that a `WarpRoute.transferRemote` or `Mailbox.process` call actually executed on an EVM — the EVM execution happens inside the proof, not on any external node.

**`hyperlane-bridge` contract** (`risc0-3` verifier)
Implements the bridge logic, proven with RISC0. It has two actions:

- `TransferRemote { eth_recipient, amount }` — Hyli → Ethereum path. Verifies that a `smt-token Transfer` to the bridge reserve matches the `amount` in the reth blob's `transferRemote` call.
- `ProcessMessage` — Ethereum → Hyli path. Decodes the `Mailbox.process` call from the reth blob to extract `(recipient, amount)`, then asserts the callee `smt-token Transfer` releases exactly that amount from the bridge to the recipient.

### Server

A Rust async server (`bridge-server`) composed of four modules:

- **`HylaneRpcProxyModule`** — JSON-RPC endpoint (Ethereum-compatible). Translates `eth_sendRawTransaction` calls from the Hyperlane relayer into Hyli blob transactions. Also stubs `eth_getLogs`, `eth_blockNumber`, `eth_chainId`, `eth_call`, etc.
- **`ContractListener`** — polls the indexer PostgreSQL database for new bridge contract transactions and emits them onto the internal message bus.
- **`AutoProver<BridgeTxHandler, Risc0Prover>`** — batches transactions from the bus and generates RISC0 proofs for the `hyperlane-bridge` contract, then submits proof transactions to the Hyli node.
- **`RestApi`** — HTTP server hosting all module routes, including the RPC proxy and the AutoProver status endpoint.

---

## Data Flows

### Hyli → Ethereum (`TransferRemote`)

```
User
 │
 ├─ sends blob tx with 3 blobs:
 │    blob[0]: hyperlane reth blob  ← raw EIP-2718 ETH tx calling WarpRoute.transferRemote(dest, recipient, amount)
 │    blob[1]: hyperlane-bridge blob ← TransferRemote { eth_recipient, amount }
 │    blob[2]: smt-token blob        ← Transfer { sender: user, recipient: "hyperlane-bridge", amount }
 │
 ▼
Hyli node
 │
 ├─ AutoProver generates RISC0 proof for blob[1]:
 │    • checks blob[2]: smt-token Transfer to "hyperlane-bridge" with correct amount ✓
 │    • checks blob[0]: reth blob encodes transferRemote with matching amount ✓
 │
 ▼
Hyperlane relayer
 │
 ├─ polls eth_getLogs on RPC proxy → receives synthetic Dispatch log
 │    (built from the TransferRemote parameters in the bridge blob)
 │
 ├─ delivers proof to Ethereum Mailbox.process()
 │
 ▼
Ethereum: tokens released to eth_recipient
```

### Ethereum → Hyli (`ProcessMessage`)

```
Hyperlane relayer
 │
 ├─ detects Dispatch on Ethereum, calls eth_sendRawTransaction on RPC proxy
 │    with: raw EIP-2718 tx calling Mailbox.process(metadata, message)
 │
 ▼
RPC proxy (eth_send_raw_transaction handler)
 │
 ├─ decodes Mailbox.process calldata → extracts (hyli_recipient, amount) from TokenMessage body
 │
 ├─ builds 3-blob transaction:
 │    blob[0]: hyperlane reth blob  ← the original raw EIP-2718 bytes
 │    blob[1]: hyperlane-bridge blob ← ProcessMessage
 │    blob[2]: smt-token blob        ← Transfer { sender: "hyperlane-bridge", recipient, amount }
 │
 ├─ submits to Hyli node
 │
 ▼
AutoProver generates RISC0 proof for blob[1]:
 │    • decodes reth blob → extracts (recipient, amount) from Mailbox.process message ✓
 │    • checks callee blob[2]: smt-token Transfer from bridge to recipient, exact amount ✓
 │
 ▼
Hyli: tokens released to hyli_recipient
```

---

## Server Components

### `HylaneRpcProxyModule` (`server/src/rpc_proxy/`)

An Ethereum JSON-RPC endpoint that the Hyperlane relayer connects to instead of a real Ethereum node.

| RPC method | Behavior |
|---|---|
| `eth_sendRawTransaction` | Decodes `Mailbox.process` call, builds 3-blob Hyli tx, submits to node |
| `eth_getLogs` | Scans indexer for `TransferRemote` bridge txs, synthesizes `Dispatch` event logs |
| `eth_blockNumber` | Proxies Hyli block height |
| `eth_chainId` / `net_version` | Returns configured `hyli_chain_id` |
| `eth_getBlockByNumber` | Returns Hyli block data in Ethereum block format |
| `eth_getTransactionReceipt` | Looks up tx status in indexer |
| `eth_call` | Stubs `latestCheckpoint()` and `threshold()` for Hyperlane validator queries |

### `ContractListener` (`hyli-modules`)

Polls the indexer PostgreSQL database on a configurable interval for new transactions involving the bridge contract. Publishes them to the shared bus for the AutoProver to consume.

### `AutoProver<BridgeTxHandler, Risc0Prover>` (`hyli-modules`)

Consumes transactions from the bus, batches them (configurable buffer size, max batch size, idle flush interval), generates a RISC0 proof using the `hyperlane-bridge` ELF, and submits the proof transaction back to the Hyli node.

### `RestApi` (`hyli-modules`)

Axum HTTP server. Mounts routes from all other modules (RPC proxy at `/`, AutoProver status at its own path).

---

## Configuration

Configuration is read from a TOML file (default: `bridge-server.toml`), with `BRIDGE_*` environment variable overrides. All defaults are in `server/src/conf_defaults.toml`.

| Field | Default | Description |
|---|---|---|
| `node_url` | `http://localhost:4321` | Hyli node HTTP endpoint |
| `server_port` | `4000` | Port the bridge server listens on |
| `log_format` | `full` | Log format: `plain`, `json`, or `node` |
| `hyli_chain_id` | `1337` | Domain ID presented to Hyperlane agents as the chain ID |
| `eth_state_root` | `000...000` | 32-byte hex Ethereum state root to initialize the `hyperlane` contract |
| `bridge_cn` | `hyperlane-bridge` | Hyli contract name for the bridge contract |
| `hyperlane_cn` | `hyperlane` | Hyli contract name for the reth/Ethereum state contract |
| `token_cn` | `oranj` | Hyli contract name for the smt-token |
| `relayer_key` | _(none)_ | Hex secp256k1 private key used to sign Hyli transactions; if absent uses `relayer@hyperlane-bridge` |
| `data_directory` | `data/bridge-server` | Directory for module state files |
| `noinit` | `false` | Skip contract registration on startup |
| `indexer_database_url` | `postgresql://postgres:postgres@localhost:5432/hyli_indexer` | PostgreSQL URL for ContractListener |
| `contract_listener_poll_interval_secs` | `5` | How often ContractListener polls the DB |
| `prover_tx_buffer_size` | `0` | Min transactions before flushing a proof batch (0 = flush immediately) |
| `prover_max_txs_per_proof` | `10` | Max transactions per RISC0 proof batch |
| `prover_tx_working_window_size` | `50` | Size of in-flight transaction window |
| `prover_idle_flush_interval_secs` | `2` | Flush buffered transactions after this many idle seconds |

---

## Getting Started

### Prerequisites

- Rust toolchain (stable)
- A running Hyli node
- A running Hyli indexer with PostgreSQL
- RISC0 toolchain (for proof generation)
- A Hyperlane relayer configured to use this server's RPC endpoint

### Build

```bash
cargo build --release -p bridge-server
```

### Run

```bash
# Copy and edit the config
cp bridge-server.example.toml bridge-server.toml

# Start the server
./target/release/bridge-server --config-file bridge-server.toml
```

Or with environment variable overrides:

```bash
BRIDGE_NODE_URL=http://mynode:4321 \
BRIDGE_ETH_STATE_ROOT=<32-byte-hex-root> \
./target/release/bridge-server
```

### Contract Reference

On first startup (when `noinit = false`), the server registers both contracts if they do not already exist:

1. **`hyperlane`** — registered with the `reth` verifier, program ID `[0u8; 65]` (no specific signer check), and initial state = the configured `eth_state_root`. This seeds the genesis of the EVM chain; subsequent proven blob transactions advance the state root one block at a time.
2. **`hyperlane-bridge`** — registered with the `risc0-3` verifier, program ID from the compiled ELF, and initial state = SHA-256(`hyperlane_cn` || `token_cn`).

The bridge contract's state never changes after initialization (its commit is a hash of the two companion contract names). The hyperlane contract's state root advances as Ethereum blocks are proven.
