//! Client operations for the Autonomi network.
//!
//! Provides high-level APIs for storing and retrieving data
//! on the Autonomi decentralized network.

pub mod batch;
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
use ant_node::client::XorName;
use ant_node::core::{MultiAddr, P2PNode, PeerId};
use ant_node::CLOSE_GROUP_SIZE;
use evmlib::wallet::Wallet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

/// Default timeout for lightweight network operations (quotes, DHT lookups) in seconds.
const DEFAULT_QUOTE_TIMEOUT_SECS: u64 = 10;

/// Default timeout for chunk store operations in seconds.
///
/// Chunk PUTs transfer multi-MB payloads to multiple peers. On residential
/// connections with limited upload bandwidth, the default quote timeout (10 s)
/// is far too short — a 4 MB chunk at 1 Mbps takes ~32 s just for the data
/// transfer, before accounting for QUIC slow-start and NAT traversal overhead.
const DEFAULT_STORE_TIMEOUT_SECS: u64 = 10;

/// Default quote concurrency: high because quoting is pure network I/O
/// (DHT lookups + small request/response messages) with no CPU-bound work.
const DEFAULT_QUOTE_CONCURRENCY: usize = 32;

/// Default store concurrency: moderate because each chunk PUT sends ~4MB
/// to 7 close-group peers. At 8 concurrent stores, ~225MB of outbound
/// traffic can be in flight. Users on fast connections can increase this
/// with --store-concurrency; users on slow connections can decrease it.
const DEFAULT_STORE_CONCURRENCY: usize = 8;

/// Configuration for the Autonomi client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Timeout for lightweight network operations (quotes, DHT lookups) in seconds.
    pub quote_timeout_secs: u64,
    /// Timeout for chunk store (PUT) operations in seconds.
    ///
    /// This should be significantly longer than `quote_timeout_secs` because
    /// each chunk PUT transfers ~4 MB to multiple peers.
    pub store_timeout_secs: u64,
    /// Number of closest peers to consider for routing.
    pub close_group_size: usize,
    /// Maximum number of chunks quoted or downloaded concurrently.
    ///
    /// Controls parallelism for quote collection and chunk retrieval.
    /// These are pure network I/O operations (DHT lookups, small messages)
    /// with negligible CPU cost, so a high default is safe.
    pub quote_concurrency: usize,
    /// Maximum number of chunks stored concurrently during uploads.
    ///
    /// Controls parallelism for chunk PUT operations. Lower than quote
    /// concurrency because storing to NAT nodes requires hole-punch
    /// connection establishment, which is stateful and time-sensitive.
    /// Defaults to half the available CPU threads.
    pub store_concurrency: usize,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            quote_timeout_secs: DEFAULT_QUOTE_TIMEOUT_SECS,
            store_timeout_secs: DEFAULT_STORE_TIMEOUT_SECS,
            close_group_size: CLOSE_GROUP_SIZE,
            quote_concurrency: DEFAULT_QUOTE_CONCURRENCY,
            store_concurrency: DEFAULT_STORE_CONCURRENCY,
        }
    }
}

/// Client for the Autonomi decentralized network.
///
/// Provides high-level APIs for storing and retrieving chunks
/// and files on the network.
pub struct Client {
    config: ClientConfig,
    network: Network,
    wallet: Option<Arc<Wallet>>,
    evm_network: Option<evmlib::Network>,
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
            evm_network: None,
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
            "Connecting to Autonomi network with {} bootstrap peers",
            bootstrap_peers.len()
        );
        let network = Network::new(bootstrap_peers).await?;
        Ok(Self {
            config,
            network,
            wallet: None,
            evm_network: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
        })
    }

    /// Set the wallet for payment operations.
    ///
    /// Also populates the EVM network from the wallet so that
    /// token approvals work without a separate `with_evm_network` call.
    #[must_use]
    pub fn with_wallet(mut self, wallet: Wallet) -> Self {
        self.evm_network = Some(wallet.network().clone());
        self.wallet = Some(Arc::new(wallet));
        self
    }

    /// Set the EVM network without requiring a wallet.
    ///
    /// This enables token approval and contract interactions
    /// for external-signer flows where the private key lives outside Rust.
    #[must_use]
    pub fn with_evm_network(mut self, network: evmlib::Network) -> Self {
        self.evm_network = Some(network);
        self
    }

    /// Get the EVM network, falling back to the wallet's network if available.
    ///
    /// # Errors
    ///
    /// Returns an error if neither `with_evm_network` nor `with_wallet` was called.
    pub(crate) fn require_evm_network(&self) -> Result<&evmlib::Network> {
        if let Some(ref net) = self.evm_network {
            return Ok(net);
        }
        if let Some(ref wallet) = self.wallet {
            return Ok(wallet.network());
        }
        Err(Error::Payment(
            "EVM network not configured — call with_evm_network() or with_wallet() first"
                .to_string(),
        ))
    }

    /// Get the client configuration.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a mutable reference to the client configuration.
    pub fn config_mut(&mut self) -> &mut ClientConfig {
        &mut self.config
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
    /// Returns each peer paired with its known network addresses.
    pub(crate) async fn close_group_peers(
        &self,
        target: &XorName,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
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
