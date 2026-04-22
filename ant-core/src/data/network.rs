//! Network layer wrapping ant-node's P2P node.
//!
//! Provides peer discovery, message sending, and DHT operations
//! for the client library.

use crate::data::error::{Error, Result};
use ant_node::ant_protocol::MAX_WIRE_MESSAGE_SIZE;
use ant_node::core::{CoreNodeConfig, IPDiversityConfig, MultiAddr, NodeMode, P2PNode, PeerId};
use std::net::SocketAddr;
use std::sync::Arc;

/// Network abstraction for the Autonomi client.
///
/// Wraps a `P2PNode` providing high-level operations for
/// peer discovery and message routing.
pub struct Network {
    node: Arc<P2PNode>,
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
    pub fn from_node(node: Arc<P2PNode>) -> Self {
        Self { node }
    }

    /// Get a reference to the underlying P2P node.
    #[must_use]
    pub fn node(&self) -> &Arc<P2PNode> {
        &self.node
    }

    /// Get the local peer ID.
    #[must_use]
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

    /// Get all currently connected peers.
    pub async fn connected_peers(&self) -> Vec<PeerId> {
        self.node.connected_peers().await
    }
}
