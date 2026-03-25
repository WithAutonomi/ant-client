//! E2E tests for in-memory data upload/download using self-encryption with real EVM payments.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, ClientConfig};
use bytes::Bytes;
use self_encryption::encrypt;
use serial_test::serial;
use std::sync::Arc;
use support::MiniTestnet;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(8).await;
    let node = testnet.node(4).expect("Node 4 should exist");

    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    (client, testnet)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_data_upload_download_round_trip() {
    let (client, testnet) = setup().await;

    // Self-encryption requires at least 3072 bytes
    let content = Bytes::from(vec![0x42u8; 4096]);

    let result = client
        .data_upload(content.clone())
        .await
        .expect("data_upload should succeed");

    assert!(
        result.chunks_stored >= 3,
        "self-encryption produces at least 3 chunks"
    );

    let downloaded = client
        .data_download(&result.data_map)
        .await
        .expect("data_download should succeed");

    assert_eq!(
        downloaded, content,
        "downloaded content should match original"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_data_large_content() {
    let (client, testnet) = setup().await;

    // 100KB of patterned data
    let content: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();
    let content = Bytes::from(content);

    let result = client
        .data_upload(content.clone())
        .await
        .expect("data_upload should succeed");

    assert!(result.chunks_stored >= 3, "should produce multiple chunks");

    let downloaded = client
        .data_download(&result.data_map)
        .await
        .expect("data_download should succeed");

    assert_eq!(
        downloaded.len(),
        content.len(),
        "downloaded size should match"
    );
    assert_eq!(downloaded, content, "content should match exactly");

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_data_upload_too_small_fails() {
    let (client, testnet) = setup().await;

    // MIN_ENCRYPTABLE_BYTES = 3 (3 * MIN_CHUNK_SIZE where MIN_CHUNK_SIZE=1)
    // So only 0, 1, or 2 bytes should fail
    let content = Bytes::from(vec![0x42u8; 2]);

    let result = client.data_upload(content).await;
    assert!(
        result.is_err(),
        "files smaller than MIN_ENCRYPTABLE_BYTES (3) should fail"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_data_deterministic_encryption() {
    let (client, testnet) = setup().await;

    let content = Bytes::from(vec![0xAA; 4096]);

    let result1 = client
        .data_upload(content.clone())
        .await
        .expect("first upload should succeed");

    let result2 = client
        .data_upload(content.clone())
        .await
        .expect("second upload should succeed");

    // Verify download produces the original content (retry for CI transport flakiness)
    let mut downloaded = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match client.data_download(&result2.data_map).await {
            Ok(d) => {
                downloaded = Some(d);
                break;
            }
            Err(e) if attempt < 2 => {
                eprintln!("attempt {attempt}: data_download failed: {e}");
            }
            Err(e) => panic!("data_download should succeed: {e}"),
        }
    }
    let downloaded = downloaded.expect("data_download should succeed after retries");
    assert_eq!(
        downloaded, content,
        "downloaded content should match original after deterministic re-upload"
    );

    // Convergent encryption: same content produces same DataMap
    assert_eq!(
        result1.data_map.infos().len(),
        result2.data_map.infos().len(),
        "same content should produce same number of chunks"
    );

    // Verify chunk addresses match (convergent encryption property)
    for (a, b) in result1
        .data_map
        .infos()
        .iter()
        .zip(result2.data_map.infos().iter())
    {
        assert_eq!(
            a.dst_hash, b.dst_hash,
            "same content should produce same chunk addresses"
        );
    }

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_data_upload_partial_overlap_skips_payment_for_existing_chunks() {
    let (client, testnet) = setup().await;

    // Use enough data to produce multiple encrypted chunks (self-encryption needs >= 3)
    let content = Bytes::from(vec![0xBB; 8192]);

    // Encrypt locally to discover what chunks will be produced
    let (_data_map, encrypted_chunks) = encrypt(content.clone()).expect("encrypt should succeed");
    assert!(
        encrypted_chunks.len() >= 3,
        "need at least 3 chunks, got {}",
        encrypted_chunks.len()
    );

    // Pre-store the FIRST encrypted chunk on the network
    let first_chunk_content = encrypted_chunks
        .first()
        .expect("should have first chunk")
        .content
        .clone();
    client
        .chunk_put(first_chunk_content)
        .await
        .expect("pre-storing one chunk should succeed");

    // Record balance AFTER pre-storing one chunk (this is our baseline)
    let balance_before = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    // Upload the full data — chunk_put will be called for each encrypted chunk,
    // but the pre-stored one should be detected as AlreadyStored and skipped
    let result = client
        .data_upload(content.clone())
        .await
        .expect("data_upload should succeed");

    assert!(
        result.chunks_stored >= 3,
        "should store at least 3 chunks total"
    );

    let balance_after_partial = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    // Balance should have decreased (we paid for N-1 new chunks)
    assert!(
        balance_after_partial < balance_before,
        "should have paid for the new chunks"
    );

    // Now upload the SAME data again — ALL chunks exist, so zero payment
    let balance_before_dup = balance_after_partial;
    let result2 = client
        .data_upload(content.clone())
        .await
        .expect("duplicate data_upload should succeed");

    assert_eq!(
        result.chunks_stored, result2.chunks_stored,
        "same content should produce same chunk count"
    );

    let balance_after_dup = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    assert_eq!(
        balance_before_dup, balance_after_dup,
        "full duplicate upload should not spend any tokens"
    );

    // Verify the data is still downloadable and correct (retry for CI transport flakiness)
    let mut downloaded = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match client.data_download(&result.data_map).await {
            Ok(d) => {
                downloaded = Some(d);
                break;
            }
            Err(e) if attempt < 2 => {
                eprintln!("attempt {attempt}: data_download failed: {e}");
            }
            Err(e) => panic!("data_download should succeed: {e}"),
        }
    }
    let downloaded = downloaded.expect("data_download should succeed after retries");
    assert_eq!(downloaded, content, "downloaded content should match");

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_public_data_map_store_and_fetch() {
    let (client, testnet) = setup().await;

    // Upload data, get a DataMap
    let content = Bytes::from(vec![0xFFu8; 4096]);
    let result = client
        .data_upload(content.clone())
        .await
        .expect("data_upload should succeed");

    // Store the DataMap publicly (as a chunk on the network)
    let dm_address = client
        .data_map_store(&result.data_map)
        .await
        .expect("data_map_store should succeed");

    // Fetch it back by address
    let fetched_dm = client
        .data_map_fetch(&dm_address)
        .await
        .expect("data_map_fetch should succeed");

    assert_eq!(
        result.data_map.infos().len(),
        fetched_dm.infos().len(),
        "Fetched DataMap should have same number of chunks"
    );

    // Use the fetched DataMap to download the original data (retry for CI transport flakiness)
    let mut downloaded = None;
    for attempt in 0..3u32 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        match client.data_download(&fetched_dm).await {
            Ok(d) => {
                downloaded = Some(d);
                break;
            }
            Err(e) if attempt < 2 => {
                eprintln!("attempt {attempt}: data_download failed: {e}");
            }
            Err(e) => panic!("data_download from fetched DataMap should succeed: {e}"),
        }
    }
    let downloaded = downloaded.expect("data_download should succeed after retries");

    assert_eq!(
        downloaded, content,
        "downloaded content should match original"
    );

    drop(client);
    testnet.teardown().await;
}
