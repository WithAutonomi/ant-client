# ant — Autonomi Network Client

A unified CLI and library for managing Autonomi network nodes.

## Overview

This project provides:

- **ant-core** — A headless Rust library containing all business logic for node management. Designed to be consumed by any frontend (CLI, TUI, GUI, AI agents).
- **ant-cli** — A CLI binary (`ant`) built on `ant-core`.

The architecture uses a **daemon-based model**: a single long-running process manages all node processes on the machine, exposing a REST API on localhost. No OS service managers or admin privileges are required.

## Installation

```bash
cargo install --path ant-cli
```

## Commands

### `ant node daemon`

Manage the node management daemon.

#### `ant node daemon start`

Launch the daemon as a detached background process.

```
$ ant node daemon start
Daemon started (pid: 12345, port: 48532)
```

The daemon runs in the background, listening on a random port on `127.0.0.1`. The port is written to `~/.local/share/ant/daemon.port` for discovery.

#### `ant node daemon stop`

Shut down the running daemon.

```
$ ant node daemon stop
Daemon stopped (pid: 12345)
```

Sends SIGTERM to the daemon process and waits for it to exit. Cleans up the PID and port files.

#### `ant node daemon status`

Show whether the daemon is running and summary statistics.

```
$ ant node daemon status
Daemon is running
  PID:           12345
  Port:          48532
  Uptime:        3600s
  Nodes total:   3
  Nodes running: 2
  Nodes stopped: 1
  Nodes errored: 0
```

If the daemon is not running:

```
$ ant node daemon status
Daemon is not running
```

#### `ant node daemon info`

Output connection details for programmatic use. Always outputs JSON regardless of the `--json` flag.

```json
{
  "running": true,
  "pid": 12345,
  "port": 48532,
  "api_base": "http://127.0.0.1:48532/api/v1"
}
```

AI agents can use this to bootstrap their connection to the daemon.

### `ant node add`

Register one or more nodes in the node registry. Does **not** start them and does **not** require the daemon to be running.

```
$ ant node add --rewards-address 0xYourWalletAddress --path /path/to/antnode
Added 1 node(s):
  Node 1:
    Data dir: ~/.local/share/ant/nodes/node-1
    Log dir:  ~/.local/share/ant/nodes/node-1/logs
    Port:     (none)
    Binary:   /path/to/antnode
    Version:  0.1.0
```

#### Adding multiple nodes with a port range

```
$ ant node add --rewards-address 0xYourWallet --count 3 --node-port 12000-12002 --path /path/to/antnode
Added 3 node(s):
  Node 1:
    Data dir: ~/.local/share/ant/nodes/node-1
    Port:     12000
  Node 2:
    Data dir: ~/.local/share/ant/nodes/node-2
    Port:     12001
  Node 3:
    Data dir: ~/.local/share/ant/nodes/node-3
    Port:     12002
```

#### Options

| Flag | Description |
|------|-------------|
| `--rewards-address <ADDR>` | Required. Wallet address for node earnings |
| `--count <N>` | Number of nodes to add (default: 1) |
| `--node-port <PORT\|RANGE>` | Port or port range (e.g., `12000` or `12000-12004`) |
| `--metrics-port <PORT\|RANGE>` | Metrics port or range |
| `--data-dir-path <PATH>` | Custom data directory prefix |
| `--log-dir-path <PATH>` | Custom log directory prefix |
| `--network-id <ID>` | Network ID (default: 1 for mainnet) |
| `--path <PATH>` | Path to a local node binary |
| `--version <X.Y.Z>` | Download a specific version (not yet implemented) |
| `--url <URL>` | Download binary from a URL (not yet implemented) |
| `--bootstrap <IP:PORT>` | Bootstrap peer(s), comma-separated |
| `--env <K=V>` | Environment variables, comma-separated |

If the daemon is running, the command routes through its REST API. Otherwise, it operates directly on the registry file.

### `ant node start`

Start registered node(s). Requires the daemon to be running. With no arguments, starts all registered nodes. Use `--service-name` to start a specific node.

```
$ ant node start
Started 3 node(s):
  node1 (1) — PID 12345
  node2 (2) — PID 12346
  node3 (3) — PID 12347
```

#### Starting a specific node

```
$ ant node start --service-name node1
Node node1 (1) started (PID 12345)
```

If the daemon is not running, the command will fail with an error asking you to start it first.

#### Options

| Flag | Description |
|------|-------------|
| `--service-name <NAME>` | Start a specific node by its service name (e.g., `node1`) |

### `ant node stop`

Stop running node(s). Requires the daemon to be running. With no arguments, stops all running nodes. Use `--service-name` to stop a specific node.

```
$ ant node stop
Stopped 3 node(s):
  node1 (1)
  node2 (2)
  node3 (3)
```

#### Stopping a specific node

```
$ ant node stop --service-name node1
Node node1 (1) stopped
```

If the daemon is not running, the command will fail with an error asking you to start it first.

#### Options

| Flag | Description |
|------|-------------|
| `--service-name <NAME>` | Stop a specific node by its service name (e.g., `node1`) |

### `ant node reset`

Remove all node data directories, log directories, and clear the registry, returning the system to a clean state. All nodes must be stopped before running this command.

```
$ ant node reset
This will remove all node data directories, log directories, and clear the registry.
Are you sure? [y/N] y
Reset complete:
  Nodes cleared: 3
  Removed data dir: ~/.local/share/ant/nodes/node-1
  Removed data dir: ~/.local/share/ant/nodes/node-2
  Removed data dir: ~/.local/share/ant/nodes/node-3
  Removed log dir:  ~/.local/share/ant/logs/node-1/logs
  Removed log dir:  ~/.local/share/ant/logs/node-2/logs
  Removed log dir:  ~/.local/share/ant/logs/node-3/logs
```

#### Options

| Flag | Description |
|------|-------------|
| `--force` | Skip the confirmation prompt |

If any nodes are running, the command will fail with an error asking you to stop them first. If the daemon is running, the command routes through its REST API.

### Global Flags

| Flag | Description |
|------|-------------|
| `--json` | Output structured JSON instead of human-readable text |

## REST API

When the daemon is running, it exposes these endpoints on `127.0.0.1:<port>`:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/status` | Daemon health, uptime, node count summary |
| GET | `/api/v1/events` | SSE stream of real-time node events |
| POST | `/api/v1/nodes` | Add one or more nodes to the registry |
| DELETE | `/api/v1/nodes/{id}` | Remove a node from the registry |
| POST | `/api/v1/nodes/{id}/start` | Start a specific node |
| POST | `/api/v1/nodes/start-all` | Start all registered nodes |
| POST | `/api/v1/nodes/{id}/stop` | Stop a specific node |
| POST | `/api/v1/nodes/stop-all` | Stop all running nodes |
| POST | `/api/v1/reset` | Reset all node state (fails if nodes running) |
| GET | `/api/v1/openapi.json` | OpenAPI 3.1 specification |

## Development

```bash
# Build
cargo build

# Run tests
cargo test --all

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Format
cargo fmt --all

# Run the CLI
cargo run --bin ant -- node daemon status
```

## Project Structure

```
├── ant-core/           # Headless library — all business logic
│   ├── src/
│   │   ├── config.rs   # Platform-appropriate paths
│   │   ├── error.rs    # Unified error type
│   │   └── node/       # Node management modules
│   └── tests/          # Integration tests
├── ant-cli/            # CLI binary (thin adapter layer)
│   └── src/
│       ├── cli.rs      # clap argument definitions
│       └── commands/   # Command handlers
└── docs/               # Architecture documentation
```

## License

TBD
