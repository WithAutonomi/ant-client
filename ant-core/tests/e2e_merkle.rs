//! E2E tests for merkle batch payment uploads.
//!
//! Spins up a 35-node testnet with Anvil EVM, tests merkle payment flow
//! including upload/download verification, payment mode assertion,
//! and in-memory data upload path.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::cast_possible_truncation
)]

mod support;

use ant_core::data::client::merkle::PaymentMode;
use ant_core::data::{compute_address, Client, ClientConfig};
use serial_test::serial;
use std::io::Write;
use std::sync::Arc;
use support::MiniTestnet;
use tempfile::{NamedTempFile, TempDir};

const CLIENT_QUOTE_TIMEOUT_SECS: u64 = 120;
const CLIENT_STORE_TIMEOUT_SECS: u64 = 120;

/// Chunk size for merkle security tests (small, fast to hash).
const TEST_CHUNK_SIZE: usize = 1024;

/// Create a 35-node testnet suitable for merkle payments.
async fn setup_merkle_testnet() -> (Client, MiniTestnet) {
    eprintln!("Starting 35-node testnet...");
    let testnet = MiniTestnet::start(35).await;
    eprintln!(
        "Testnet started, {} nodes running",
        testnet.running_node_count()
    );

    let node = testnet.node(5).expect("Node 5 should exist");
    let routing_size = node.dht().get_routing_table_size().await;
    let connected = node.connected_peers().await.len();
    eprintln!("Client node routing table: {routing_size} entries, {connected} connected");

    let config = ClientConfig {
        quote_timeout_secs: CLIENT_QUOTE_TIMEOUT_SECS,
        store_timeout_secs: CLIENT_STORE_TIMEOUT_SECS,
        close_group_size: 20,
        ..Default::default()
    };
    let client = Client::from_node(Arc::clone(&node), config).with_wallet(testnet.wallet().clone());

    (client, testnet)
}

/// Merkle file upload/download round-trip with payment mode assertion.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_merkle_file_upload_download() {
    let (client, testnet) = setup_merkle_testnet().await;

    // 500KB file — self-encryption produces 3+ chunks
    let data: Vec<u8> = (0u8..=255).cycle().take(500_000).collect();
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    eprintln!("Uploading 500KB file with forced merkle payment mode...");

    let result = client
        .file_upload_with_mode(input_file.path(), PaymentMode::Merkle)
        .await
        .expect("merkle file upload should succeed");

    // Assert merkle payment was actually used (not a silent fallback)
    assert_eq!(
        result.payment_mode_used,
        PaymentMode::Merkle,
        "payment_mode_used must be Merkle, not a silent fallback to Single"
    );

    eprintln!(
        "Upload complete: {} chunks stored via {:?}",
        result.chunks_stored, result.payment_mode_used
    );

    assert!(
        result.chunks_stored >= 3,
        "self-encryption should produce at least 3 chunks, got {}",
        result.chunks_stored
    );

    // Download and verify content integrity
    let output_dir = TempDir::new().expect("create temp dir");
    let output_path = output_dir.path().join("merkle_downloaded.bin");

    eprintln!("Downloading file...");

    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("file download should succeed");

    let downloaded = std::fs::read(&output_path).expect("read downloaded file");
    assert_eq!(downloaded.len(), data.len(), "downloaded size must match");
    assert_eq!(downloaded, data, "downloaded content must match original");
    assert_eq!(bytes_written, data.len() as u64, "bytes_written must match");

    eprintln!("Merkle file upload/download round-trip verified.");

    drop(client);
    testnet.teardown().await;
}

/// Merkle in-memory data upload/download round-trip.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_merkle_data_upload_download() {
    let (client, testnet) = setup_merkle_testnet().await;

    let data: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();

    eprintln!("Uploading 100KB in-memory data with merkle mode...");

    let result = client
        .data_upload_with_mode(bytes::Bytes::from(data.clone()), PaymentMode::Merkle)
        .await
        .expect("merkle data upload should succeed");

    assert_eq!(
        result.payment_mode_used,
        PaymentMode::Merkle,
        "data upload must use Merkle mode"
    );

    assert!(result.chunks_stored >= 3, "should produce multiple chunks");

    // Download and verify
    let downloaded = client
        .data_download(&result.data_map)
        .await
        .expect("data download should succeed");

    assert_eq!(downloaded.len(), data.len());
    assert_eq!(downloaded.as_ref(), data.as_slice());

    eprintln!("Merkle data upload/download round-trip verified.");

    drop(client);
    testnet.teardown().await;
}

