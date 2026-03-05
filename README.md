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
