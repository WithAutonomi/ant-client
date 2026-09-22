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
use serde::{Deserialize, Serialize};
#[cfg(feature = "native")]
use std::net::SocketAddr;
#[cfg(feature = "native")]
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
#[cfg(feature = "native")]
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
#[derive(Clone)]
pub struct Network {
    #[cfg(feature = "native")]
    node: Arc<P2PNode>,
    #[cfg(not(feature = "native"))]
    backend: std::rc::Rc<dyn BrowserNetwork>,
}

/// Peer identities and the addresses that can reach them.
pub type PeerAddresses = Vec<(PeerId, Vec<MultiAddr>)>;

/// Bounded, latest-value hints for an immutable read in progress. Hints only
/// start authenticated GETs; they never establish closeness, absence or payment
/// authority. The receiver may cancel discovery after verifying content.
#[derive(Clone)]
pub struct ReadProgress {
    target: [u8; 32],
    local: PeerId,
    sender: tokio::sync::watch::Sender<PeerAddresses>,
}

impl ReadProgress {
    pub(crate) fn new(
        target: [u8; 32],
        local: PeerId,
        sender: tokio::sync::watch::Sender<PeerAddresses>,
    ) -> Self {
        Self {
            target,
            local,
            sender,
        }
    }

    /// Offer authenticated discovery hints, retaining only the nearest bounded
    /// set. Sending never waits for a slow read consumer.
    pub fn offer(&self, peers: PeerAddresses) {
        if self.sender.is_closed() {
            return;
        }
        self.sender.send_modify(|current| {
            for peer in peers {
                if peer.0 == self.local || peer.1.is_empty() {
                    continue;
                }
                if let Some(existing) = current.iter_mut().find(|entry| entry.0 == peer.0) {
                    *existing = peer;
                } else {
                    current.push(peer);
                }
            }
            current.sort_by_key(|peer| {
                ant_protocol::transport::xor_distance(peer.0.as_bytes(), &self.target)
            });
            current.truncate(crate::client_engine::read::MAX_GET_FALLBACK_PEERS);
        });
    }
}

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
    /// Read-only discovery may report verified candidates before completing.
    /// Existing adapters remain compatible and supply their final result only.
    fn find_read_peers<'a>(
        &'a self,
        target: &'a [u8; 32],
        count: usize,
        _progress: ReadProgress,
    ) -> futures::future::LocalBoxFuture<'a, Result<PeerAddresses>> {
        self.find_closest_peers(target, count)
    }
    /// Authenticated responder views for witnessed quote admission.
    fn find_witnessed_close_group<'a>(
        &'a self,
        target: &'a [u8; 32],
        count: usize,
        view_count: usize,
    ) -> futures::future::LocalBoxFuture<'a, Result<WitnessedCloseGroup>>;
    /// Known records used as fallback candidates after a lookup failure.
    fn known_peers(&self) -> Vec<DHTNode>;
    /// Authenticated live peers eligible for an opportunistic immutable read.
    /// Adapters without connection telemetry retain discovery-first behavior.
    fn connected_read_peers(&self) -> Vec<PeerId> {
        Vec::new()
    }
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
        let seeds: Vec<_> = bootstrap_peers
            .iter()
            .copied()
            .map(MultiAddr::quic)
            .collect();
        Self::new_multiaddrs(&seeds, allow_loopback, ipv6).await
    }

    /// Connect using QUIC multiaddresses, preserving optional peer identity pins.
    #[cfg(feature = "native")]
    pub async fn new_multiaddrs(
        bootstrap_peers: &[MultiAddr],
        allow_loopback: bool,
        ipv6: bool,
    ) -> Result<Self> {
        let seeds = bootstrap_peers
            .iter()
            .map(|addr| crate::network_defaults::parse_quic_seed(&addr.to_string()))
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| Error::Network(e.to_string()))?;
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

        core_config.bootstrap_peers = seeds;

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

    /// Find the same peers, in the same order, while capturing read-only DHT
    /// context for the explicitly enabled download diagnostics sidecar.
    #[cfg(feature = "native")]
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

    /// Compute the live network-participation snapshot.
    ///
    /// Both node reads are in-memory, so this is cheap enough to call per
    /// request — no caching or background worker needed. See
    /// [`NetworkHealth::from_counts`] for the `write_ready` semantics.
    ///
    /// Do not substitute `is_bootstrapped()` (sticky true — it stays true
    /// through a total outage) or saorsa's `health_check()` (an
    /// over-connection guard, despite the name) for this.
    #[cfg(feature = "native")]
    pub async fn health(&self) -> NetworkHealth {
        let connected_peers = self.node.peer_count().await;
        let routing_table_size = self.node.dht_manager().get_routing_table_size().await;
        NetworkHealth::from_counts(connected_peers, routing_table_size)
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

    /// Seed early reads from the same local phonebook on both platforms.
    pub(crate) async fn seed_read_candidates(&self, progress: &ReadProgress) {
        progress.offer(
            self.known_peers()
                .await
                .into_iter()
                .map(|node| {
                    let addrs = node.addresses_by_priority();
                    (node.peer_id, addrs)
                })
                .collect(),
        );
    }

    #[cfg(not(feature = "native"))]
    pub(crate) async fn find_read_peers(
        &self,
        target: &[u8; 32],
        count: usize,
        progress: ReadProgress,
    ) -> Result<PeerAddresses> {
        self.backend.find_read_peers(target, count, progress).await
    }
}

/// Execute a request through the platform adapter and apply the shared response handler.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn send_and_await_chunk_response<T, E: From<crate::data::error::Error>>(
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
            // Preserve browser phase diagnostics and timeout classification. Queue
            // and send expiry must not be reported as a ten-second store wait.
            Err(error @ crate::data::error::Error::Timeout(_)) => return Err(error.into()),
            Err(error) => return Err(send_error(error.to_string())),
        };
        if response.request_id != request_id {
            return Err(timeout_error());
        }
        response_handler(response.body).unwrap_or_else(|| Err(timeout_error()))
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
