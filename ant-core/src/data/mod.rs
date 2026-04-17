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

// Re-export LocalDevnet from its new home in the node module (requires `devnet` feature)
#[cfg(feature = "devnet")]
pub use crate::node::devnet::LocalDevnet;

// Re-export commonly used wire types from ant-protocol.
pub use ant_protocol::{compute_address, DataChunk, XorName};

// Re-export client data types
pub use client::batch::{finalize_batch_payment, PaidChunk, PaymentIntent, PreparedChunk};
pub use client::data::DataUploadResult;
pub use client::file::{
    DownloadEvent, ExternalPaymentInfo, FileUploadResult, PreparedUpload, UploadEvent,
};
pub use client::merkle::{
    finalize_merkle_batch, MerkleBatchPaymentResult, PaymentMode, PreparedMerkleBatch,
    DEFAULT_MERKLE_THRESHOLD,
};

// Re-export self-encryption types
pub use self_encryption::DataMap;

// Re-export protocol constants + transport types needed by CLI for P2P node creation
pub use ant_protocol::transport::{CoreNodeConfig, MultiAddr, NodeMode, P2PNode};
pub use ant_protocol::{MAX_CHUNK_SIZE, MAX_WIRE_MESSAGE_SIZE};

// DevnetManifest is a pure-data handoff file between the devnet
// launcher and clients that want to connect — no ant-node runtime
// dep needed to read or write it.
pub use ant_protocol::DevnetManifest;

// Re-export EVM types needed by CLI for wallet and network setup. Sourced
// through ant-protocol so ant-core keeps no direct evmlib dep — that pin
// is owned by ant-protocol.
pub use ant_protocol::evm::{
    Address as EvmAddress, CustomNetwork, Network as EvmNetwork, Wallet, U256,
};
