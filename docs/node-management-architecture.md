# Architecture: Node Management

## Overview

Node management uses a daemon-based architecture. A single long-running daemon process manages all
node processes on the machine, exposing a REST API on localhost that the CLI (and any other
frontend) talks to. No OS service managers are used. No admin privileges are required on any
platform.

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

## Key Principles

These align with the `ant-core` design from the Unified CLI project:

1. **No printing, no UI assumptions.** All `ant-core` functions return structured result types.
   Long-running operations report progress through the `ProgressReporter` trait and lifecycle events
   through the `EventListener` trait.

2. **Option structs replace long parameter lists.** Every operation takes an options struct rather
   than 30+ function parameters.

3. **Async-first.** All I/O operations are `async`. The daemon runs on a tokio runtime.

4. **Serializable types.** All result types and status models derive `serde::Serialize` +
   `serde::Deserialize`. This enables `--json` CLI output, REST API responses, and the web status
   console.

5. **Cancellation support.** Long-running operations accept a `CancellationToken` so any frontend
   can abort cleanly.

6. **No OS service managers.** Nodes run as regular processes managed by the daemon. The only
   platform-specific code is process detachment, isolated in one module.

7. **AI agent friendly.** The REST API is the primary integration point for AI agents. Every
   operation available through the CLI is also available through the REST API, including node
   registration (`add`/`remove`). All CLI commands support a `--json` global flag for structured
   output. The API is self-describing via an OpenAPI spec. Real-time events are available via SSE
   for agents that need to react to state changes without polling.

## Crate Responsibilities

### ant-core

All business logic. No UI code, no terminal output.

```
ant-core/src/
├── lib.rs
├── config.rs                 # Platform-appropriate data/log directory paths
├── error.rs                  # Unified error type (thiserror)
├── data/                     # Data storage and retrieval
│   ├── mod.rs                # Re-exports and module declarations
│   ├── error.rs              # Data operation errors
│   ├── network.rs            # P2P network wrapper (DHT, peer discovery)
│   └── client/               # High-level client API
│       ├── mod.rs            # Client, ClientConfig
│       ├── chunk.rs          # chunk_put, chunk_get, chunk_exists
│       ├── data.rs           # data_upload, data_download, data_map_store/fetch
│       ├── file.rs           # file_upload, file_download (streaming)
│       ├── payment.rs        # pay_for_storage, approve_token_spend
│       ├── quote.rs          # get_store_quotes from network peers
│       ├── merkle.rs         # Merkle batch payment (PaymentMode enum)
│       └── cache.rs          # In-memory LRU chunk cache
└── node/                     # Node management
    ├── mod.rs                # add_nodes, remove_node, reset
    ├── types.rs              # DaemonConfig, NodeConfig, AddNodeOpts, etc.
    ├── events.rs             # NodeEvent enum, EventListener trait
    ├── binary.rs             # Binary resolution, ProgressReporter trait
    ├── registry.rs           # Node registry (CRUD, JSON persistence, file locking)
    ├── devnet.rs             # LocalDevnet (local network + Anvil EVM)
    ├── daemon/
    │   ├── mod.rs
    │   ├── client.rs         # Daemon client API (start/stop/status via HTTP)
    │   ├── server.rs         # HTTP server (axum), REST API handlers
    │   └── supervisor.rs     # Process supervision loop
    └── process/
        ├── mod.rs
        ├── spawn.rs          # Spawning node processes
        └── detach.rs         # Platform-specific session detachment
```

The daemon logic lives in `ant-core` so that any frontend (TUI, GUI) could embed a daemon
in-process if desired, rather than being forced to shell out to the `ant` binary.

### ant-cli

Thin adapter layer. Parses arguments, calls `ant-core`, formats output.

```
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
        ├── add.rs            # ant node add
        ├── daemon.rs         # ant node daemon start/stop/status/info/run
        ├── start.rs          # ant node start
        ├── stop.rs           # ant node stop
        ├── status.rs         # ant node status
        └── reset.rs          # ant node reset
```

When the daemon is not running, the CLI calls `ant-core` directly for registry operations (`add`,
`remove`). When the daemon is running, all operations go through the REST API — including `add` and
`remove` — so that AI agents and other HTTP clients have full access to every operation without
needing the CLI.

## Command Structure

