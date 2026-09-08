//! Network layer wrapping ant-node's P2P node.
//!
//! Provides peer discovery, message sending, and DHT operations
//! for the client library.

#[cfg(feature = "native")]
use crate::data::error::Error;
use crate::data::error::Result;
use ant_protocol::transport::{DHTNode, MultiAddr, PeerId, WitnessedCloseGroup};
#[cfg(feature = "native")]
use ant_protocol::{
    transport::{CoreNodeConfig, IPDiversityConfig, NodeMode, P2PNode},
    MAX_WIRE_MESSAGE_SIZE,
};
#[cfg(feature = "native")]
use std::net::SocketAddr;
#[cfg(feature = "native")]
use std::sync::Arc;

/// Network abstraction for the Autonomi client.
///
/// Wraps a `P2PNode` providing high-level operations for
/// peer discovery and message routing.
#[derive(Clone)]
pub struct Network {
    #[cfg(feature = "native")]
    node: Arc<P2PNode>,
    #[cfg(not(feature = "native"))]
    backend: std::rc::Rc<dyn BrowserNetwork>,
}

/// Peer identities and the addresses that can reach them.
pub type PeerAddresses = Vec<(PeerId, Vec<MultiAddr>)>;

/// Browser transport boundary for the shared client. Implementations perform
/// authenticated RPC and discovery; client policy stays in `Client`.
#[cfg(not(feature = "native"))]
pub trait BrowserNetwork {
    /// Local identity used when excluding the client from remote candidates.
    fn peer_id(&self) -> &PeerId;
    /// Closest authenticated peers, ordered by XOR distance.
    fn find_closest_peers<'a>(
        &'a self,
        target: &'a [u8; 32],
        count: usize,
    ) -> futures::future::LocalBoxFuture<'a, Result<PeerAddresses>>;
    /// Authenticated responder views for witnessed quote admission.
    fn find_witnessed_close_group<'a>(
        &'a self,
        target: &'a [u8; 32],
        count: usize,
        view_count: usize,
    ) -> futures::future::LocalBoxFuture<'a, Result<WitnessedCloseGroup>>;
    /// Known records used as fallback candidates after a lookup failure.
    fn known_peers(&self) -> Vec<DHTNode>;
    /// Execute one authenticated request, preserving its request identifier.
    fn request<'a>(
        &'a self,
        peer: &'a PeerId,
        addrs: &'a [MultiAddr],
        request: ant_protocol::ChunkMessage,
        timeout: std::time::Duration,
    ) -> futures::future::LocalBoxFuture<'a, Result<ant_protocol::ChunkMessage>>;
}

impl Network {
    /// Create a new network connection with the given bootstrap peers.
    ///
    /// `allow_loopback` controls the saorsa-transport `local` flag on the
    /// underlying `CoreNodeConfig`. Set it to `true` only for devnet / local
    /// testing. Public Autonomi network peers reject the QUIC handshake
    /// variant produced when `local = true`, so production callers must pass
    /// `false` (this is what `ant-cli` does by default — see
    /// `ant-cli/src/main.rs::create_client_node_raw`, which builds a similar
    /// `CoreNodeConfig` directly, with `ipv6` toggled by the `--ipv4-only`
    /// flag).
    ///
    /// `ipv6` controls whether the node binds a dual-stack IPv6 socket
    /// (`true`) or an IPv4-only socket (`false`). The default for library
    /// callers should be `true` to match the CLI default; set it to `false`
    /// only when running on hosts without a working IPv6 stack, to avoid
    /// advertising unreachable v6 addresses to the DHT.
    ///
    /// # Errors
    ///
    /// Returns an error if the P2P node cannot be created or bootstrapping fails.
    #[cfg(feature = "native")]
    pub async fn new(
        bootstrap_peers: &[SocketAddr],
        allow_loopback: bool,
        ipv6: bool,
    ) -> Result<Self> {
        let mut core_config = CoreNodeConfig::builder()
            .port(0)
            .ipv6(ipv6)
            .local(allow_loopback)
            .mode(NodeMode::Client)
            .max_message_size(MAX_WIRE_MESSAGE_SIZE)
            .build()
            .map_err(|e| Error::Network(format!("Failed to create core config: {e}")))?;

        // Clients never enforce IP-diversity limits: they don't host data and
        // their routing table exists only to find peers, not to be defended
        // against Sybil clustering. Strict per-IP / per-subnet caps would
        // silently drop legitimate testnet peers that share an IP or /24.
        core_config.diversity_config = Some(IPDiversityConfig::permissive());

        core_config.bootstrap_peers = bootstrap_peers
            .iter()
            .map(|addr| MultiAddr::quic(*addr))
            .collect();

        let node = P2PNode::new(core_config)
            .await
            .map_err(|e| Error::Network(format!("Failed to create P2P node: {e}")))?;

        node.start()
            .await
            .map_err(|e| Error::Network(format!("Failed to start P2P node: {e}")))?;

        Ok(Self {
            node: Arc::new(node),
        })
    }

    /// Create a network from an existing P2P node.
    #[must_use]
    #[cfg(feature = "native")]
    pub fn from_node(node: Arc<P2PNode>) -> Self {
        Self { node }
    }

    /// Get a reference to the underlying P2P node.
    #[must_use]
    #[cfg(feature = "native")]
    pub fn node(&self) -> &Arc<P2PNode> {
        &self.node
    }

    /// Get the local peer ID.
    #[must_use]
    #[cfg(feature = "native")]
    pub fn peer_id(&self) -> &PeerId {
        self.node.peer_id()
    }

