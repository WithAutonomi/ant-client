//! Bootstrap-cache population helpers.
//!
//! Wires client-side peer contacts into saorsa-core's `BootstrapManager`
//! so the persistent cache reflects real peer quality across sessions.

use ant_protocol::transport::{MultiAddr, P2PNode, PeerId};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tracing::debug;

/// Feed a peer contact outcome into the `BootstrapManager` cache so future
/// cold-starts can rank peers by observed latency and success.
///
/// `success = true`: upserts the peer via `add_discovered_peer` (subject to
/// saorsa-core Sybil checks — rate limit + IP diversity) and records RTT via
/// `update_peer_metrics`.
///
/// `success = false`: only updates the quality score of peers already in
/// the cache. Unreachable peers are never inserted.
///
/// Both upstream calls silently discard errors — peer-cache bookkeeping
/// must never abort a user operation. Enable the `saorsa_core::bootstrap`
/// tracing target to see rejection reasons.
pub(crate) async fn record_peer_outcome(
    node: &Arc<P2PNode>,
    peer_id: PeerId,
    addrs: &[MultiAddr],
    success: bool,
    rtt_ms: Option<u64>,
) {
    if success {
        let before = node.cached_peer_count().await;
        let _ = node.add_discovered_peer(peer_id, addrs.to_vec()).await;
        let after = node.cached_peer_count().await;
        if after > before {
            debug!("Bootstrap cache grew: {before} -> {after} peers");
        }
    }
    if let Some(primary) = select_primary_multiaddr(addrs) {
        let _ = node
            .update_peer_metrics(primary, success, rtt_ms, None)
            .await;
    }
}

/// Pick the `MultiAddr` to use as the peer's cache key.
///
/// Prefers a globally routable socket address over RFC1918 / link-local /
/// loopback. Without this, a peer advertising `[10.0.0.5, 203.0.113.1]`
/// would be keyed under the RFC1918 address, so metrics recorded during
/// a contact over the public address would land on a stale cache entry.
/// Falls back to any socket-addressable `MultiAddr` if none look global.
fn select_primary_multiaddr(addrs: &[MultiAddr]) -> Option<&MultiAddr> {
    addrs
        .iter()
        .find(|a| a.socket_addr().is_some_and(|sa| is_globally_routable(&sa)))
        .or_else(|| addrs.iter().find(|a| a.socket_addr().is_some()))
}

fn is_globally_routable(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(v4) => {
            !v4.is_private()
                && !v4.is_loopback()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_documentation()
                && !v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            // Full Ipv6Addr::is_global is unstable; this is the practical
            // subset that mirrors the IPv4 checks above.
            !v6.is_loopback()
                && !v6.is_unspecified()
                && !v6.is_multicast()
                && !v6.segments()[0].eq(&0xfe80) // link-local fe80::/10 (approx)
                && !matches!(v6.segments()[0] & 0xfe00, 0xfc00) // unique-local fc00::/7
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    #[test]
    fn globally_routable_v4() {
        // 8.8.8.8 (Google DNS) — genuinely public, not in any reserved range.
        assert!(is_globally_routable(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8)),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5)),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)),
            80
        )));
        // 203.0.113.0/24 is TEST-NET-3 documentation — rejected by
        // `is_documentation()`, which is the behaviour we want: quality
        // metrics should not land on addresses that are never dialed in
        // production by spec.
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(203, 0, 113, 1)),
            80
        )));
    }

    #[test]
    fn globally_routable_v6() {
        // 2606:4700:4700::1111 (Cloudflare DNS) — a real public v6 outside
        // the `2001:db8::/32` documentation prefix.
        assert!(is_globally_routable(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111)),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1)),
            80
        )));
        assert!(!is_globally_routable(&SocketAddr::new(
            IpAddr::V6(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 1)),
            80
        )));
    }
}
