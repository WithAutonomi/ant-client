//! Client operations for the saorsa network.
//!
//! Provides high-level APIs for storing and retrieving data
//! on the saorsa decentralized network.

pub mod cache;
pub mod chunk;
pub mod data;
pub mod file;
pub mod merkle;
pub mod payment;
pub mod quote;

use crate::data::client::cache::ChunkCache;
use crate::data::error::{Error, Result};
use crate::data::network::Network;
use evmlib::wallet::Wallet;
use saorsa_node::client::XorName;
use saorsa_node::core::{P2PNode, PeerId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

/// Configuration for the saorsa client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Timeout for network operations in seconds.
    pub timeout_secs: u64,
    /// Number of closest peers to consider for routing.
    pub close_group_size: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 30,
            close_group_size: 8,
        }
    }
}

/// Client for the saorsa decentralized network.
///
/// Provides high-level APIs for storing and retrieving chunks
/// and files on the network.
pub struct Client {
    config: ClientConfig,
    network: Network,
    wallet: Option<Arc<Wallet>>,
    chunk_cache: ChunkCache,
    next_request_id: AtomicU64,
}

impl Client {
    /// Create a client connected to the given P2P node.
    #[must_use]
    pub fn from_node(node: Arc<P2PNode>, config: ClientConfig) -> Self {
        let network = Network::from_node(node);
        Self {
            config,
            network,
            wallet: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
        }
    }

    /// Create a client connected to bootstrap peers.
    ///
    /// # Errors
    ///
    /// Returns an error if the P2P node cannot be created or bootstrapping fails.
    pub async fn connect(
        bootstrap_peers: &[std::net::SocketAddr],
        config: ClientConfig,
    ) -> Result<Self> {
        debug!(
            "Connecting to saorsa network with {} bootstrap peers",
            bootstrap_peers.len()
        );
        let network = Network::new(bootstrap_peers).await?;
        Ok(Self {
            config,
            network,
            wallet: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
        })
    }

    /// Set the wallet for payment operations.
    #[must_use]
    pub fn with_wallet(mut self, wallet: Wallet) -> Self {
        self.wallet = Some(Arc::new(wallet));
        self
    }

    /// Get the client configuration.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a reference to the network layer.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.network
    }

    /// Get the wallet, if configured.
    #[must_use]
    pub fn wallet(&self) -> Option<&Arc<Wallet>> {
        self.wallet.as_ref()
    }

    /// Get a reference to the chunk cache.
    #[must_use]
    pub fn chunk_cache(&self) -> &ChunkCache {
        &self.chunk_cache
    }

    /// Get the next request ID for protocol messages.
    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Return all peers in the close group for a target address.
    ///
    /// Queries the DHT for the closest peers by XOR distance.
    pub(crate) async fn close_group_peers(&self, target: &XorName) -> Result<Vec<PeerId>> {
        let peers = self
            .network()
            .find_closest_peers(target, self.config().close_group_size)
            .await?;

        if peers.is_empty() {
            return Err(Error::InsufficientPeers(
                "DHT returned no peers for target address".to_string(),
            ));
        }
        Ok(peers)
    }
}
