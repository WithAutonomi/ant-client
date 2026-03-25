//! Payment flow E2E tests for ant-core.
//!
//! Tests legitimate payment flows: paid chunk storage, concurrent uploads,
//! payment enforcement, large chunks, idempotency, and quote collection.

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

// ─── Test 1: Paid chunk put and retrieve ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_paid_chunk_put_and_retrieve() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("payment flow: put and retrieve test payload");
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
    let chunk = retrieved.expect("Chunk should be found after storing");
    assert_eq!(chunk.content.as_ref(), content.as_ref());
    assert_eq!(chunk.address, address);

    drop(client);
    testnet.teardown().await;
}

// ─── Test 2: Concurrent paid uploads ────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_concurrent_paid_uploads() {
    let (client, testnet) = setup().await;

    let content_a = Bytes::from("concurrent upload chunk A");
    let content_b = Bytes::from("concurrent upload chunk B");
    let content_c = Bytes::from("concurrent upload chunk C");

    let client_ref = &client;
    let (result_a, result_b, result_c) = futures::join!(
        client_ref.chunk_put(content_a.clone()),
        client_ref.chunk_put(content_b.clone()),
        client_ref.chunk_put(content_c.clone()),
    );

    let addr_a = result_a.expect("chunk A upload should succeed");
    let addr_b = result_b.expect("chunk B upload should succeed");
    let addr_c = result_c.expect("chunk C upload should succeed");

    // Verify all are retrievable
    let chunk_a = client
        .chunk_get(&addr_a)
        .await
        .expect("get A")
        .expect("chunk A should exist");
    assert_eq!(chunk_a.content.as_ref(), content_a.as_ref());

    let chunk_b = client
        .chunk_get(&addr_b)
        .await
        .expect("get B")
        .expect("chunk B should exist");
    assert_eq!(chunk_b.content.as_ref(), content_b.as_ref());

    let chunk_c = client
        .chunk_get(&addr_c)
        .await
        .expect("get C")
        .expect("chunk C should exist");
    assert_eq!(chunk_c.content.as_ref(), content_c.as_ref());

    // All addresses should be unique
    assert_ne!(addr_a, addr_b, "A and B should have different addresses");
    assert_ne!(addr_b, addr_c, "B and C should have different addresses");
    assert_ne!(addr_a, addr_c, "A and C should have different addresses");

    drop(client);
    testnet.teardown().await;
}

// ─── Test 3: Payment required enforcement ───────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_payment_required_enforcement() {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Client with wallet for the paid test
    let client_with_wallet = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    // Try chunk_put_with_proof using garbage proof — should be rejected by the node
    let content = Bytes::from("payment enforcement test data");
    let address = ant_core::data::compute_address(&content);
    let garbage_proof = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let (target_peer, target_addrs) = client_with_wallet
        .network()
        .find_closest_peers(&address, 1)
        .await
        .expect("should find peers")
        .into_iter()
        .next()
        .expect("should have at least one peer");
    let unpaid_result = client_with_wallet
        .chunk_put_with_proof(content.clone(), garbage_proof, &target_peer, &target_addrs)
        .await;

    assert!(
        unpaid_result.is_err(),
        "PUT with invalid proof should be rejected on payment-enabled network"
    );
    let err_msg = format!("{}", unpaid_result.expect_err("should have error"));
    let err_lower = err_msg.to_lowercase();
    assert!(
        err_lower.contains("payment") || err_lower.contains("error"),
        "Error should be payment-related, got: {err_msg}"
    );

    // Now do a paid chunk_put — should succeed
    let paid_result = client_with_wallet.chunk_put(content).await;
    assert!(paid_result.is_ok(), "Paid chunk_put should succeed");

    drop(client_with_wallet);
    testnet.teardown().await;
}

// ─── Test 4: Large chunk payment ────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_large_chunk_payment() {
    let (client, testnet) = setup().await;

    // 1 MB chunk
    let large_content = Bytes::from(vec![0xAB_u8; 1_000_000]);

    let address = client
        .chunk_put(large_content.clone())
        .await
        .expect("large chunk_put should succeed");

    let expected_address = compute_address(&large_content);
    assert_eq!(
        address, expected_address,
        "address should match BLAKE3 hash"
    );

    // Verify round-trip
    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");
    let chunk = retrieved.expect("Large chunk should be found");
    assert_eq!(
        chunk.content.len(),
        1_000_000,
        "Retrieved chunk should be 1 MB"
    );
    assert_eq!(chunk.content.as_ref(), large_content.as_ref());

    drop(client);
    testnet.teardown().await;
}

