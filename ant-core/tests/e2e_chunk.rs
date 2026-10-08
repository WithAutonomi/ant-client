//! E2E tests for chunk operations using a local testnet with real EVM payments.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client};
use bytes::Bytes;
use futures::future::join_all;
use serial_test::serial;
use std::sync::Arc;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    (client, testnet)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_get_round_trip() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("ant-core chunk e2e test payload");
    let address = client
        .chunk_put(content.clone())
        .await
        .expect("chunk_put should succeed with payment");

    let expected_address = compute_address(&content);
    assert_eq!(
        address, expected_address,
        "address should be BLAKE3(content)"
    );

    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");

    let chunk = retrieved.expect("Chunk should be found after storing it");
    assert_eq!(chunk.content.as_ref(), content.as_ref());
    assert_eq!(chunk.address, address);

    drop(client);
    testnet.teardown().await;
}

/// Concurrent reads of one chunk share a single fetch: with the cache empty,
/// every concurrent `chunk_get` returns the chunk and no node is asked for it
/// more than once. Without sharing, each reader sends its own GETs and the
/// first holder is asked once per reader.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_concurrent_chunk_gets_share_one_fetch() {
    const READERS: usize = 8;
    let (client, testnet) = setup().await;

    let content = Bytes::from("ant-core concurrent chunk_get payload");
    let address = client
        .chunk_put(content.clone())
        .await
        .expect("chunk_put should succeed with payment");
    client.chunk_cache().clear();
    let before = testnet.chunk_gets_received(&address);

    let results = join_all((0..READERS).map(|_| client.chunk_get(&address))).await;
    for result in results {
        let chunk = result
            .expect("chunk_get should succeed")
            .expect("Chunk should be found after storing it");
        assert_eq!(chunk.content.as_ref(), content.as_ref());
    }

    testnet.flush_chunk_gets(client.network().node()).await;
    let sent: Vec<usize> = testnet
        .chunk_gets_received(&address)
        .iter()
        .zip(&before)
        .map(|(after, before)| after - before)
        .collect();
    assert!(
        sent.iter().sum::<usize>() >= 1,
        "the chunk should come from the network: {sent:?}"
    );
    assert!(
        sent.iter().all(|&gets| gets <= 1),
        "{READERS} concurrent readers should ask each node at most once: {sent:?}"
    );

    drop(client);
    testnet.teardown().await;
}

