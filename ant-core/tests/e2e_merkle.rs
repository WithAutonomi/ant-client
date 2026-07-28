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

use ant_core::data::client::merkle::{merkle_billable_leaves, PaymentMode};
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

/// Merkle payment either side of the 256-leaf batch boundary, against a real
/// on-chain settlement.
///
/// 257 is the count the old `addresses.chunks(MAX_LEAVES)` split partitioned
/// as `[256, 1]`: the 256-address batch was paid for on-chain and the
/// singleton remainder could not build a tree, so the call returned a *paid*
/// partial result carrying 256 proofs. It now splits as `[255, 2]` and every
/// address comes back with a proof.
///
/// Paying directly is what makes the boundary reachable: getting 257 chunks
/// out of self-encryption needs a ~1 GB file, while `pay_for_merkle_batch`
/// takes the address set straight.
///
/// Settlement is checked against the same padded-leaf model the estimator
/// bills with — 65 addresses settle 128 leaves, 257 settle 256 + 2 — so the
/// two counts must cost in that ratio.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_merkle_payment_across_batch_boundary() {
    let (client, testnet) = setup_merkle_testnet().await;

    // Distinct, deterministic addresses. Payment binds to addresses only; the
    // chunks themselves are never stored by this test.
    let addresses = |count: usize, tag: u8| -> Vec<[u8; 32]> {
        (0..count)
            .map(|i| {
                let mut addr = [0u8; 32];
                addr[0] = tag;
                addr[1..9].copy_from_slice(&(i as u64).to_be_bytes());
                addr
            })
            .collect()
    };

    let mut paid: Vec<(usize, u128)> = Vec::new();

    for (count, tag) in [(65usize, 0xA1u8), (257usize, 0xB2u8)] {
        let addrs = addresses(count, tag);

        // A 65/257-address tree collects far more candidate pools than the
        // small-tree tests above, and an in-process 35-node testnet on a loaded
        // CI runner can leave one pool a candidate short ("Got 15 merkle
        // candidates, need 16"). That shortfall is transient, so retry it.
        // The bug this test guards is not: a `[256, 1]` partition fails to
        // build its second tree on every attempt, so a short proof set
        // survives all three and still fails the assertion below.
        let mut result = None;
        let mut last_shortfall = String::new();
        for attempt in 1..=3 {
            eprintln!("Paying for {count} addresses via merkle batch (attempt {attempt}/3)...");
            match client
                .pay_for_merkle_batch(&addrs, 0, TEST_CHUNK_SIZE as u64)
                .await
            {
                Ok(full) if full.proofs.len() == count => {
                    result = Some(full);
                    break;
                }
                Ok(partial) => {
                    last_shortfall = format!(
                        "paid but returned {} of {count} proofs",
                        partial.proofs.len()
                    );
                }
                Err(e) => last_shortfall = e.to_string(),
            }
            eprintln!("  attempt {attempt}/3 fell short: {last_shortfall}");
        }
        let result = result.unwrap_or_else(|| {
            panic!(
                "merkle payment for {count} addresses never returned a full proof set \
                 after 3 attempts: {last_shortfall}"
            )
        });

        assert_eq!(
            result.proofs.len(),
            count,
            "every one of the {count} addresses must come back with a proof, not just the \
             sub-batches that fit a tree"
        );
        assert_eq!(result.chunk_count, count);
        for addr in &addrs {
            assert!(
                result.proofs.contains_key(addr),
                "missing proof for {}",
                hex::encode(addr)
            );
        }

        let settled: u128 = result
            .storage_cost_atto
            .parse()
            .expect("settled amount should parse");
        assert!(
            settled > 0,
            "{count} addresses must settle a non-zero amount"
        );
        eprintln!(
            "  {count} addresses: {} leaves billed, settled {settled} atto",
            merkle_billable_leaves(count as u64)
        );
        paid.push((count, settled));
    }

    // Prices are uniform across a freshly started local testnet and this test
    // stores nothing, so the only thing separating the two settlements is the
    // padded leaf count each partition pays for.
    let [(small, small_atto), (large, large_atto)] = paid[..] else {
        panic!("expected two payments");
    };
    let expected =
        merkle_billable_leaves(large as u64) as f64 / merkle_billable_leaves(small as u64) as f64;
    let observed = large_atto as f64 / small_atto as f64;
    assert!(
        (observed - expected).abs() / expected < 0.15,
        "settlement should scale with padded leaves: expected ~{expected:.3}x \
         ({small} -> {large} addresses), observed {observed:.3}x"
    );

    drop(client);
    testnet.teardown().await;
}

// Single-node coexistence is tested in e2e_file.rs (DEFAULT_NODE_COUNT testnet).
// The 35-node testnet's DHT can have sparse XOR regions where single-node
// quotes can't find 5 peers for a random chunk address, making that test
// unreliable here. Merkle tests are the focus of this file.