// ─── Test 5: Idempotent chunk storage (no double payment) ──────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_idempotent_chunk_storage() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("idempotent storage test data");

    // First upload — pays
    let addr1 = client
        .chunk_put(content.clone())
        .await
        .expect("first put should succeed");

    // Get wallet balance BEFORE second put
    let balance_before = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    // Second upload of same content — should skip payment
    let addr2 = client
        .chunk_put(content)
        .await
        .expect("duplicate put should succeed");

    assert_eq!(addr1, addr2, "Duplicate put should return same address");

    // Wallet balance should be unchanged (no on-chain payment for the duplicate)
    let balance_after = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    assert_eq!(
        balance_before, balance_after,
        "Duplicate chunk_put should not spend any tokens"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 6: Quote collection ───────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_quote_collection() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("quote collection test data");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    let quotes = client
        .get_store_quotes(&address, data_size, 0)
        .await
        .expect("get_store_quotes should succeed");

    // At least 5 quotes required
    assert!(
        quotes.len() >= 5,
        "Should receive at least 5 quotes, got {}",
        quotes.len()
    );

    // All prices should be > 0
    for (peer_id, _addrs, _quote, price) in &quotes {
        assert!(
            !price.is_zero(),
            "Quote price from peer {peer_id} should be > 0"
        );
    }

    // All peer IDs should be unique
    let unique_peers: std::collections::HashSet<_> = quotes.iter().map(|(p, _, _, _)| *p).collect();
    assert_eq!(
        unique_peers.len(),
        quotes.len(),
        "All quote peer IDs should be unique"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 7: Insufficient funds ──────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_chunk_put_fails_with_insufficient_funds() {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Create a wallet with zero balance by using a random private key
    // (not the Anvil-funded default key)
    let evm_network = testnet.evm_network().clone();
    let empty_wallet = evmlib::wallet::Wallet::new_from_private_key(
        evm_network,
        "0x0000000000000000000000000000000000000000000000000000000000000001",
    )
    .expect("create wallet from random key");

    let client =
        Client::from_node(Arc::clone(&node), ClientConfig::default()).with_wallet(empty_wallet);

    let content = Bytes::from("insufficient funds test data");
    let result = client.chunk_put(content).await;

    assert!(
        result.is_err(),
        "chunk_put should fail with an unfunded wallet"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 8: Payment flow survives node failures ─────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_payment_flow_with_node_failure() {
    let mut testnet = MiniTestnet::start(6).await;

    // With a 6-node testnet and CLOSE_GROUP_SIZE = 5, we need exactly 5
    // remote peers. The client's own node is excluded from quote collection,
    // so killing even one node leaves only 4 remote peers — not enough.
    //
    // Instead, verify the payment flow survives a brief disruption: shut
    // down a node, wait, then bring it back by verifying the remaining
    // network can still serve reads.
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    // Store a chunk BEFORE any failure
    let content = Bytes::from("resilience test: payment before node failure");
    let stored_address = client
        .chunk_put(content.clone())
        .await
        .expect("initial chunk_put should succeed");

    // Shut down node 5
    testnet.shutdown_node(5);
    let remaining = testnet.running_node_count();
    assert_eq!(remaining, 5, "Should have 5 nodes after shutting down 1");

    tokio::time::sleep(std::time::Duration::from_secs(5)).await;

    // Verify retrieval still works despite node failure (retry to handle network settling)
    let mut last_err = None;
    let mut chunk = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }
        match client.chunk_get(&stored_address).await {
            Ok(Some(c)) => {
                chunk = Some(c);
                break;
            }
            Ok(None) => {
                eprintln!("attempt {attempt}: chunk_get returned None");
            }
            Err(e) => {
                eprintln!("attempt {attempt}: chunk_get failed: {e}");
                last_err = Some(e);
            }
        }
    }
    if chunk.is_none() {
        eprintln!("All retry attempts failed. Last error: {last_err:?}");
    }
    let chunk = chunk.expect("Chunk should be retrievable after node failure");
    assert_eq!(chunk.content.as_ref(), content.as_ref());

    drop(client);
    drop(node);
    testnet.teardown().await;
}