    /// Find the closest peers to a target address.
    ///
    /// Returns each peer paired with its known network addresses, enabling
    /// callers to pass addresses to `send_and_await_chunk_response` for
    /// faster connection establishment.
    ///
    /// # Errors
    ///
    /// Returns an error if the DHT lookup fails.
    #[cfg(feature = "native")]
    pub async fn find_closest_peers(
        &self,
        target: &[u8; 32],
        count: usize,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        let local_peer_id = self.node.peer_id();

        // Request one extra to account for filtering out our own peer ID
        let closest_nodes = self
            .node
            .dht()
            .find_closest_nodes(target, count + 1)
            .await
            .map_err(|e| Error::Network(format!("DHT closest-nodes lookup failed: {e}")))?;

        Ok(closest_nodes
            .into_iter()
            .filter(|n| n.peer_id != *local_peer_id)
            .take(count)
            .map(|n| {
                let addrs = n.addresses_by_priority();
                (n.peer_id, addrs)
            })
            .collect())
    }

    /// Find a witnessed close-group transcript for a target address.
    ///
    /// The underlying DHT method returns the initial client K, each responder's
    /// self-inclusive closest-K node view, and enough trusted node records for
    /// callers to apply their own quorum and fallback policy.
    ///
    /// # Errors
    ///
    /// Returns an error if the DHT lookup itself fails. The returned transcript
    /// may still be inconclusive; callers should evaluate it before payment.
    pub async fn find_witnessed_close_group(
        &self,
        target: &[u8; 32],
        count: usize,
    ) -> Result<WitnessedCloseGroup> {
        self.find_witnessed_close_group_with_view_count(target, count, count)
            .await
    }

    /// Find a witnessed close-group transcript with wider responder views.
    ///
    /// `count` is the initial responder set size. `view_count` is the number
    /// of closest nodes each responder view may contribute.
    ///
    /// # Errors
    ///
    /// Returns an error if the DHT lookup itself fails. The returned transcript
    /// may still be inconclusive; callers should evaluate it before payment.
    #[cfg(feature = "native")]
    pub async fn find_witnessed_close_group_with_view_count(
        &self,
        target: &[u8; 32],
        count: usize,
        view_count: usize,
    ) -> Result<WitnessedCloseGroup> {
        self.node
            .dht()
            .find_witnessed_close_group_with_view_count(target, count, view_count)
            .await
            .map_err(|e| Error::Network(format!("DHT witnessed close-group lookup failed: {e}")))
    }

    /// Get all currently connected peers.
    #[cfg(feature = "native")]
    pub async fn connected_peers(&self) -> Vec<PeerId> {
        self.node.connected_peers().await
    }
}

impl Network {
    /// Construct the same client network facade with a browser transport.
    #[cfg(not(feature = "native"))]
    pub fn from_browser(backend: std::rc::Rc<dyn BrowserNetwork>) -> Self {
        Self { backend }
    }

    /// Local client identity.
    #[cfg(not(feature = "native"))]
    pub fn peer_id(&self) -> &PeerId {
        self.backend.peer_id()
    }

    /// Find closest authenticated peers through the browser adapter.
    #[cfg(not(feature = "native"))]
    pub async fn find_closest_peers(
        &self,
        target: &[u8; 32],
        count: usize,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        self.backend.find_closest_peers(target, count).await
    }

    /// Collect authenticated close-group responder views.
    #[cfg(not(feature = "native"))]
    pub async fn find_witnessed_close_group_with_view_count(
        &self,
        target: &[u8; 32],
        count: usize,
        view_count: usize,
    ) -> Result<WitnessedCloseGroup> {
        self.backend
            .find_witnessed_close_group(target, count, view_count)
            .await
    }

    /// Currently known peers on the browser session.
    #[cfg(not(feature = "native"))]
    pub async fn connected_peers(&self) -> Vec<PeerId> {
        self.backend
            .known_peers()
            .into_iter()
            .map(|node| node.peer_id)
            .collect()
    }

    /// Peer records available for fallback retrieval.
    pub async fn known_peers(&self) -> Vec<DHTNode> {
        #[cfg(feature = "native")]
        {
            self.node.dht().routing_table_peers().await
        }
        #[cfg(not(feature = "native"))]
        {
            self.backend.known_peers()
        }
    }
}

/// Execute a request through the platform adapter and apply the shared response handler.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_and_await_chunk_response<T, E>(
    network: &Network,
    target_peer: &PeerId,
    message_bytes: Vec<u8>,
    request_id: u64,
    timeout: std::time::Duration,
    peer_addrs: &[MultiAddr],
    response_handler: impl Fn(ant_protocol::ChunkMessageBody) -> Option<std::result::Result<T, E>>,
    send_error: impl FnOnce(String) -> E,
    timeout_error: impl FnOnce() -> E,
) -> std::result::Result<T, E> {
    #[cfg(feature = "native")]
    {
        ant_protocol::send_and_await_chunk_response(
            network.node(),
            target_peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            response_handler,
            send_error,
            timeout_error,
        )
        .await
    }
    #[cfg(not(feature = "native"))]
    {
        let response = match ant_protocol::ChunkMessage::decode(&message_bytes) {
            Ok(request) => {
                network
                    .backend
                    .request(target_peer, peer_addrs, request, timeout)
                    .await
            }
            Err(error) => Err(crate::data::error::Error::Protocol(error.to_string())),
        };
        let response = match response {
            Ok(response) => response,
            Err(crate::data::error::Error::Timeout(_)) => return Err(timeout_error()),
            Err(error) => return Err(send_error(error.to_string())),
        };
        if response.request_id != request_id {
            return Err(timeout_error());
        }
        response_handler(response.body).unwrap_or_else(|| Err(timeout_error()))
    }
}