```
ant [global flags]
├── file
│   ├── upload <PATH> [--public] [--merkle|--no-merkle]
│   └── download [ADDRESS] [--datamap PATH] [-o PATH]
├── chunk
│   ├── put [FILE]
│   └── get <ADDRESS> [-o PATH]
├── wallet
│   ├── address
│   └── balance
└── node
    ├── daemon start
    ├── daemon stop
    ├── daemon status
    ├── daemon info
    ├── daemon run                        # Hidden; runs daemon in foreground
    ├── add [options]
    ├── start [--service-name NAME]
    ├── stop [--service-name NAME]
    ├── status
    └── reset [--force]
```

The `--json` global flag causes all commands to output structured JSON instead of human-readable
text. This is essential for AI agents and scripts that consume the CLI directly rather than using
the REST API.

## Data Flow: Upload and Download

### File Upload

```
┌─────────┐    read 8KB    ┌──────────────────┐    encrypted    ┌──────────┐
│  Disk    │──────────────▶│  Self-Encryption  │──────chunks───▶│  Client  │
│  (file)  │   streaming   │  (convergent enc) │                │          │
└─────────┘                └──────────────────┘                └──────────┘
                                    │                               │
                                    ▼                               │ for each chunk:
                              DataMap                               │ 1. get quotes
                              (chunk addresses                      │ 2. pay (EVM tx)
                               + encryption keys)                   │ 3. store on peer
                                                                    ▼
                                                           ┌────────────────┐
                                                           │  P2P Network   │
                                                           │  (XOR-routed)  │
                                                           └────────────────┘
```

- Files are streamed in 8KB reads through `self_encryption::stream_encrypt`.
- Each encrypted chunk is content-addressed (XOR hash of its bytes).
- Payment is per-chunk or via a merkle batch (single EVM transaction for many chunks).
- The `DataMap` is the key to reconstruct the file. Store it publicly (on-network) or keep it private (local file).

### File Download

```
┌───────────┐   chunk addresses   ┌──────────┐   requests    ┌────────────────┐
│  DataMap   │──────────────────▶│  Client  │─────────────▶│  P2P Network   │
└───────────┘                    │          │◀─────────────│  (XOR-routed)  │
                                 └──────────┘  encrypted   └────────────────┘
                                      │        chunks
                                      ▼
                              ┌──────────────────┐
                              │  Self-Decryption  │
                              │  (streaming)      │
                              └──────────────────┘
                                      │
                                      ▼
                                 ┌─────────┐
                                 │  Disk    │
                                 │  (file)  │
                                 └─────────┘
```

- Downloads are also streaming: chunks are fetched concurrently and decrypted in batches.
- The file is written incrementally via a temp file, then atomically renamed.

## Node Registry

A JSON file persisted at a platform-appropriate location (e.g.,
`~/.local/share/ant/node_registry.json`). This is the source of truth for which nodes exist and
their configuration.

```json
{
  "schema_version": 1,
  "nodes": {
    "1": {
      "id": 1,
      "service_name": "node1",
      "rewards_address": "0xabc123...",
      "data_dir": "/home/user/.local/share/ant/nodes/node-1",
      "log_dir": "/home/user/.local/share/ant/nodes/node-1/logs",
      "node_port": null,
      "metrics_port": null,
      "network_id": 1,
      "binary_path": "/home/user/.local/share/ant/bin/antnode",
      "version": "0.110.0",
      "env_variables": {},
      "bootstrap_peers": []
    }
  },
  "next_id": 2
}
```

The registry stores configuration only. Runtime state (PID, status, uptime) is held in memory by
the daemon and served via the REST API. This avoids stale state on disk when the daemon isn't
running.

## REST API

All endpoints are on `127.0.0.1:<port>`. The port is selected dynamically and written to a known
file path (`~/.local/share/ant/daemon.port`) so the CLI can discover it.

### Daemon Discovery

AI agents and other HTTP clients need to find the daemon. Two mechanisms:

1. **Port file:** Read the port from `~/.local/share/ant/daemon.port`. The CLI uses this internally.
2. **CLI helper:** `ant node daemon info --json` outputs connection details as JSON, useful for
   agents that shell out to the CLI to bootstrap their connection.

### Endpoints