/// ADR-0002: when initial PUT targets fail, the client falls back to the
/// chunk's genuine next-closest peers — reusing the same `ProofOfPayment`,
/// without re-quoting or re-paying — and still reaches quorum.
///
/// Forces `DEAD_INITIAL_PEERS` (= `CLOSE_GROUP_MAJORITY`) unreachable initial
/// peers, so every initial send fails and a full quorum's worth of stores must
/// come from the *real* next-closest peers in the put-target set. A successful,
/// retrievable store proves the fallback reached those real peers on the single
/// proof produced by one payment.
///
/// Scope: failures here are driven by *unreachable* peers (transport), which
/// exercises the fallback walk + single-proof reuse + quorum end-to-end. The
/// *classification* that routes a full node's `StorageFailed` and a price-floor
/// `PaymentFailed`/`PaymentRequired` into the same fallback is covered by the
/// `classify_put_failure_*` unit test. A live close-group node returning
/// `StorageFailed` (a genuinely full member) needs a node-side capacity hook the
/// in-process MiniTestnet does not expose — tracked as a follow-up.
#[cfg(feature = "test-utils")]
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_extended_fallback_reaches_quorum() {
    /// Unreachable initial peers to inject; equals `CLOSE_GROUP_MAJORITY` so a
    /// full quorum must be served by the extended fallback.
    const DEAD_INITIAL_PEERS: usize = 4;

    let (client, testnet) = setup().await;

    let content = Bytes::from("adr-0002 extended fallback payload");
    let address = compute_address(&content);

    let stored = client
        .chunk_put_with_dead_initial_peers(content.clone(), DEAD_INITIAL_PEERS)
        .await
        .expect("extended fallback should reach quorum despite dead initial peers");
    assert_eq!(stored, address, "stored address should be BLAKE3(content)");

    // Retrievable -> quorum was genuinely met via the fallback peers.
    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");
    let chunk = retrieved.expect("chunk should be found after fallback store");
    assert_eq!(chunk.content.as_ref(), content.as_ref());

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_duplicate_is_idempotent() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("duplicate chunk test");
    let addr1 = client
        .chunk_put(content.clone())
        .await
        .expect("first put should succeed");

    // Second put — node sees AlreadyExists, returns success
    let addr2 = client
        .chunk_put(content.clone())
        .await
        .expect("duplicate put should succeed");

    assert_eq!(addr1, addr2, "duplicate put should return same address");

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_get_nonexistent_returns_none() {
    let (client, testnet) = setup().await;

    let missing_address = [0xDE; 32];
    let result = client
        .chunk_get(&missing_address)
        .await
        .expect("get for missing address should not error");

    assert!(
        result.is_none(),
        "Should return None for non-existent chunk"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_exists() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("exists check test");
    let address = client.chunk_put(content).await.expect("put should succeed");

    let exists = client
        .chunk_exists(&address)
        .await
        .expect("exists should succeed");
    assert!(exists, "exists() should return true for stored chunk");

    let missing = [0xAA; 32];
    let not_exists = client
        .chunk_exists(&missing)
        .await
        .expect("exists for missing should succeed");
    assert!(
        !not_exists,
        "exists() should return false for missing chunk"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_with_insufficient_proof_rejected() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("this should be rejected — insufficient proof");
    let address = compute_address(&content);

    // Send a too-short proof (not even valid msgpack)
    let insufficient_proof = vec![0x00; 16];
    let (target_peer, target_addrs) = client
        .network()
        .find_closest_peers(&address, 1)
        .await
        .expect("should find peers")
        .into_iter()
        .next()
        .expect("should have at least one peer");
    let result = client
        .chunk_put_with_proof(content, insufficient_proof, &target_peer, &target_addrs)
        .await;

    assert!(
        result.is_err(),
        "PUT with insufficient proof should be rejected"
    );
    let err_msg = format!("{}", result.expect_err("should have error"));
    let err_lower = err_msg.to_lowercase();
    assert!(
        err_lower.contains("payment") || err_lower.contains("error"),
        "Error should be payment-related, got: {err_msg}"
    );

    // Verify the chunk was NOT stored on the network
    let get_result = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");
    assert!(
        get_result.is_none(),
        "Rejected chunk should not be stored on the network"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_get_is_always_free() {
    let (client, testnet) = setup().await;

    // First, store a chunk with payment
    let content = Bytes::from("chunk for free-get test");
    let address = client
        .chunk_put(content.clone())
        .await
        .expect("paid put should succeed");

    // Create a client WITHOUT a wallet using the SAME P2P node.
    // Reads are free so the wallet absence should not matter.
    // Using the same node ensures the DHT routes to the same storing peer.
    let node = testnet.node(3).expect("Node 3 should exist");
    let no_wallet_client = Client::from_node(Arc::clone(&node), test_client_config());

    let retrieved = no_wallet_client
        .chunk_get(&address)
        .await
        .expect("GET without wallet should succeed (reads are free)");

    let chunk = retrieved.expect("Chunk should be found");
    assert_eq!(chunk.content.as_ref(), content.as_ref());

    drop(client);
    drop(no_wallet_client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_with_invalid_proof_rejected() {
    let (client, testnet) = setup().await;

    // Build a garbage proof
    let content = Bytes::from("chunk with invalid proof");
    let address = compute_address(&content);
    let invalid_proof = vec![0xDE, 0xAD, 0xBE, 0xEF];

    let (target_peer, target_addrs) = client
        .network()
        .find_closest_peers(&address, 1)
        .await
        .expect("should find peers")
        .into_iter()
        .next()
        .expect("should have at least one peer");
    let result = client
        .chunk_put_with_proof(content, invalid_proof, &target_peer, &target_addrs)
        .await;

    // The node should reject this — either a deserialization error or payment verification failure
    assert!(result.is_err(), "PUT with invalid proof should be rejected");

    // Verify the chunk was NOT stored on the network
    let get_result = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");
    assert!(
        get_result.is_none(),
        "Chunk with invalid proof should not be stored on the network"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_no_wallet_fails() {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Client WITHOUT wallet
    let client = Client::from_node(Arc::clone(&node), test_client_config());

    let content = Bytes::from("chunk_put without wallet test");
    let result = client.chunk_put(content).await;

    assert!(result.is_err(), "chunk_put without wallet should fail");
    let err_msg = format!("{}", result.expect_err("should have error"));
    let err_lower = err_msg.to_lowercase();
    assert!(
        err_lower.contains("wallet"),
        "Error should mention wallet, got: {err_msg}"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_duplicate_skips_payment() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("duplicate payment prevention test");

    // First put — should succeed with payment
    let addr1 = client
        .chunk_put(content.clone())
        .await
        .expect("first put should succeed");

    // Get wallet balance BEFORE the second put
    let balance_before = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    // Second put of same content — should detect existence and skip payment
    let addr2 = client
        .chunk_put(content)
        .await
        .expect("duplicate put should succeed");

    assert_eq!(addr1, addr2, "duplicate put should return same address");

    // Wallet balance should be unchanged (no on-chain payment for the duplicate)
    let balance_after = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    assert_eq!(
        balance_before, balance_after,
        "duplicate chunk_put should not spend any tokens"
    );

    drop(client);
    testnet.teardown().await;
}
