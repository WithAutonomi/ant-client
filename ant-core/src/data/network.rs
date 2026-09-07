//! Network layer wrapping ant-node's P2P node.
//!
//! Provides peer discovery, message sending, and DHT operations
//! for the client library.

use crate::data::error::{Error, Result};
use ant_protocol::transport::{
    CoreNodeConfig, IPDiversityConfig, MultiAddr, NodeMode, P2PNode, PeerId, WitnessedCloseGroup,
};
use ant_protocol::MAX_WIRE_MESSAGE_SIZE;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

/// Mirror of saorsa-core's private `AUTO_REBOOTSTRAP_THRESHOLD`
/// (dht_network_manager.rs): the routing-table size below which the DHT
/// auto-re-bootstraps. saorsa-core PR #153 makes the real const public;
/// once a release carries it, consume that instead of this mirror.
pub const REBOOTSTRAP_THRESHOLD: usize = 3;

/// Live network-participation snapshot.
///
/// One implementation of the write-readiness formula for every embedded-client
/// consumer (antd, ant-gui, ant-ffi, ant-tui) — see [`Network::health`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkHealth {
    /// Best-effort write-path floor:
    /// `max(routing_table_size, connected_peers) >= rebootstrap_threshold`.
    pub write_ready: bool,
    /// Identity-verified peer connections currently held by the node.
    pub connected_peers: u32,
    /// Entries in the DHT routing table.
    pub routing_table_size: u32,
    /// Routing-table size below which the DHT auto-re-bootstraps.
    pub rebootstrap_threshold: u32,
}

impl NetworkHealth {
    /// Build a snapshot from raw peer counts.
    ///
    /// `write_ready` is keyed on `max(routing_table_size, connected_peers)`:
    /// in client mode the DHT routing table can sit below the re-bootstrap
    /// threshold while plenty of live connections exist and stores succeed
    /// (observed on a LAN devnet: rt=2, connected=10, paid upload fine), so
    /// the routing table alone would under-report; the connected count alone
    /// misses the inverse case (~1 reachable peer, rt=0, stores failing).
    /// Neither signal guarantees a store will fully succeed (stores proceed
    /// with as little as one reachable node), but when both are below the
    /// threshold the node is known-degraded.
    #[must_use]
    pub fn from_counts(connected_peers: usize, routing_table_size: usize) -> Self {
        Self {
            write_ready: routing_table_size.max(connected_peers) >= REBOOTSTRAP_THRESHOLD,
            connected_peers: connected_peers.try_into().unwrap_or(u32::MAX),
            routing_table_size: routing_table_size.try_into().unwrap_or(u32::MAX),
            rebootstrap_threshold: REBOOTSTRAP_THRESHOLD as u32,
        }
    }
}