// ─── Merkle Payment Security Tests ─────────────────────────────────────────
//
// Verify that nodes reject tampered merkle proofs. Unlike single-node payments
// where the client controls the amount, merkle payment amounts are determined
// by the smart contract. The cheating vectors for merkle are:
// - Using a proof from one chunk to store a different chunk (address mismatch)
// - Using a proof from a payment that didn't happen on-chain

/// Use a valid merkle proof from chunk A to try storing chunk B.
///
/// The merkle proof contains an address-binding commitment: the proof's
/// `address` field and sibling hashes bind it to a specific leaf in the tree.
/// Nodes must verify this binding rejects mismatched chunks.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_merkle_proof_for_wrong_chunk() {
    let (client, testnet) = setup_merkle_testnet().await;

    // Create 4 small chunks for a minimal merkle tree
    let chunks: Vec<bytes::Bytes> = (0..4u8)
        .map(|i| bytes::Bytes::from(vec![i; TEST_CHUNK_SIZE]))
        .collect();
    let addresses: Vec<[u8; 32]> = chunks.iter().map(|c| compute_address(c)).collect();

    eprintln!("Paying for 4 chunks via merkle batch...");

    // Pay for these chunks via merkle batch payment
    let batch_result = client
        .pay_for_merkle_batch(&addresses, 0, TEST_CHUNK_SIZE as u64)
        .await
        .expect("merkle batch payment should succeed");

    assert_eq!(
        batch_result.proofs.len(),
        4,
        "should have proofs for all 4 chunks"
    );

    // Get the proof for chunk 0
    let proof_for_chunk_0 = batch_result
        .proofs
        .get(&addresses[0])
        .expect("should have proof for chunk 0")
        .clone();

    // Create a completely different chunk NOT in the merkle tree
    let evil_content = bytes::Bytes::from("this content was NOT in the merkle tree");
    let evil_address = compute_address(&evil_content);
    assert_ne!(
        evil_address, addresses[0],
        "evil chunk must have a different address"
    );

    // Find a peer close to the evil chunk's address to PUT to
    let peers = client
        .network()
        .find_closest_peers(&evil_address, 1)
        .await
        .expect("should find peers");
    let (target_peer, target_addrs) = &peers[0];

    eprintln!("Attempting PUT of wrong chunk with merkle proof for chunk 0...");

    // Try to store the evil chunk using chunk 0's merkle proof
    let result = client
        .chunk_put_with_proof(evil_content, proof_for_chunk_0, target_peer, target_addrs)
        .await;

    assert!(
        result.is_err(),
        "PUT with merkle proof for a different chunk should be rejected (address mismatch)"
    );

    drop(client);
    testnet.teardown().await;
}

/// Use a proof from chunk A to try storing chunk B where both are in the tree.
///
/// Even when both chunks have valid merkle proofs from the same batch, the
/// proofs are NOT interchangeable — each proof binds to its specific leaf
/// via the address and sibling hash path.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_merkle_proof_swap_within_batch() {
    let (client, testnet) = setup_merkle_testnet().await;

    let chunks: Vec<bytes::Bytes> = (0..4u8)
        .map(|i| bytes::Bytes::from(vec![i; TEST_CHUNK_SIZE]))
        .collect();
    let addresses: Vec<[u8; 32]> = chunks.iter().map(|c| compute_address(c)).collect();

    eprintln!("Paying for 4 chunks via merkle batch...");

    let batch_result = client
        .pay_for_merkle_batch(&addresses, 0, TEST_CHUNK_SIZE as u64)
        .await
        .expect("merkle batch payment should succeed");

    // Take chunk 0's proof and try to store chunk 1 with it
    let proof_for_chunk_0 = batch_result
        .proofs
        .get(&addresses[0])
        .expect("should have proof for chunk 0")
        .clone();

    let peers = client
        .network()
        .find_closest_peers(&addresses[1], 1)
        .await
        .expect("should find peers");
    let (target_peer, target_addrs) = &peers[0];

    eprintln!("Attempting to store chunk 1 using chunk 0's merkle proof...");

    let result = client
        .chunk_put_with_proof(
            chunks[1].clone(),
            proof_for_chunk_0,
            target_peer,
            target_addrs,
        )
        .await;

    assert!(
        result.is_err(),
        "Swapping merkle proofs between chunks in the same batch should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// Single-node coexistence is tested in e2e_file.rs (DEFAULT_NODE_COUNT testnet).
// The 35-node testnet's DHT can have sparse XOR regions where single-node
// quotes can't find 5 peers for a random chunk address, making that test
// unreliable here. Merkle tests are the focus of this file.