```
GET    /api/v1/status                    Daemon health, uptime, node count summary
GET    /api/v1/nodes/status              Node status summary (all nodes)
POST   /api/v1/nodes                     Add/register nodes (same as CLI `add`)
DELETE /api/v1/nodes/:id                 Remove a node from the registry
POST   /api/v1/nodes/:id/start           Start a node
POST   /api/v1/nodes/:id/stop            Stop a node
POST   /api/v1/nodes/start-all           Start all registered nodes
POST   /api/v1/nodes/stop-all            Stop all running nodes
POST   /api/v1/reset                     Reset all node state (fails if nodes running)
GET    /api/v1/events                    SSE stream of real-time node events
GET    /api/v1/openapi.json              OpenAPI spec for API self-discovery
GET    /console                          Web status console (static HTML)
```

### Idempotency

Conflict responses (409) include the current state of the resource so that callers can distinguish
between "my request failed" and "the desired state already exists." For example, starting an
already-running node returns:

```json
{
  "error": {
    "code": "NODE_ALREADY_RUNNING",
    "message": "Node 3 is already running"
  },
  "current_state": {
    "node_id": 3,
    "status": "running",
    "pid": 12345,
    "uptime_secs": 3600
  }
}
```

This is critical for AI agents that retry on timeout — they can inspect `current_state` to confirm
the operation already succeeded rather than treating the 409 as a failure.

### Error Envelope

All error responses use a consistent envelope:

```json
{
  "error": {
    "code": "NODE_NOT_FOUND",
    "message": "No node with id 42"
  }
}
```

### OpenAPI Spec

The daemon serves an OpenAPI 3.1 spec at `/api/v1/openapi.json`. This allows AI agents to
self-discover available endpoints, parameter shapes, and response schemas without external
documentation. The spec is generated from the axum route definitions using `utoipa`.

## Process Management

### Spawning

The daemon spawns node processes as children. Each node gets:
- Its configured data directory, log directory, and ports passed as CLI arguments
- Environment variables from the registry
- stdout/stderr redirected to log files in the node's log directory

### Supervision

The daemon runs a supervision loop for each active node:
- Detects process exit via child process handle
- On unexpected exit (crash): restart with exponential backoff (1s, 2s, 4s, ... capped at 60s)
- On repeated crashes (e.g., 5 in 5 minutes): mark node as `errored`, stop retrying, emit event
- Backoff resets after a node runs successfully for a configurable duration

### Auto-Upgrade Handling

The node binary handles its own upgrades and restarts. When this happens:
- The daemon's child process exits
- The node binary spawns a new process (not a child of the daemon)
- The daemon needs to re-discover the node

The exact mechanism for re-discovery is deferred. Options include PID file monitoring, port-based
process lookup, or re-using process discovery code from the old node manager. This will be designed
separately once the core daemon architecture is in place.

### Session Detachment

When the user runs `ant node daemon start`, the CLI spawns the daemon detached from the user
session. This is the only platform-specific code:

- **Linux/macOS:** `setsid` + double-fork, close inherited file descriptors, redirect
  stdin/stdout/stderr to /dev/null (daemon logs to its own file)
- **Windows:** `CREATE_NO_WINDOW` + `DETACHED_PROCESS` creation flags via `std::process::Command`

This code is isolated in `ant-core::node::process::detach`.

## Event System

For the supervisor and long-running operations, `ant-core` emits structured events:

```rust
pub enum NodeEvent {
    NodeStarting { node_id: u32 },
    NodeStarted { node_id: u32, pid: u32 },
    NodeStopping { node_id: u32 },
    NodeStopped { node_id: u32 },
    NodeCrashed { node_id: u32, exit_code: Option<i32> },
    NodeRestarting { node_id: u32, attempt: u32 },
    NodeErrored { node_id: u32, message: String },
    DownloadStarted { version: String },
    DownloadProgress { bytes: u64, total: u64 },
    DownloadComplete { version: String, path: PathBuf },
}

pub trait EventListener: Send + Sync {
    fn on_event(&self, event: NodeEvent);
}
```

The daemon exposes events via Server-Sent Events (SSE) at `GET /api/v1/events`. This is the
primary mechanism for AI agents and other clients to receive real-time updates without polling.
Each SSE message contains a JSON-serialized `NodeEvent` with an event type field for filtering:

```
event: node_started
data: {"node_id": 1, "pid": 12345}

event: node_crashed
data: {"node_id": 2, "exit_code": 1}
```

Clients that don't need real-time updates can poll `GET /api/v1/nodes/status` instead.