/// Read-only DHT context captured for one diagnostics-enabled closest-peer
/// selection. None of these fields influence selection or dialing.
pub(crate) struct ClosestPeerDiagnostics {
    pub peer_id: PeerId,
    pub addresses: Vec<MultiAddr>,
    pub address_types: Vec<String>,
    /// This process's monotonic age since its last successful DHT interaction.
    pub local_last_seen_age_ms: Option<u64>,
    /// Publisher-clock-derived age of the latest address-set publication.
    pub publisher_address_set_age_ms: Option<u64>,
    pub publisher_address_set_unix_ns: Option<u64>,
}

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

    /// Find the same peers, in the same order, while capturing read-only DHT
    /// context for the explicitly enabled download diagnostics sidecar.
    pub(crate) async fn find_closest_peers_with_diagnostics(
        &self,
        target: &[u8; 32],
        count: usize,
    ) -> Result<Vec<ClosestPeerDiagnostics>> {
        let local_peer_id = self.node.peer_id();
        let closest_nodes = self
            .node
            .dht()
            .find_closest_nodes(target, count + 1)
            .await
            .map_err(|e| Error::Network(format!("DHT closest-nodes lookup failed: {e}")))?;
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let now_ns = u64::try_from(now_ns).unwrap_or(u64::MAX);

        let mut result = Vec::with_capacity(count);
        for node in closest_nodes
            .into_iter()
            .filter(|node| node.peer_id != *local_peer_id)
            .take(count)
        {
            let publisher_address_set_unix_ns = node.publisher_address_set_unix_ns();
            // A publisher clock may be ahead of ours. In that case, retain the
            // raw timestamp but do not misreport its age as zero.
            let publisher_address_set_age_ms = publisher_address_set_unix_ns
                .and_then(|published| now_ns.checked_sub(published))
                .map(|age_ns| age_ns / 1_000_000);
            let local_last_seen_age_ms = self
                .node
                .peer_last_seen_elapsed(&node.peer_id)
                .await
                .map(|age| u64::try_from(age.as_millis()).unwrap_or(u64::MAX));
            let address_context = node.address_and_type_labels_by_priority();
            let (addresses, address_types) = address_context
                .into_iter()
                .map(|(address, label)| (address, label.to_string()))
                .unzip();
            result.push(ClosestPeerDiagnostics {
                peer_id: node.peer_id,
                addresses,
                address_types,
                local_last_seen_age_ms,
                publisher_address_set_age_ms,
                publisher_address_set_unix_ns,
            });
        }
        Ok(result)
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
    pub async fn connected_peers(&self) -> Vec<PeerId> {
        self.node.connected_peers().await
    }

    /// Compute the live network-participation snapshot.
    ///
    /// Both node reads are in-memory, so this is cheap enough to call per
    /// request — no caching or background worker needed. See
    /// [`NetworkHealth::from_counts`] for the `write_ready` semantics.
    ///
    /// Do not substitute `is_bootstrapped()` (sticky true — it stays true
    /// through a total outage) or saorsa's `health_check()` (an
    /// over-connection guard, despite the name) for this.
    pub async fn health(&self) -> NetworkHealth {
        let connected_peers = self.node.peer_count().await;
        let routing_table_size = self.node.dht_manager().get_routing_table_size().await;
        NetworkHealth::from_counts(connected_peers, routing_table_size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_ready_false_with_no_peers() {
        let h = NetworkHealth::from_counts(0, 0);
        assert!(!h.write_ready);
        assert_eq!(h.connected_peers, 0);
        assert_eq!(h.routing_table_size, 0);
        assert_eq!(h.rebootstrap_threshold, REBOOTSTRAP_THRESHOLD as u32);
    }

    #[test]
    fn write_ready_false_below_threshold_on_both_signals() {
        // The reporter's incident shape (ant-sdk#232): ~1 reachable peer,
        // empty routing table, stores failing.
        assert!(!NetworkHealth::from_counts(1, 0).write_ready);
        assert!(!NetworkHealth::from_counts(2, 2).write_ready);
    }

    #[test]
    fn write_ready_true_via_connections_despite_low_routing_table() {
        // Client-mode under-reporting observed live on a LAN devnet:
        // rt pinned at 2 with 10 verified connections and stores succeeding.
        // The max() in the formula exists for exactly this state.
        assert!(NetworkHealth::from_counts(10, 2).write_ready);
    }

    #[test]
    fn write_ready_true_via_routing_table_alone() {
        assert!(NetworkHealth::from_counts(0, REBOOTSTRAP_THRESHOLD).write_ready);
    }

    #[test]
    fn write_ready_true_at_exact_threshold_on_connections() {
        assert!(NetworkHealth::from_counts(REBOOTSTRAP_THRESHOLD, 0).write_ready);
    }

    #[test]
    fn counts_saturate_at_u32_max() {
        let h = NetworkHealth::from_counts(usize::MAX, usize::MAX);
        assert_eq!(h.connected_peers, u32::MAX);
        assert_eq!(h.routing_table_size, u32::MAX);
        assert!(h.write_ready);
    }
}
