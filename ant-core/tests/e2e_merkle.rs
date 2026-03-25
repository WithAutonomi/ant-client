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
use ant_core::data::{Client, ClientConfig};
use serial_test::serial;
use std::io::Write;
use std::sync::Arc;
use support::MiniTestnet;
use tempfile::{NamedTempFile, TempDir};

const CLIENT_TIMEOUT_SECS: u64 = 120;

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
        timeout_secs: CLIENT_TIMEOUT_SECS,
        close_group_size: 20,
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

// Single-node coexistence is tested in e2e_file.rs (6-node testnet).
// The 35-node testnet's DHT can have sparse XOR regions where single-node
// quotes can't find 5 peers for a random chunk address, making that test
// unreliable here. Merkle tests are the focus of this file.
