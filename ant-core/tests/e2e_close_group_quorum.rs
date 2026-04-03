//! E2E tests for close group quorum validation with signed quotes.
//!
//! Verifies the full flow: quotes contain a signed close_group field,
//! the client validates quorum using those views, payment succeeds,
//! and nodes accept the proof (including close group membership checks).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client, ClientConfig};
use ant_node::{CLOSE_GROUP_MAJORITY, CLOSE_GROUP_SIZE};
use bytes::Bytes;
use evmlib::PaymentQuote;
use serial_test::serial;
use std::collections::HashSet;
use std::sync::Arc;
use support::MiniTestnet;

const CHUNK_DATA_TYPE: u32 = 0;
/// More nodes than DEFAULT_NODE_COUNT to tolerate the 2x over-query.
/// With only CLOSE_GROUP_SIZE+1 nodes, a single peer timeout causes
/// failure since there are zero spare peers.
const QUORUM_TEST_NODE_COUNT: usize = 10;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(QUORUM_TEST_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());
    (client, testnet)
}

// ─── Test 1: Quotes contain non-empty signed close_group ───────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_quotes_contain_signed_close_group() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("close group quorum test payload");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    let quotes = client
        .get_store_quotes(&address, data_size, CHUNK_DATA_TYPE)
        .await
        .expect("get_store_quotes should succeed");

    assert_eq!(
        quotes.len(),
        CLOSE_GROUP_SIZE,
        "should get exactly CLOSE_GROUP_SIZE quotes"
    );

    // Every quote must have a non-empty close_group field
    for (peer_id, _addrs, quote, _price) in &quotes {
        assert!(
            !quote.close_group.is_empty(),
            "Quote from {peer_id} should have non-empty close_group"
        );
    }

    drop(client);
    testnet.teardown().await;
}

// ─── Test 2: Close group views have mutual recognition (quorum) ────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_close_group_mutual_recognition() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("mutual recognition quorum test");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    let quotes = client
        .get_store_quotes(&address, data_size, CHUNK_DATA_TYPE)
        .await
        .expect("get_store_quotes should succeed");

    // Build peer_id -> close_group_view map
    let peer_ids: Vec<[u8; 32]> = quotes
        .iter()
        .map(|(pid, _, _, _)| *pid.as_bytes())
        .collect();

    let views: Vec<HashSet<[u8; 32]>> = quotes
        .iter()
        .map(|(_, _, quote, _)| quote.close_group.iter().copied().collect())
        .collect();

    // Count how many peers mutually recognize each other.
    // For each pair (i, j), both i's view must contain j AND j's view must contain i.
    let mut mutual_pairs = 0usize;
    for i in 0..peer_ids.len() {
        for j in (i + 1)..peer_ids.len() {
            if views[i].contains(&peer_ids[j]) && views[j].contains(&peer_ids[i]) {
                mutual_pairs += 1;
            }
        }
    }

    // A quorum of CLOSE_GROUP_MAJORITY mutually-recognizing peers requires
    // at least C(CLOSE_GROUP_MAJORITY, 2) mutual pairs.
    let min_pairs = CLOSE_GROUP_MAJORITY * (CLOSE_GROUP_MAJORITY - 1) / 2;
    assert!(
        mutual_pairs >= min_pairs,
        "Expected at least {min_pairs} mutual recognition pairs, got {mutual_pairs}"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 3: Quorum members are prioritized (appear first in quotes) ───────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_quorum_members_prioritized_in_quotes() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("quorum prioritization test");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    let quotes = client
        .get_store_quotes(&address, data_size, CHUNK_DATA_TYPE)
        .await
        .expect("get_store_quotes should succeed");

    // The first CLOSE_GROUP_MAJORITY peers should form a mutual clique
    // (get_store_quotes reorders so quorum members come first).
    let first_majority: Vec<[u8; 32]> = quotes[..CLOSE_GROUP_MAJORITY]
        .iter()
        .map(|(pid, _, _, _)| *pid.as_bytes())
        .collect();

    let first_views: Vec<HashSet<[u8; 32]>> = quotes[..CLOSE_GROUP_MAJORITY]
        .iter()
        .map(|(_, _, quote, _)| quote.close_group.iter().copied().collect())
        .collect();

    // Verify mutual recognition within the first CLOSE_GROUP_MAJORITY peers
    for (i, peer_i) in first_majority.iter().enumerate() {
        for (j, peer_j) in first_majority.iter().enumerate() {
            if i == j {
                continue;
            }
            assert!(
                first_views[i].contains(peer_j),
                "Quorum member {} should recognize quorum member {} in its close group view",
                hex::encode(peer_i),
                hex::encode(peer_j),
            );
        }
    }

    drop(client);
    testnet.teardown().await;
}

// ─── Test 4: Full payment flow with quorum validation ──────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_paid_upload_with_close_group_quorum() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("full quorum payment flow test");
    let address = client
        .chunk_put(content.clone())
        .await
        .expect("chunk_put should succeed with quorum validation");

    let expected_address = compute_address(&content);
    assert_eq!(address, expected_address);

    // Verify the chunk is retrievable
    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed");
    let chunk = retrieved.expect("Chunk should be found after quorum-validated upload");
    assert_eq!(chunk.content.as_ref(), content.as_ref());

    drop(client);
    testnet.teardown().await;
}

// ─── Test 5: Quote close_group is bound by signature ───────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_quote_close_group_is_signature_bound() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("signature binding test payload");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    let quotes = client
        .get_store_quotes(&address, data_size, CHUNK_DATA_TYPE)
        .await
        .expect("get_store_quotes should succeed");

    // Pick the first quote and verify its signature covers the close_group
    let (_peer_id, _addrs, quote, _price) = &quotes[0];

    // Reconstruct signing bytes and verify they include close_group
    let signing_bytes = PaymentQuote::bytes_for_signing(
        quote.content,
        quote.timestamp,
        &quote.price,
        &quote.rewards_address,
        &quote.close_group,
    );

    // The self-computed signing bytes should match
    let self_bytes = quote.bytes_for_sig();
    assert_eq!(
        signing_bytes, self_bytes,
        "bytes_for_signing with close_group should match bytes_for_sig"
    );

    // Verify that changing the close_group produces different signing bytes
    let tampered_bytes = PaymentQuote::bytes_for_signing(
        quote.content,
        quote.timestamp,
        &quote.price,
        &quote.rewards_address,
        &[[0xFFu8; 32]], // fake close group
    );
    assert_ne!(
        signing_bytes, tampered_bytes,
        "Tampered close_group should produce different signing bytes"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 6: Multiple uploads reuse quorum validation ──────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_multiple_uploads_with_quorum() {
    let (client, testnet) = setup().await;

    let payloads = [
        Bytes::from("quorum multi-upload chunk A"),
        Bytes::from("quorum multi-upload chunk B"),
        Bytes::from("quorum multi-upload chunk C"),
    ];

    let mut addresses = Vec::new();
    for payload in &payloads {
        let addr = client
            .chunk_put(payload.clone())
            .await
            .expect("chunk_put should succeed");
        addresses.push(addr);
    }

    // Verify all chunks are retrievable
    for (addr, payload) in addresses.iter().zip(payloads.iter()) {
        let chunk = client
            .chunk_get(addr)
            .await
            .expect("chunk_get should succeed")
            .expect("chunk should exist");
        assert_eq!(chunk.content.as_ref(), payload.as_ref());
    }

    drop(client);
    testnet.teardown().await;
}
