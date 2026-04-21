//! E2E tests for BootstrapManager cache population from real peer interactions.
//!
//! Proves that client-side uploads and downloads feed the BootstrapManager
//! cache via `add_discovered_peer` + `update_peer_metrics`, so that subsequent
//! cold-starts can load quality-scored peers beyond the bundled bootstrap set.
//!
//! ## Why the assertion is "cache grew", not "cache >= 10"
//!
//! saorsa-core gates `add_peer` through two independent Sybil mechanisms:
//!
//! 1. `BootstrapIpLimiter::can_accept` — the IP-diversity limiter. When the
//!    node is built with `allow_loopback = true` (as `MiniTestnet` does),
//!    this returns early for loopback IPs, so it is NOT the bottleneck here.
//! 2. `JoinRateLimiter::check_join_allowed` — the temporal rate limiter.
//!    Defaults cap inserts at 3 per /24 subnet per hour and are NOT exempt
//!    for loopback (`saorsa-core/src/rate_limit.rs:254` has no `is_loopback`
//!    branch). All testnet nodes bind to `127.0.0.1`, so all ~11 available
//!    peers fall in the single `127.0.0.0/24` bucket — the first 3 land in
//!    the cache, the rest are rejected with `Subnet24LimitExceeded`.
//!
//! In production, peers span many /24s (typically one per ASN), so the /24
//! rate limit is never the binding constraint and crossing
//! `min_peers_to_save = 10` is straightforward.
//!
//! Asserting `after > before` is sufficient proof that the client library
//! correctly wires `add_discovered_peer` and `update_peer_metrics` into the
//! upload (and, transitively, download) paths. The threshold-crossing +
//! persistence behavior is an upstream contract covered by saorsa-transport's
//! own tests.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, ClientConfig};
use bytes::Bytes;
use serial_test::serial;
use std::sync::Arc;
use support::MiniTestnet;

const BOOTSTRAP_CACHE_TEST_NODES: usize = 12;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_bootstrap_cache_grows_after_client_activity() {
    let testnet = MiniTestnet::start(BOOTSTRAP_CACHE_TEST_NODES).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    let before = node.cached_peer_count().await;

    let content = Bytes::from("bootstrap-cache e2e payload");
    let address = client
        .chunk_put(content.clone())
        .await
        .expect("chunk_put should succeed with payment");

    // The GET exercises the download-side hook (chunk_get_from_peer), which
    // would silently break if record_peer_outcome's signature drifted from
    // what chunk.rs expects. The assertion here is just that the round-trip
    // works — cache growth from the GET itself is capped by the /24 rate
    // limiter which saturated during the PUT.
    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed")
        .expect("chunk should be retrievable");
    assert_eq!(retrieved.content.as_ref(), content.as_ref());

    let after = node.cached_peer_count().await;
    assert!(
        after > before,
        "cache should grow after peer interactions: before={before} after={after}"
    );

    drop(client);
    testnet.teardown().await;
}

/// Cold-start-from-disk round-trip.
///
/// ## What this proves
///
/// - A populated `BootstrapManager` cache with ≥ `min_peers_to_save` peers
///   is persisted to disk on `save()`.
/// - A *fresh* `BootstrapManager` constructed against the same `cache_dir`
///   reloads the persisted peers on startup.
///
/// Together with `test_bootstrap_cache_grows_after_client_activity` above
/// (which exercises the add-during-activity hook), this closes the loop on
/// the V2-202 value prop: cold-start clients reload real peers from disk.
///
/// ## Why `add_peer_trusted` and not `add_discovered_peer`
///
/// `add_discovered_peer` goes through `BootstrapManager::add_peer`, which
/// runs both the IP-diversity limiter and the temporal `JoinRateLimiter`.
/// The latter caps inserts at 3 per /24 subnet per hour and has no
/// loopback exemption. A real test that populates 15 peers through that
/// path would need peers on distinct /24s — not practical on a single-host
/// testnet. `add_peer_trusted` skips both limiters and talks to the same
/// underlying `BootstrapCache::add_seed` that our hooks ultimately feed,
/// so the persistence path exercised is identical to production's.
#[tokio::test]
async fn test_bootstrap_cache_roundtrip_through_disk() {
    use saorsa_core::{BootstrapConfig, BootstrapManager};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    let cache_dir = tempfile::TempDir::new().expect("create temp cache dir");
    let config = BootstrapConfig {
        cache_dir: cache_dir.path().to_path_buf(),
        ..BootstrapConfig::default()
    };

    // Populate with peers on distinct /24s (cosmetic — add_peer_trusted
    // skips rate limits — but keeps the data realistic if saorsa-transport
    // ever tightens its invariants).
    let peer_count = 15;
    {
        let mgr = BootstrapManager::with_config(config.clone())
            .await
            .expect("construct populating BootstrapManager");
        for i in 0..peer_count {
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, i as u8 + 1)), 9000);
            mgr.add_peer_trusted(&addr, vec![addr]).await;
        }
        assert_eq!(mgr.peer_count().await, peer_count, "in-memory populate");
        mgr.save()
            .await
            .expect("save should succeed above threshold");
    }

    // Fresh manager, same cache_dir: peers should be reloaded.
    let reloaded = BootstrapManager::with_config(config)
        .await
        .expect("construct reloading BootstrapManager");
    let reloaded_count = reloaded.peer_count().await;
    assert_eq!(
        reloaded_count, peer_count,
        "all {peer_count} peers should reload from disk, got {reloaded_count}"
    );
}
