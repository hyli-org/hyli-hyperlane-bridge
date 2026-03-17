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
| `hyperlane-bridge` | `risc0-3` | SHA-256(hyperlane_cn) | Policy guard: inspects EVM transactions and enforces chain-level rules |

**`hyperlane` contract** (`reth` verifier)
This is an EVM blockchain running inside Hyli. Its genesis state root is set to the configured `eth_state_root` at registration time. Every blob transaction that touches this contract must contain a raw EIP-2718 Ethereum transaction as its blob data, together with a proof that bundles a `StatelessInput` (a single-transaction Ethereum block + execution witness) and the chain spec. The verifier runs full stateless EVM execution, checks all receipts succeeded, and transitions the state root from `parent_header.state_root` to `block.header.state_root`. This is how the bridge proves that a `WarpRoute.transferRemote` or `Mailbox.process` call actually executed on an EVM — the EVM execution happens inside the proof, not on any external node.

**`hyperlane-bridge` contract** (`risc0-3` verifier)
A policy guard that runs alongside every reth blob transaction. It has a single action:

- `VerifyTransaction` — reads the EVM transaction from the companion reth blob and enforces chain-level rules. Current policy: **reject contract deployments** (CREATE transactions are not allowed). Future: will also gate EVM-to-Hyli token bridging, so that tokens can be released into native Hyli contracts when the EVM transaction authorises it.

The bridge logic itself (Mailbox, WarpRoute, ISM, token accounting) lives entirely inside the EVM state of the `hyperlane` reth contract as Solidity contracts — exactly as it would on any real EVM chain bridged via Hyperlane. The `hyperlane-bridge` RISC0 contract does not re-implement that logic; it only inspects and guards it.

### Server

A Rust async server (`bridge-server`) composed of four modules:

- **`HylaneRpcProxyModule`** — JSON-RPC endpoint (Ethereum-compatible). Translates `eth_sendRawTransaction` calls from the Hyperlane relayer into Hyli blob transactions. Also stubs `eth_getLogs`, `eth_blockNumber`, `eth_chainId`, `eth_call`, etc.
- **`ContractListener`** — polls the indexer PostgreSQL database for new bridge contract transactions and emits them onto the internal message bus.
- **`AutoProver<BridgeTxHandler, Risc0Prover>`** — batches transactions from the bus and generates RISC0 proofs for the `hyperlane-bridge` contract, then submits proof transactions to the Hyli node.
- **`RestApi`** — HTTP server hosting all module routes, including the RPC proxy and the AutoProver status endpoint.

---

## Data Flows

Every Hyli blob transaction touching the EVM chain has the same two-blob structure:

```
blob[0]: hyperlane reth blob  ← raw EIP-2718 ETH tx (proven by reth verifier)
blob[1]: hyperlane-bridge blob ← VerifyTransaction   (proven by RISC0)
```

The reth proof advances the EVM chain state root. The RISC0 proof checks the policy rules. Both must succeed for the transaction to be accepted.

### Hyli → Ethereum (`WarpRoute.transferRemote`)

```
User
 │
 ├─ sends 2-blob tx:
 │    blob[0]: reth blob  ← EIP-2718 tx calling WarpRoute.transferRemote(dest, recipient, amount)
 │    blob[1]: bridge blob ← VerifyTransaction
 │
 ▼
Hyli node
 ├─ reth verifier: runs stateless EVM block execution, advances EVM state root
 │    WarpRoute.transferRemote() executes inside the EVM, emits Dispatch event ✓
 ├─ RISC0: VerifyTransaction checks the reth blob is not a CREATE tx ✓
 │
 ▼
Hyperlane relayer
 │
 ├─ polls eth_getLogs on RPC proxy
 │    → proxy scans proven reth blobs for transferRemote calldata
 │    → synthesizes Dispatch event log from the calldata parameters
 │
 ├─ delivers proof to Ethereum Mailbox.process()
 │
 ▼
Ethereum: WarpRoute releases tokens to eth_recipient
```

### Ethereum → Hyli (`Mailbox.process`)

```
Hyperlane relayer
 │
 ├─ detects Dispatch on Ethereum, calls eth_sendRawTransaction on RPC proxy
 │    with: raw EIP-2718 tx calling Mailbox.process(metadata, message)
 │
 ▼
RPC proxy (eth_send_raw_transaction handler)
 │
 ├─ validates the bytes decode as a valid EIP-2718 transaction
 │
 ├─ builds 2-blob transaction:
 │    blob[0]: reth blob  ← the original raw EIP-2718 bytes
 │    blob[1]: bridge blob ← VerifyTransaction
 │
 ├─ submits to Hyli node
 │
 ▼
Hyli node
 ├─ reth verifier: runs stateless EVM block execution, advances EVM state root
 │    Mailbox.process() executes inside the EVM, WarpRoute mints/releases tokens ✓
 ├─ RISC0: VerifyTransaction checks the reth blob is not a CREATE tx ✓
 │
 ▼
EVM state updated: tokens credited to recipient inside the EVM chain
```

---

## Server Components

### `HylaneRpcProxyModule` (`server/src/rpc_proxy/`)

An Ethereum JSON-RPC endpoint that the Hyperlane relayer connects to instead of a real Ethereum node.

| RPC method | Behavior |
|---|---|
| `eth_sendRawTransaction` | Wraps the raw EIP-2718 tx in a 2-blob Hyli tx (reth + bridge), submits to node |
| `eth_getLogs` | Scans proven reth blobs for `transferRemote` calldata, synthesizes `Dispatch` event logs |
| `eth_blockNumber` | Proxies Hyli block height |
| `eth_chainId` / `net_version` | Returns configured `hyli_chain_id` |
| `eth_getBlockByNumber` | Returns Hyli block data in Ethereum block format |
| `eth_getTransactionReceipt` | Looks up tx status in indexer |
| `eth_call` | Stubs `latestCheckpoint()` and `threshold()` for Hyperlane validator queries |

### `ContractListener` (`hyli-modules`)

Polls the indexer PostgreSQL database on a configurable interval for new transactions involving the `hyperlane-bridge` contract. Publishes them to the shared bus for the AutoProver to consume.

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
| `token_cn` | `oranj` | Reserved for future EVM-to-Hyli token bridging |
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
2. **`hyperlane-bridge`** — registered with the `risc0-3` verifier, program ID from the compiled ELF, and initial state = SHA-256(`hyperlane_cn`).

The bridge contract's state never changes after initialization (its commit is a hash of the companion reth contract name). The hyperlane contract's state root advances one block at a time as EVM transactions are proven.
