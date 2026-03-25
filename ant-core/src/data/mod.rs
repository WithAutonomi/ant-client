//! Data operations for the saorsa decentralized network.
//!
//! Provides high-level APIs for storing and retrieving data
//! using post-quantum cryptography.

pub mod client;
pub mod devnet;
pub mod error;
pub mod network;

pub use client::cache::ChunkCache;
pub use client::{Client, ClientConfig};
pub use devnet::LocalDevnet;
pub use error::{Error, Result};
pub use network::Network;

// Re-export commonly used types from saorsa-node
pub use saorsa_node::client::{compute_address, DataChunk, XorName};

// Re-export client data types
pub use client::data::DataUploadResult;
pub use client::file::FileUploadResult;
pub use client::merkle::{MerkleBatchPaymentResult, PaymentMode, DEFAULT_MERKLE_THRESHOLD};

// Re-export self-encryption types
pub use self_encryption::DataMap;
