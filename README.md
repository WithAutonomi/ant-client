# ant — Autonomi Network Client

A unified CLI and Rust library for storing data on the Autonomi decentralized network and managing Autonomi nodes.

## Overview

This project provides two crates:

- **ant-core** — A headless Rust library containing all business logic: data storage/retrieval with self-encryption and EVM payments, node lifecycle management, and local devnet tooling. Designed to be consumed by any frontend (CLI, GUI, AI agents, REST clients).
- **ant-cli** — A thin CLI binary (`ant`) built on `ant-core`.

Data on Autonomi is **content-addressed**. Files are split into encrypted chunks (via [self-encryption](https://en.wikipedia.org/wiki/Convergent_encryption)), each stored at an XOR address derived from its content. A `DataMap` tracks which chunks belong to a file. Payments for storage are made on an EVM-compatible blockchain (Arbitrum).

## Installation

### Linux / macOS

```bash
curl -fsSL https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.sh | bash
```

### Windows

```powershell
irm https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.ps1 | iex
```

> **Note:** the `curl ... | bash` installer is for Linux/macOS only. Running it inside WSL installs a Linux binary that is only usable from within WSL — to use `ant` from PowerShell or cmd, install with the PowerShell command above.

Both installers take the same environment variables:

| Variable | Description |
|----------|-------------|
| `ANT_CHANNEL` | Release channel: `stable` (default) or `beta`. See [Beta Programme](#beta-programme). |
| `ANT_VERSION` | Install one specific version, e.g. `0.3.3`. Overrides `ANT_CHANNEL`. |
| `INSTALL_DIR` | Where to put the binary. Defaults to `~/.local/bin` on Linux, `/usr/local/bin` on macOS, `%LOCALAPPDATA%\ant\bin` on Windows. |

### npm

`ant` is also on npm, which is useful in agent sandboxes and CI runners that allow
package-manager traffic but block direct binary downloads:

```bash
npm install -g @withautonomi/ant
```

This installs the same signed binary as the release page — a per-platform companion package
carries it, so only the one your machine needs is downloaded. Use `@withautonomi/ant@beta` for
the beta channel, and `npm update -g @withautonomi/ant` to upgrade; `ant update` recognises an
npm-managed install and points you at npm rather than replacing itself.

> **Note:** npm 12 blocks package install scripts by default, so an npm install may not write
> `bootstrap_peers.toml` into your config directory. `ant` works either way — it falls back to
> the peer list compiled into the binary — but if you want the editable file, install with
> `npm install -g --allow-scripts=@withautonomi/ant @withautonomi/ant`. See
> [`npm/README.md`](npm/README.md).

## Quick Start

### Store and retrieve a file (production)

```bash
# Upload (private — DataMap saved locally)
SECRET_KEY=0x... ant file upload photo.jpg -b 1.2.3.4:12000
# Output:
#   Upload complete!
#     Datamap: photo.datamap
#     Chunks:  3

# Download using the local DataMap
ant file download --datamap photo.datamap -o photo_copy.jpg -b 1.2.3.4:12000
```

### Store and retrieve a file (local devnet)

```bash
# 1. Start a local devnet (spins up 25 nodes + a local Anvil EVM chain)
cargo run --release --example start-local-devnet

# 2. Upload a file (the manifest is auto-written to the shared data dir)
SECRET_KEY=0x... ant file upload photo.jpg --public \
    --devnet-manifest ~/.local/share/ant/devnet-manifest.json --allow-loopback --evm-network local

# 3. Download it back
ant file download abc123... -o photo_copy.jpg \
    --devnet-manifest ~/.local/share/ant/devnet-manifest.json --allow-loopback --evm-network local
```

### Store and retrieve a file (Arbitrum mainnet)

```bash
# Upload (private — DataMap saved locally)
SECRET_KEY=0x... ant file upload photo.jpg \
    --bootstrap 1.2.3.4:12000 --evm-network arbitrum-one
# Output: DATAMAP_FILE=photo.jpg.datamap

# Download using the local DataMap
ant file download --datamap photo.jpg.datamap -o photo_copy.jpg \
    --bootstrap 1.2.3.4:12000 --evm-network arbitrum-one
```

### Low-level chunk operations

```bash
# Store a single chunk (< 1 MB)
echo "hello autonomi" | SECRET_KEY=0x... ant chunk put --bootstrap ...
# Output: abc123def456...

# Retrieve it
ant chunk get abc123def456... --bootstrap ...
# Output: hello autonomi
```

---

## CLI Reference

### Global Flags

| Flag | Description |
|------|-------------|
| `--json` | Output structured JSON instead of human-readable text |
| `-b, --bootstrap <IP:PORT>` | Bootstrap peer addresses, comma-separated or repeated (`-b 1.2.3.4:10000,5.6.7.8:10000`) |
| `--devnet-manifest <PATH>` | Path to devnet manifest JSON file |
| `--allow-loopback` | Allow loopback connections (required for local devnet) |
| `--timeout-secs <N>` | Network operation timeout in seconds (default: 60) |
| `-v, --verbose` | Increase verbosity: `-v` info, `-vv` debug, `-vvv` trace. Default: no logs (privacy by design) |
| `--evm-network <NET>` | EVM network: `arbitrum-one` (default), `arbitrum-sepolia`, or `local` |

### `ant file` — File Operations

Upload and download files with automatic chunking, self-encryption, and EVM payment.

#### `ant file upload <PATH>`

Upload a file to the network. The file is split into encrypted chunks, each paid for via the configured EVM network. Requires `SECRET_KEY` environment variable.

```
$ SECRET_KEY=0x... ant file upload my_data.bin --public
Connecting to network... done
Approving token spend... done
Uploading my_data.bin (439.5 KB)...
Storing public data map... done

Upload complete!
  Address: a1b2c3d4e5f6...
  Chunks:  7
  Size:    439.5 KB
  Time:    12.3s

Anyone can download this file with:
  ant file download a1b2c3d4e5f6...
```

**Options:**

| Flag | Description |
|------|-------------|
| `--public` | Store the DataMap on-network (anyone with the address can download). Without this flag, the DataMap is saved to a local `.datamap` file (private). |
| `--merkle` | Force merkle batch payment (single EVM transaction for all chunks). Reduces gas costs for multi-chunk uploads. |
| `--no-merkle` | Disable merkle, always use per-chunk payments. |

**How it works:**
1. The file is streamed through self-encryption in 8KB reads (never fully loaded into memory).
2. Each encrypted chunk is stored on the network at its XOR content address.
3. Payment is made per-chunk or via a merkle batch transaction (auto-selected by default when >= 64 chunks).
4. A `DataMap` is produced that records which chunks compose the file.
5. In `--public` mode, the DataMap itself is stored as a chunk; the returned address is the DataMap's content address. In private mode, the DataMap is saved to `<filename>.datamap` on disk.

#### `ant file download [ADDRESS]`

Download a file from the network.

```
# Public download (by address)
$ ant file download a1b2c3d4e5f6... -o restored.bin
Connecting to network... done
Downloading from network...
Download complete!
  File: restored.bin
  Size: 439.5 KB
  Time: 3.2s

# Private download (from local DataMap)
$ ant file download --datamap my_data.bin.datamap -o restored.bin
Connecting to network... done
Downloading from network...
Download complete!
  File: restored.bin
  Size: 439.5 KB
  Time: 2.8s
```

**Options:**

| Flag | Description |
|------|-------------|
| `ADDRESS` | Hex-encoded public DataMap address (64 hex chars). |
| `--datamap <PATH>` | Path to a local `.datamap` file (for private downloads). |
| `-o, --output <PATH>` | Output file path (default: `downloaded_file`). |

### `ant chunk` — Single-Chunk Operations

Low-level put/get for individual chunks (max ~1 MB each). Useful for small data or building custom data structures.

#### `ant chunk put [FILE]`

Store a single chunk. Reads from `FILE` or stdin. Requires `SECRET_KEY`.

```
$ echo "small payload" | SECRET_KEY=0x... ant chunk put
a1b2c3d4...
```

#### `ant chunk get <ADDRESS>`

Retrieve a single chunk by its hex-encoded XOR address.

```
$ ant chunk get a1b2c3d4... -o output.bin
```

**Options:**

| Flag | Description |
|------|-------------|
| `-o, --output <PATH>` | Write to file instead of stdout. |

### `ant wallet` — Wallet Operations

Inspect the EVM wallet derived from `SECRET_KEY`.

#### `ant wallet address`

Print the wallet's EVM address.

```
$ SECRET_KEY=0x... ant wallet address
0x1234567890abcdef...
```

#### `ant wallet balance`

Query the token balance on the configured EVM network.

```
$ SECRET_KEY=0x... ant wallet balance --evm-network arbitrum-one
1000000000000000000
```

### `ant node` — Node Management

Manage Autonomi network nodes via a local daemon process. The daemon runs in the background, exposes a REST API on `127.0.0.1`, and supervises all node processes.

#### `ant node daemon start`

Launch the daemon as a detached background process. By default it binds to a random free port on `127.0.0.1` and writes the chosen port to `daemon.port` for discovery.

```
$ ant node daemon start
Daemon started (pid: 12345, port: 48532)
```

**Options:**

| Flag | Description |
|------|-------------|
| `--port <PORT>` | Pin the HTTP port. `0` means OS-assigned (the default behavior). |
| `--listen-addr <IP>` | Bind address. Defaults to `127.0.0.1`. |

Pin the port and bind on all interfaces — useful when the daemon runs inside a container and the API needs to be reachable through a port mapping:

```
$ ant node daemon start --listen-addr 0.0.0.0 --port 8765
```

```
$ docker run -d -p 8765:8765 my/ant-image \
    ant node daemon start --listen-addr 0.0.0.0 --port 8765
```

> **Warning:** the daemon has no authentication. Binding to a non-loopback address exposes node management — start, stop, reset, registry mutation — to anyone who can reach the port. Only do this when the network path is controlled (e.g. a container with an explicit port mapping or a trusted private network).

#### `ant node daemon stop`

Shut down the running daemon. Sends SIGTERM and waits for exit.

```
$ ant node daemon stop
Daemon stopped (pid: 12345)
```

#### `ant node daemon status`

Show daemon status and node count summary.

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

#### `ant node daemon info`

Output connection details as JSON (always JSON, regardless of `--json` flag). AI agents use this to discover the daemon's REST API.

```json
{
  "running": true,
  "pid": 12345,
  "port": 48532,
  "api_base": "http://127.0.0.1:48532/api/v1"
}
```

#### `ant node add`

Register one or more nodes in the registry. Does **not** start them. Does **not** require the daemon.

```
$ ant node add --rewards-address 0xYourWallet --count 3 --node-port 12000-12002 --path /path/to/antnode
Added 3 node(s):
  Node 1: port 12000
  Node 2: port 12001
  Node 3: port 12002
```

If the daemon is running, the command routes through its REST API. Otherwise, it operates directly on the registry file.

**Options:**

| Flag | Description |
|------|-------------|
| `--rewards-address <ADDR>` | Required. EVM wallet address for node earnings. |
| `--count <N>` | Number of nodes to add (default: 1). |
| `--node-port <PORT\|RANGE>` | Port or range (e.g., `12000` or `12000-12004`). |
| `--data-dir-path <PATH>` | Custom data directory prefix. |
| `--log-dir-path <PATH>` | Custom log directory prefix. |
| `--path <PATH>` | Path to a local `antnode` binary. |
| `--version <X.Y.Z>` | Download a specific version (accepts pre-releases, e.g. `0.17.0-beta.1`). |
| `--url <URL>` | Download binary from a URL archive. |
| `--bootstrap <IP:PORT>` | Bootstrap peer(s), comma-separated. |
| `--evm-network <NET>` | EVM network for storage payments: `arbitrum-one` (default) or `arbitrum-sepolia`. |
| `--upgrade-channel <CHAN>` | Release channel the node tracks for automatic upgrades: `stable` (default) or `beta`. Also selects which release the binary is downloaded from when no `--path`/`--version`/`--url` is given. |
| `--env <K=V>` | Environment variables, comma-separated. |

With none of `--path`, `--version` or `--url`, the binary is downloaded from the newest release
eligible for the chosen channel: the latest stable release on `stable`, and the highest of the
`-beta.N` and stable releases on `beta`. Release candidates (`-rc.N`) are never installed on
either channel — they are cut before the release gates have given a verdict.

#### `ant node start`

Start registered node(s). Requires the daemon to be running.

```
$ ant node start                          # Start all
$ ant node start --service-name node1     # Start specific node
```

#### `ant node stop`

Stop running node(s). Requires the daemon.

```
$ ant node stop                           # Stop all
$ ant node stop --service-name node1      # Stop specific node
```

#### `ant node status`

Display all nodes and their current status.

#### `ant node reset`

Remove all node data, log directories, and clear the registry. All nodes must be stopped first.

```
$ ant node reset --force
```

#### `ant node logs forward`

Optionally ship this machine's node logs to the Autonomi beta log endpoint, to help judge a beta
build. Off by default and never enabled for you — running `enable` yourself is the whole of the
consent, and `disable` stops it. See
[Forwarding your node logs](#forwarding-your-node-logs-optional) for the walkthrough.

```
$ ant node logs forward enable --token <token>
$ ant node logs forward status
$ ant node logs forward disable
```

**`enable` options:**

| Flag | Description |
|------|-------------|
| `--token <TOKEN>` | The write-only token issued with your beta enrolment. Only needed the first time; re-enabling after a `disable` reuses the stored one. |
| `--endpoint <URL>` | Send somewhere other than the default endpoint. Intended for testing against a local sink. |
| `--level <LEVEL>` | Lowest level to forward: `trace`, `debug`, `info`, `warn`, `error`. Defaults to `info`. |

`status` reports whether forwarding is on, which nodes are being tailed, which are being skipped and
why, and how many events have been delivered:

```
$ ant node logs forward status
Log forwarding: on
  Endpoint: https://logs.autonomi.com   Level: INFO and above
  Token:    75f57160d2c2

  Forwarding (1)
    ● node1 (1)

  Not forwarding (2)
    ○ node2 (2) — logging is not enabled for this node — re-add it with --log-dir-path to forward its logs
    ○ node3 (3) — logging is not enabled for this node — re-add it with --log-dir-path to forward its logs

  Node logging is off unless a node was added with --log-dir-path.

  Delivery
    Forwarded: 1284   Batches: 7 sent, 0 failed
```

The token is stored at `~/.config/ant/log_forward.json` with owner-only permissions and is never
printed back or returned by the API — `status` shows a short fingerprint of it instead, enough to
tell which token is in use.

### `ant update` — Self-Update

Replace the running `ant` binary with the newest release from GitHub. The downloaded archive's
ML-DSA-65 signature is verified against the embedded release signing key before anything is
installed, and the extracted binary must report the expected version.

```
$ ant update
Current version: 0.3.3
Checking for updates on the stable channel...
Update available: v0.3.3 -> v0.3.4
Updated successfully: v0.3.3 -> v0.3.4
```

**Options:**

| Flag | Description |
|------|-------------|
| `--channel <CHAN>` | Release channel to update along: `stable` or `beta`. Defaults to the channel the running binary belongs to. |
| `--force` | Re-download and reinstall even when already on the newest release. |

The default channel is inferred from the running binary's own version, so there is nothing to
configure: a `-beta.N` build tracks `beta` and every other build tracks `stable`. On `beta`,
both `-beta.N` and stable releases are accepted and the highest wins; `-rc.N` releases are
rejected on both channels.

Because a beta version outranks the stable release it was cut from, `--channel stable` cannot
walk a beta build backwards — it will report that you are already up to date. Leaving the beta
channel means installing a stable build manually.

---

## Beta Programme

The beta channel carries the week's build ahead of the stable train, for people who want to soak it
and report back.

Taking part means [running beta builds](#running-beta-builds). That is the whole requirement.

Separately, and entirely optionally, you can [forward your node logs](#forwarding-your-node-logs-optional)
to us. It is off by default, it is not a condition of being on the beta channel, and choosing not to
turn it on costs you nothing.

### Running beta builds

The client and the nodes have separate beta channels, opted into separately.

```bash
# 1. Install the beta client. Pass ANT_CHANNEL=beta to the quick-start installer:
$ curl -fsSL https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.sh \
      | ANT_CHANNEL=beta bash

#    On Windows:
#      $env:ANT_CHANNEL="beta"; irm https://raw.githubusercontent.com/WithAutonomi/ant-client/main/install.ps1 | iex

# 2. From then on, self-update stays on beta with no flag needed.
$ ant update

# 3. Add nodes that track the beta channel. The binary is downloaded from the newest
#    beta-eligible ant-node release.
$ ant node add --rewards-address 0xYourWallet --count 3 --upgrade-channel beta
```

Existing nodes are not switched to the beta channel by any of this — `--upgrade-channel` applies
to nodes at the point they are added, so opting in means adding new nodes.

### Forwarding your node logs (optional)

**This is opt-in and stays off until you ask for it.** Nothing leaves your machine unless you run
`ant node logs forward enable` yourself, and running it is the whole of the consent. You can skip
this section entirely and still run beta builds.

If you do want to help: beta enrolment comes with a write-only token, and handing it to that command
makes the daemon tail your nodes' log files and ship them to the Autonomi beta log endpoint. It is
the difference between us judging a build on a handful of self-reported problems and judging it on
what the nodes actually did, so it is genuinely useful — but it is your call, and `disable` stops it
at any point.

**If you do turn it on, the order of these three steps matters:**

```bash
# 1. Add nodes WITH LOGGING ENABLED. Node file logging is off by default; without
#    --log-dir-path a node writes no log files at all and there is nothing to forward.
$ ant node add --rewards-address 0xYourWallet --count 3 \
      --upgrade-channel beta --log-dir-path ~/.local/share/ant

# 2. Enable forwarding BEFORE starting the nodes.
$ ant node logs forward enable --token <your-token>

# 3. Now start them.
$ ant node start
```

#### Why the order matters

**`--log-dir-path` has to be set when the node is added.** The value is a prefix — each node gets
`<prefix>/node-<id>/logs` — and there is no way to turn logging on for an existing node short of
removing it and adding it again. If you forget, `enable` will tell you —
those nodes appear under "Not forwarding" in `ant node logs forward status` — but the node has to be
re-added to fix it.

**Enable before you start, not after.** Enabling forwarding is forward-looking consent: for any log
file that *already exists*, the daemon starts reading from the end of it, so nothing written before
you opted in is ever uploaded. A node that has not run yet has no log file, so the one it creates
when it starts is read from its first line.

The practical difference is the node's startup line, which is where its version, commit and peer ID
are recorded, along with the bootstrap and listen-address lines that show whether it actually joined
the network. Enable first and those are captured. Enable afterwards and they are skipped, so every
event forwarded from that run is missing the fields that identify which build produced it — which is
most of what makes the logs useful.

If you have already started a node, restarting it does not necessarily help: the node appends to the
same daily file, so a restart on the same day resumes from where the daemon had got to rather than
from the new run's first line.

#### What gets sent

Worth knowing before you decide. Only events at `INFO` and above, from nodes with logging enabled.
Every event is tagged with the node ID, service name, binary version, release channel, and the OS
and architecture of the machine.

Each event also carries a random identifier generated when you first enable forwarding, stored in
`~/.config/ant/log_forward.json`. Because every participant writes into the same daily index, it is
there to stop two machines' events colliding and overwriting one another. It is generated from
random bytes — not from your hostname, MAC address or username — so it distinguishes your
installation from others without describing it.

Your machine's hostname is **not** sent. Your wallet and rewards address are not part of what the
daemon adds. Beyond those tags, the content is whatever `ant-node` itself wrote to its log at `INFO`
or above — the same lines you can read yourself in the node's log directory.

#### If something looks wrong

`ant node logs forward status` is the place to look. `Batches: N sent, M failed` with an error line
underneath means the endpoint is unreachable or rejecting the token. Delivery is best-effort by
design: it is bounded in memory, it retries a few times and then gives up on a batch, and it never
blocks or slows a node. Losing some log lines is an acceptable outcome; a stalled node is not.

`disable` takes effect immediately: it waits for any request already in flight to be abandoned
before reporting that forwarding has stopped, so nothing is still being uploaded once the command
returns.

Forwarding survives a daemon restart, picking up where it left off without re-sending what it had
already delivered. If you want a genuinely clean slate, `disable` first, then delete
`~/.local/share/ant/log_forward_offsets.json`.

### Turning forwarding off

You can stop forwarding at any time, without leaving the beta channel and without giving a reason:

```bash
$ ant node logs forward disable
```

Nothing else about your nodes changes — no restart, no change to how they run, no data touched. They
keep writing their own logs to disk exactly as before; the daemon just stops reading them.

### Leaving the beta channel

Separate from the above, and manual. Because a beta version outranks the stable release it was cut
from, `ant update --channel stable` cannot walk a beta build backwards — installing a stable build
means downloading it yourself. Nodes already on `--upgrade-channel beta` stay on it.

---

## REST API

When the daemon is running, it exposes a REST API on `127.0.0.1:<port>`. Discover the port via `ant node daemon info` or by reading `~/.local/share/ant/daemon.port`.

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/v1/status` | Daemon health, uptime, node count summary |
| GET | `/api/v1/events` | SSE stream of real-time node events |
| GET | `/api/v1/nodes/status` | Node status summary |
| POST | `/api/v1/nodes` | Add nodes to the registry |
| DELETE | `/api/v1/nodes/{id}` | Remove a node |
| POST | `/api/v1/nodes/{id}/start` | Start a specific node |
| POST | `/api/v1/nodes/start-all` | Start all registered nodes |
| POST | `/api/v1/nodes/{id}/stop` | Stop a specific node |
| POST | `/api/v1/nodes/stop-all` | Stop all running nodes |
| POST | `/api/v1/reset` | Reset all node state (fails if nodes running) |
| GET | `/api/v1/logs/forward` | Beta log-forwarding status, tailed nodes, delivery counters |
| POST | `/api/v1/logs/forward/enable` | Enable beta log forwarding |
| POST | `/api/v1/logs/forward/disable` | Disable beta log forwarding |
| GET | `/api/v1/openapi.json` | OpenAPI 3.1 specification |
| GET | `/console` | Web status console (HTML) |

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

### Idempotency

409 Conflict responses include `current_state` so retrying clients can confirm the desired state already exists:

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

### SSE Events

`GET /api/v1/events` streams real-time node lifecycle events:

```
event: node_started
data: {"node_id": 1, "pid": 12345}

event: node_crashed
data: {"node_id": 2, "exit_code": 1}
```

Event types: `node_starting`, `node_started`, `node_stopping`, `node_stopped`, `node_crashed`, `node_restarting`, `node_errored`, `download_started`, `download_progress`, `download_complete`.

---

## Rust Library API (`ant-core`)

The `ant-core` crate exposes the full API programmatically. Add it as a dependency:

```toml
[dependencies]
ant-core = { path = "ant-core" }
```

### Connecting to the Network

```rust
use ant_core::data::{Client, ClientConfig};

// Connect to bootstrap peers
let client = Client::connect(&["1.2.3.4:12000".parse()?], ClientConfig::default()).await?;

// Attach a wallet for paid operations (uploads)
use ant_core::data::{Wallet, EvmNetwork};
let wallet = Wallet::new_from_private_key(EvmNetwork::ArbitrumOne, "0xprivate_key...")?;
let client = client.with_wallet(wallet);
client.approve_token_spend().await?;
```

### Uploading and Downloading Files

```rust
use std::path::Path;
use ant_core::data::PaymentMode;

// Upload a file (streamed, never fully loaded into memory)
let result = client.file_upload(Path::new("photo.jpg")).await?;
println!("Stored {} chunks", result.chunks_stored);

// Upload with explicit payment mode
let result = client.file_upload_with_mode(Path::new("photo.jpg"), PaymentMode::Merkle).await?;

// Store DataMap publicly (anyone with the address can download)
let public_address = client.data_map_store(&result.data_map).await?;

// Download a public file
let data_map = client.data_map_fetch(&public_address).await?;
client.file_download(&data_map, Path::new("photo_copy.jpg")).await?;
```

### Uploading and Downloading In-Memory Data

```rust
use bytes::Bytes;

// Upload bytes (encrypted + chunked automatically)
let result = client.data_upload(Bytes::from("hello autonomi")).await?;

// Download and decrypt
let content = client.data_download(&result.data_map).await?;
assert_eq!(content, Bytes::from("hello autonomi"));
```

### Low-Level Chunk Operations

```rust
use ant_core::data::XorName;

// Store a single chunk (< MAX_CHUNK_SIZE bytes)
let address: XorName = client.chunk_put(Bytes::from("small data")).await?;

// Retrieve it
if let Some(chunk) = client.chunk_get(&address).await? {
    println!("Got {} bytes", chunk.content.len());
}

// Check existence without downloading
let exists: bool = client.chunk_exists(&address).await?;
```

### Payment Modes

Autonomi supports two payment strategies:

| Mode | Description |
|------|-------------|
| `PaymentMode::Single` | One EVM transaction per chunk. Simple but more gas for many chunks. |
| `PaymentMode::Merkle` | Single EVM transaction for a batch of chunks via merkle proof. Lower gas for large uploads. |
| `PaymentMode::Auto` | Default. Uses merkle when chunk count >= 64, otherwise per-chunk. |

### Local Development with Devnet

Two ready-made examples spin up a local network and write a manifest to the shared data directory (`~/.local/share/ant/` on Linux, `~/Library/Application Support/ant/` on macOS, `%APPDATA%\ant\` on Windows). Any consumer that checks this directory — ant-gui, ant-cli, ant-tui — will auto-detect the devnet.

```bash
# Local Anvil devnet (25 nodes + embedded EVM blockchain)
# Includes a pre-funded wallet key in the manifest
cargo run --release --example start-local-devnet

# Sepolia testnet devnet (25 nodes + real Arbitrum Sepolia contracts)
# No wallet key — connect your own funded Sepolia wallet
cargo run --release --example start-devnet-sepolia
```

Both write `devnet-manifest.json` to the shared data dir and clean it up on Ctrl+C.

The [ant-gui](https://github.com/WithAutonomi/ant-ui) desktop app auto-detects the manifest on startup and switches to devnet/Sepolia mode — no manual configuration needed. The manifest can also be passed to the CLI explicitly via `--devnet-manifest <path>`.

#### Programmatic usage

```rust
use ant_core::data::LocalDevnet;

// Start a devnet (25 nodes + Anvil EVM chain)
let devnet = LocalDevnet::start(DevnetConfig::default()).await?;

// Create a client with a pre-funded wallet (ready to upload)
let client = devnet.create_funded_client().await?;

// Write manifest for discovery by other tools
let manifest_path = ant_core::config::data_dir()?.join("devnet-manifest.json");
devnet.write_manifest(&manifest_path).await?;

// Clean up
devnet.shutdown().await?;
```

### Chunk Cache

The client includes an in-memory LRU cache for recently accessed chunks:

```rust
let cache = client.chunk_cache();
cache.put(address, content);
if let Some(data) = cache.get(&address) { /* ... */ }
```

### Node Management (Programmatic)

```rust
use ant_core::node::{add_nodes, AddNodeOpts, BinarySource};
use ant_core::node::binary::NoopProgress;
use ant_core::config::data_dir;

let opts = AddNodeOpts {
    count: 3,
    rewards_address: "0xYourWallet".to_string(),
    binary_source: BinarySource::LocalPath("/path/to/antnode".into()),
    ..Default::default()
};

let result = add_nodes(opts, &data_dir()?.join("node_registry.json"), &NoopProgress).await?;
```

---

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

The daemon manages node processes, exposing a REST API on localhost. No admin privileges required. The CLI, web UI, and AI agents all communicate over HTTP.

Data operations (upload/download) go directly to the P2P network — they do not require the daemon. The daemon is only needed for node management.

## Project Structure

```
├── ant-core/                    # Headless library — all business logic
│   ├── src/
│   │   ├── lib.rs
│   │   ├── config.rs            # Platform-appropriate data/log paths
│   │   ├── error.rs             # Unified error type
│   │   ├── data/                # Data storage and retrieval
│   │   │   ├── mod.rs           # Re-exports and module declarations
│   │   │   ├── error.rs         # Data operation errors
│   │   │   ├── network.rs       # P2P network wrapper
│   │   │   └── client/          # High-level client API
│   │   │       ├── mod.rs       # Client, ClientConfig
│   │   │       ├── chunk.rs     # chunk_put, chunk_get, chunk_exists
│   │   │       ├── data.rs      # data_upload, data_download, data_map_store/fetch
│   │   │       ├── file.rs      # file_upload, file_download (streaming)
│   │   │       ├── payment.rs   # pay_for_storage, approve_token_spend
│   │   │       ├── quote.rs     # get_store_quotes from peers
│   │   │       ├── merkle.rs    # Merkle batch payment (PaymentMode)
│   │   │       └── cache.rs     # In-memory LRU chunk cache
│   │   └── node/                # Node management
│   │       ├── mod.rs           # add_nodes, remove_node, reset
│   │       ├── types.rs         # DaemonConfig, NodeConfig, AddNodeOpts, etc.
│   │       ├── events.rs        # NodeEvent enum, EventListener trait
│   │       ├── binary.rs        # Binary resolution, ProgressReporter trait
│   │       ├── registry.rs      # Node registry (CRUD, JSON, file locking)
│   │       ├── devnet.rs        # LocalDevnet (local network + Anvil EVM)
│   │       ├── daemon/
│   │       │   ├── mod.rs
│   │       │   ├── client.rs    # Daemon client (start/stop/status via HTTP)
│   │       │   ├── server.rs    # HTTP server (axum), REST API handlers
│   │       │   └── supervisor.rs # Process supervision with backoff
│   │       └── process/
│   │           ├── mod.rs
│   │           ├── spawn.rs     # Spawning node processes
│   │           └── detach.rs    # Platform-specific session detachment
│   └── tests/                   # Integration tests
├── ant-cli/                     # CLI binary (thin adapter layer)
│   └── src/
│       ├── main.rs              # Entry point, client/wallet initialization
│       ├── cli.rs               # clap argument definitions
│       └── commands/
│           ├── data/
│           │   ├── file.rs      # ant file upload/download
│           │   ├── chunk.rs     # ant chunk put/get
│           │   └── wallet.rs    # ant wallet address/balance
│           └── node/
│               ├── add.rs       # ant node add
│               ├── daemon.rs    # ant node daemon start/stop/status/info/run
│               ├── logs.rs      # ant node logs forward enable/disable/status
│               ├── start.rs     # ant node start
│               ├── stop.rs      # ant node stop
│               ├── status.rs    # ant node status
│               └── reset.rs     # ant node reset
└── docs/                        # Architecture documentation
```

## EVM Networks

| Value | Network | Use case |
|-------|---------|----------|
| `arbitrum-one` | Arbitrum mainnet | Production |
| `arbitrum-sepolia` | Arbitrum Sepolia testnet | Staging / testing |
| `local` | Custom (Anvil) | Local development (requires `--devnet-manifest`) |

## Environment Variables

| Variable | Required for | Description |
|----------|-------------|-------------|
| `SECRET_KEY` | Uploads, wallet commands | EVM private key (hex, with or without `0x` prefix) |

## Development

```bash
# Build
cargo build

# Run all tests
cargo test --all

# Lint
cargo clippy --all-targets --all-features -- -D warnings

# Format check
cargo fmt --all -- --check

# Run the CLI
cargo run --bin ant -- --help
cargo run --bin ant -- file upload photo.jpg --public --devnet-manifest ~/.local/share/ant/devnet-manifest.json --allow-loopback --evm-network local
cargo run --bin ant -- node daemon status
```

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in the work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
