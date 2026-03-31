# CLAUDE.md

This is the development guide for the `autonomi-client` project (the Unified CLI + `ant-core` library).

## Project Overview

A Rust workspace with two crates:
- **ant-core** (`ant-core/`) — Headless library containing all business logic. No UI code, no terminal output.
- **ant-cli** (`ant-cli/`) — Thin CLI adapter. Parses arguments via clap, calls `ant-core`, formats output. The binary is named `ant`.

## Design Principles

These are non-negotiable. Every contribution must follow them.

### 1. No printing, no UI assumptions in ant-core
All `ant-core` functions return structured result types. Long-running operations report progress through the `ProgressReporter` trait and lifecycle events through the `EventListener` trait. Zero `println!`, `eprintln!`, or direct terminal output.

### 2. Option structs replace long parameter lists
Every operation takes an options struct rather than many function parameters. This keeps APIs ergonomic and forward-compatible.

### 3. Async-first
All I/O operations are `async`. The daemon runs on a tokio runtime.

### 4. Serializable types
All result types and status models derive `serde::Serialize` + `serde::Deserialize`. This enables `--json` CLI output, REST API responses, and frontend integration.

### 5. Cancellation support
Long-running operations accept a `CancellationToken` so any frontend can abort cleanly.

### 6. No OS service managers
Nodes run as regular processes managed by the daemon. The only platform-specific code is process detachment, isolated in `ant-core::node::process::detach`.

### 7. AI agent friendly
The REST API is the primary integration point for AI agents. All operations available through the CLI are also available through the REST API. The API is self-describing via OpenAPI spec at `/api/v1/openapi.json`. Real-time events are available via SSE.

## Architecture

```
┌──────────┐     HTTP      ┌──────────────────────────────────┐
│  ant CLI │──────────────▶│         ant daemon                │
└──────────┘  127.0.0.1    │                                  │
                           │  ┌────────────┐ ┌────────────┐  │
┌──────────┐     HTTP      │  │  antnode 1  │ │  antnode 2  │  │
│  Web UI  │──────────────▶│  └────────────┘ └────────────┘  │
└──────────┘               │  ┌────────────┐ ┌────────────┐  │
                           │  │  antnode 3  │ │  antnode N  │  │
┌──────────┐     HTTP      │  └────────────┘ └────────────┘  │
│ AI Agent │──────────────▶│                                  │
└──────────┘               └──────────────────────────────────┘
                                       │
                                       ▼
                              node_registry.json
```

The daemon is a long-running process that manages all node processes on the machine, exposing a REST API on localhost. No admin privileges required on any platform.

## Crate Structure

```
ant-core/src/
├── lib.rs
├── error.rs                  # Unified error type (thiserror)
├── config.rs                 # Platform-appropriate data/log directory paths
├── data/                     # Data storage and retrieval
│   ├── mod.rs                # Re-exports (Client, DataMap, Wallet, PaymentMode, etc.)
│   ├── error.rs              # Data operation errors
│   ├── network.rs            # P2P network wrapper (DHT, peer discovery)
│   └── client/               # High-level client API
│       ├── mod.rs            # Client, ClientConfig
│       ├── chunk.rs          # chunk_put, chunk_get, chunk_exists
│       ├── data.rs           # data_upload, data_download, data_map_store/fetch
│       ├── file.rs           # file_upload, file_download (streaming self-encryption)
│       ├── payment.rs        # pay_for_storage, approve_token_spend
│       ├── quote.rs          # get_store_quotes from network peers
│       ├── merkle.rs         # Merkle batch payment (PaymentMode enum)
│       └── cache.rs          # In-memory LRU chunk cache
└── node/                     # Node management
    ├── mod.rs                # add_nodes, remove_node, reset
    ├── types.rs              # DaemonConfig, DaemonStatus, NodeConfig, NodeInfo, AddNodeOpts, etc.
    ├── events.rs             # NodeEvent enum, EventListener trait
    ├── binary.rs             # Binary resolution (download/cache/validate), ProgressReporter trait
    ├── registry.rs           # Node registry (CRUD, JSON persistence, file locking)
    ├── devnet.rs             # LocalDevnet (local network + Anvil EVM blockchain)
    ├── daemon/
    │   ├── mod.rs
    │   ├── client.rs         # Daemon client API (start/stop/status via HTTP)
    │   ├── server.rs         # HTTP server (axum), REST API handlers
    │   └── supervisor.rs     # Process supervision with backoff
    └── process/
        ├── mod.rs
        ├── spawn.rs          # Spawning node processes
        └── detach.rs         # Platform-specific session detachment

ant-cli/src/
├── main.rs                   # Entry point, client/wallet/EVM initialization
├── cli.rs                    # Top-level clap definition
└── commands/
    ├── data/
    │   ├── file.rs           # ant file upload/download
    │   ├── chunk.rs          # ant chunk put/get
    │   └── wallet.rs         # ant wallet address/balance
    └── node/
        ├── mod.rs
        ├── add.rs            # ant node add command
        ├── daemon.rs         # daemon start/stop/status/info/run commands
        ├── start.rs          # ant node start
        ├── stop.rs           # ant node stop
        ├── status.rs         # ant node status
        └── reset.rs          # ant node reset
```

## Common Commands

```bash
cargo check              # Fast compilation check
cargo test --all         # Run all unit and integration tests
cargo clippy --all-targets --all-features -- -D warnings  # Lint
cargo fmt --all -- --check  # Format check
cargo run --bin ant -- --help  # Run the CLI
```

## Key Conventions

- **Error handling**: Use `thiserror` in ant-core, `anyhow` in ant-cli. ant-core defines `error::Error` and `error::Result<T>`.
- **CLI output**: All commands support `--json` global flag. Human-readable output by default, structured JSON when `--json` is passed.
- **Hidden commands**: `ant node daemon run` is hidden (runs daemon in foreground, used internally by `daemon start`).
- **REST API conflict responses**: All 409 responses include a `current_state` field so retrying clients can confirm desired state already exists.
- **Platform paths**: Use `ant_core::config::data_dir()` and `ant_core::config::log_dir()` for platform-appropriate paths. Never hardcode paths.
- **OpenAPI schema**: Types exposed in the REST API derive `utoipa::ToSchema`. Use `#[schema(value_type = String)]` for `PathBuf` fields.
- **Tests**: Unit tests go in `#[cfg(test)] mod tests` inside the source file. Integration tests go in `ant-core/tests/`.
- **Progress reporting**: Long-running operations (e.g., binary downloads) accept a `&dyn ProgressReporter` from `ant_core::node::binary`. CLI provides a terminal-printing implementation; use `NoopProgress` in tests and daemon API handlers.
- **Registry file locking**: Use `NodeRegistry::load_locked()` for read-modify-write operations to prevent concurrent CLI invocations from corrupting the registry. The returned `File` handle holds the lock until dropped.
- **Dual-path CLI commands**: Commands that modify the registry (like `ant node add`) check if the daemon is running. If so, they route through the REST API; otherwise, they operate directly on the registry file.
- **Binary source resolution**: Node binary sources are represented by the `BinarySource` enum (Latest, Version, Url, LocalPath). Download variants are stubbed until release infrastructure is available.

## E2E Test Skill

The project includes an E2E test skill at `.claude/skills/e2e-node-management-test/SKILL.md` that tests all node management features against a real testnet (invoked via `/e2e-node-management-test`). When adding new `ant node` subcommands or changing existing node management behavior, update this command to cover the new or changed functionality.

## Linear Project

This project tracks work under the "Unified CLI" project in Linear (team: v2.0).
