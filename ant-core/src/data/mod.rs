//! Data operations for the Autonomi decentralized network.
//!
//! Provides high-level APIs for storing and retrieving data
//! using post-quantum cryptography.

pub mod client;
pub mod error;
pub mod network;

pub use client::cache::ChunkCache;
pub use client::{Client, ClientConfig};
pub use error::{Error, Result};
pub use network::Network;

// LocalDevnet (and the optional node-side dep it pulls in) is gated behind
// the `devnet` feature. Default builds of ant-core do not link ant-node.
#[cfg(feature = "devnet")]
pub use crate::node::devnet::LocalDevnet;

// Re-export commonly used types from the wire protocol crate.
pub use ant_protocol::{compute_address, DataChunk, XorName};

// Re-export client data types
pub use client::batch::{finalize_batch_payment, PaidChunk, PaymentIntent, PreparedChunk};
pub use client::data::DataUploadResult;
pub use client::file::{
    DownloadEvent, ExternalPaymentInfo, FileUploadResult, PreparedUpload, UploadCostEstimate,
    UploadEvent, Visibility,
};
pub use client::merkle::{
    finalize_merkle_batch, MerkleBatchPaymentResult, PaymentMode, PreparedMerkleBatch,
    DEFAULT_MERKLE_THRESHOLD,
};

// Re-export self-encryption types
pub use self_encryption::DataMap;

// Re-export networking types needed by CLI for P2P node creation. The
// devnet manifest types live in ant-protocol because both the node
// (writer) and the CLI (reader) need them; they are always available
// regardless of the `devnet` feature.
pub use ant_protocol::transport::{
    CoreNodeConfig, IPDiversityConfig, MultiAddr, NodeMode, P2PNode,
};
pub use ant_protocol::{DevnetManifest, MAX_CHUNK_SIZE, MAX_WIRE_MESSAGE_SIZE};

// Re-export EVM types needed by CLI for wallet and network setup
pub use ant_protocol::evm::{
    Address as EvmAddress, CustomNetwork, Network as EvmNetwork, Wallet, U256,
};
