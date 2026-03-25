//! E2E tests for chunk operations using a local testnet with real EVM payments.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client, ClientConfig};
use bytes::Bytes;
use serial_test::serial;
use std::sync::Arc;
use support::MiniTestnet;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
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
    let no_wallet_client = Client::from_node(Arc::clone(&node), ClientConfig::default());

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
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Client WITHOUT wallet
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default());

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
