# Architecture: Data Storage and Retrieval

## Overview

Autonomi stores data as content-addressed, encrypted chunks on a P2P network. All data — files,
blobs, metadata — is broken into chunks via [self-encryption](https://en.wikipedia.org/wiki/Convergent_encryption),
each stored at an XOR address derived from its content. Storage is paid for via EVM transactions
on an Arbitrum-compatible blockchain.

The `ant_core::data` module provides the complete API. No daemon is needed for data operations —
the client connects directly to the P2P network.

## Key Concepts

### Content Addressing (XorName)

Every chunk is stored at a 32-byte `XorName` derived from its content hash. This means:
- Identical data always maps to the same address (deduplication is automatic).
- You cannot modify data in place — you store a new chunk at a new address.
- Addresses are deterministic: you can compute a chunk's address before storing it.

### Self-Encryption

Self-encryption (convergent encryption) encrypts data using keys derived from the data itself:
1. Data is split into chunks.
2. Each chunk is encrypted using keys derived from neighboring chunks.
3. The encryption is deterministic — the same input always produces the same encrypted chunks.
4. A `DataMap` records all chunk addresses and encryption parameters needed for reconstruction.

The `DataMap` is the "key" to your data. Whoever holds the DataMap can reconstruct the original
content. DataMaps can be:
- **Kept private** (saved to a local file) — only the holder can download.
- **Stored publicly** (uploaded as a chunk) — anyone with the chunk address can download.

### Payment Model

Each chunk stored on the network requires a one-time payment to the nodes that store it:

1. **Quote collection**: Client asks close-group peers for storage quotes (price + terms).
2. **Payment**: Client sends an EVM transaction paying the quoted amount.
3. **Storage proof**: The payment transaction produces a proof that the client presents when
   uploading the chunk.
4. **Verification**: Storing nodes verify the proof before accepting the chunk.

Two payment modes are available:
- **Single** — One EVM transaction per chunk. Simple but costly for many chunks.
- **Merkle** — A single EVM transaction covers a batch of chunks via a merkle tree proof. This
  reduces gas costs significantly for multi-chunk uploads (e.g., files).
- **Auto** (default) — Uses merkle when >= 64 chunks, otherwise per-chunk.

## Client API

### Creating a Client

```rust
use ant_core::data::{Client, ClientConfig};

// Connect to known bootstrap peers
let client = Client::connect(
    &["1.2.3.4:12000".parse()?],
    ClientConfig::default(),
).await?;
```

`ClientConfig` controls:
- `timeout_secs` — Timeout for each network operation (default: 10s).
- `close_group_size` — Number of closest peers to query for routing (default: protocol constant).

### Attaching a Wallet

Upload operations require a funded EVM wallet:

```rust
use ant_core::data::{Wallet, EvmNetwork};

let wallet = Wallet::new_from_private_key(
    EvmNetwork::ArbitrumOne,
    "your_private_key_hex",
)?;
let client = client.with_wallet(wallet);

// Approve the payment contract to spend tokens (one-time per session)
client.approve_token_spend().await?;
```

### File Operations

Files are the primary high-level API. They handle chunking, encryption, payment, and storage
automatically.

```rust
use std::path::Path;
use ant_core::data::PaymentMode;

// Upload (streaming — file never fully loaded into memory)
let result = client.file_upload(Path::new("video.mp4")).await?;
// result.data_map     — the DataMap (save this to download later)
// result.chunks_stored — number of chunks uploaded

// Upload with explicit payment mode
let result = client.file_upload_with_mode(
    Path::new("large_dataset.tar"),
    PaymentMode::Merkle,
).await?;

// Make data publicly downloadable
let public_address: [u8; 32] = client.data_map_store(&result.data_map).await?;

// Download a file
let data_map = client.data_map_fetch(&public_address).await?;
let bytes_written = client.file_download(&data_map, Path::new("output.mp4")).await?;
```

### In-Memory Data Operations

For data already in memory (API payloads, generated content, etc.):

```rust
use bytes::Bytes;

// Upload bytes
let result = client.data_upload(Bytes::from(my_data)).await?;

// Upload with payment mode control
let result = client.data_upload_with_mode(
    Bytes::from(my_data),
    PaymentMode::Merkle,
).await?;

// Download and decrypt
let content: Bytes = client.data_download(&result.data_map).await?;
```

### Chunk Operations

Low-level operations on individual chunks (each up to `MAX_CHUNK_SIZE` bytes):

```rust
use ant_core::data::XorName;

// Store a chunk (pays for storage automatically)
let address: XorName = client.chunk_put(Bytes::from("small data")).await?;

// Retrieve a chunk
if let Some(chunk) = client.chunk_get(&address).await? {
    println!("Content: {} bytes", chunk.content.len());
}

// Check existence without downloading
let exists: bool = client.chunk_exists(&address).await?;
```

### Chunk Cache

The client maintains an in-memory LRU cache (default capacity: 1024 chunks) to avoid redundant
network fetches:

```rust
let cache = client.chunk_cache();
cache.contains(&address);  // Check without fetch
cache.len();               // Current cache size
cache.clear();             // Evict all
```

The cache is used transparently by `chunk_get` — repeated reads of the same address hit the cache.

## Network Layer

The `Network` struct wraps a P2P node and provides DHT operations:

```rust
use ant_core::data::Network;

let network = client.network();

// Find peers closest to a target address (XOR distance)
let peers = network.find_closest_peers(&target_hash, 8).await?;

// List currently connected peers
let connected = network.connected_peers().await;

// Get local peer ID
let my_id = network.peer_id();
```

## Local Devnet

`LocalDevnet` provides a complete local development environment:

```rust
use ant_core::data::LocalDevnet;

// Start 5 Autonomi nodes + a local Anvil EVM blockchain
let devnet = LocalDevnet::start_minimal().await?;

// Get a funded client (wallet with test tokens, pre-approved)
let client = devnet.create_funded_client().await?;

// Use the client normally
let result = client.data_upload(Bytes::from("test")).await?;
let content = client.data_download(&result.data_map).await?;

// Export manifest for CLI usage:
//   ant file upload ... --devnet-manifest /tmp/devnet.json --allow-loopback --evm-network local
devnet.write_manifest(Path::new("/tmp/devnet.json")).await?;

// Shut down everything
devnet.shutdown().await?;
```

Devnet configurations:
- `LocalDevnet::start_minimal()` — 5 nodes (fast startup, good for unit tests).
- `LocalDevnet::start_small()` — 10 nodes (better for integration tests).
- `LocalDevnet::start(config)` — Custom `DevnetConfig` with arbitrary node count, ports, etc.

## Error Handling

All data operations return `ant_core::data::Result<T>`, where `Error` covers:

| Variant | Meaning |
|---------|---------|
| `Network` | P2P network failure (connection, routing) |
| `Storage` | Storage node rejected the operation |
| `Payment` | EVM transaction failed or insufficient funds |
| `Timeout` | Peer didn't respond within the timeout |
| `InsufficientPeers` | Not enough peers found for the operation |
| `Encryption` | Self-encryption or decryption failure |
| `InvalidData` | Data integrity check failed |
| `AlreadyStored` | Chunk already exists on the network (not an error in practice) |
| `Io` | Local filesystem error |
| `Serialization` | DataMap or protocol message serialization failure |

## EVM Networks

| Network | Chain | Use case |
|---------|-------|----------|
| `EvmNetwork::ArbitrumOne` | Arbitrum mainnet | Production |
| `EvmNetwork::ArbitrumSepoliaTest` | Arbitrum Sepolia | Staging / testing |
| `EvmNetwork::Custom(...)` | Any EVM chain | Local dev (Anvil), custom deployments |

For local development, `LocalDevnet` handles all EVM setup automatically. For custom deployments,
construct a `CustomNetwork` with the RPC URL, token address, and payment contract addresses.
