//! Data operations for the saorsa decentralized network.
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

// Re-export LocalDevnet from its new home in the node module
pub use crate::node::devnet::LocalDevnet;

// Re-export commonly used types from saorsa-node
pub use saorsa_node::client::{compute_address, DataChunk, XorName};

// Re-export client data types
pub use client::data::DataUploadResult;
pub use client::file::FileUploadResult;
pub use client::merkle::{MerkleBatchPaymentResult, PaymentMode, DEFAULT_MERKLE_THRESHOLD};

// Re-export self-encryption types
pub use self_encryption::DataMap;

// Re-export networking types needed by CLI for P2P node creation
pub use saorsa_node::ant_protocol::{MAX_CHUNK_SIZE, MAX_WIRE_MESSAGE_SIZE};
pub use saorsa_node::core::{CoreNodeConfig, MultiAddr, NodeMode, P2PNode};
pub use saorsa_node::devnet::DevnetManifest;

// Re-export EVM types needed by CLI for wallet and network setup
pub use evmlib::common::{Address as EvmAddress, U256};
pub use evmlib::wallet::Wallet;
pub use evmlib::CustomNetwork;
pub use evmlib::Network as EvmNetwork;
