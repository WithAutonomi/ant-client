//! E2E tests for file upload/download using streaming self-encryption.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client, ExternalPaymentInfo, Visibility};
use evmlib::common::{QuoteHash, TxHash};
use serial_test::serial;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};
use tempfile::{NamedTempFile, TempDir};

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    (client, testnet)
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_upload_download_round_trip() {
    let (client, testnet) = setup().await;

    let mut input_file = NamedTempFile::new().expect("create temp file");
    let data = vec![0x42u8; 4096];
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let result = client
        .file_upload(input_file.path())
        .await
        .expect("file_upload should succeed");

    assert!(
        result.chunks_stored >= 3,
        "self-encryption produces at least 3 chunks"
    );

    let output_dir = TempDir::new().expect("create temp dir");
    let output_path = output_dir.path().join("downloaded.bin");

    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("file_download should succeed");

    let downloaded = std::fs::read(&output_path).expect("read output file");
    assert_eq!(downloaded, data, "downloaded content should match original");
    assert_eq!(
        bytes_written,
        data.len() as u64,
        "bytes_written should match original size"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_large_content() {
    let (client, testnet) = setup().await;

    let data: Vec<u8> = (0u8..=255).cycle().take(100_000).collect();
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let result = client
        .file_upload(input_file.path())
        .await
        .expect("file_upload should succeed");

    assert!(result.chunks_stored >= 3, "should produce multiple chunks");

    let output_dir = TempDir::new().expect("create temp dir");
    let output_path = output_dir.path().join("downloaded_large.bin");

    client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("file_download should succeed");

    let downloaded = std::fs::read(&output_path).expect("read output file");
    assert_eq!(downloaded.len(), data.len(), "downloaded size should match");
    assert_eq!(downloaded, data, "content should match exactly");

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_upload_nonexistent_path_fails() {
    let (client, testnet) = setup().await;

    let nonexistent = PathBuf::from("/tmp/ant_test_nonexistent_file_12345.bin");
    let result = client.file_upload(&nonexistent).await;
    assert!(
        result.is_err(),
        "file_upload on non-existent path should fail"
    );

    drop(client);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_download_bytes_written() {
    let (client, testnet) = setup().await;

    let data = vec![0xBB; 8192];
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let result = client
        .file_upload(input_file.path())
        .await
        .expect("file_upload should succeed");

    let output_dir = TempDir::new().expect("create temp dir");
    let output_path = output_dir.path().join("bytes_written_test.bin");

    let bytes_written = client
        .file_download(&result.data_map, &output_path)
        .await
        .expect("file_download should succeed");

    assert_eq!(
        bytes_written,
        data.len() as u64,
        "bytes_written should equal original file size"
    );

    drop(client);
    testnet.teardown().await;
}

/// External-signer prepare must bundle the serialized DataMap as one extra
/// paid chunk when `Visibility::Public` is requested, and must record the
/// resulting chunk address on the `PreparedUpload`. Private prepare must
/// leave that address unset.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_prepare_upload_visibility() {
    let (client, testnet) = setup().await;

    let data = vec![0x37u8; 4096];
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let private = client
        .file_prepare_upload_with_visibility(input_file.path(), Visibility::Private)
        .await
        .expect("private prepare should succeed");

    assert!(
        private.data_map_address.is_none(),
        "private uploads must not publish a DataMap address"
    );

    let public = client
        .file_prepare_upload_with_visibility(input_file.path(), Visibility::Public)
        .await
        .expect("public prepare should succeed");

    let public_addr = public
        .data_map_address
        .expect("public prepare must record the DataMap chunk address");

    // The recorded address must match a fresh hash of the serialized DataMap,
    // proving the address refers to exactly the chunk that was added to the
    // payment batch (and that `data_map_fetch` on this address will later
    // yield the same DataMap we're holding).
    let expected_bytes = rmp_serde::to_vec(&public.data_map).expect("serialize DataMap");
    let expected_addr = compute_address(&expected_bytes);
    assert_eq!(
        public_addr, expected_addr,
        "data_map_address must equal compute_address(rmp_serde::to_vec(&data_map))"
    );

    // A small file produces a wave-batch payment (well under the merkle
    // threshold), and the datamap chunk must appear in that batch.
    match (&private.payment_info, &public.payment_info) {
        (
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks: priv_chunks,
                ..
            },
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks: pub_chunks,
                ..
            },
        ) => {
            assert_eq!(
                pub_chunks.len(),
                priv_chunks.len() + 1,
                "public prepare must add exactly one chunk (the serialized DataMap) to the batch"
            );
            assert!(
                pub_chunks.iter().any(|c| c.address == public_addr),
                "the extra chunk must be the DataMap chunk at the recorded address"
            );
        }
        other => panic!("expected wave-batch for a 4KB file, got {other:?}"),
    }

    drop(client);
    testnet.teardown().await;
}

/// Full public-upload round-trip (wave-batch path).
///
/// Simulates the external-signer flow end-to-end: prepare → sign payments
/// via the testnet wallet → finalize → `data_map_fetch` using only the
/// returned address → `file_download` → assert recovered bytes equal the
/// original. Proves the data_map_address actually refers to a retrievable
/// DataMap on the network, not just a hash recorded in memory.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_public_upload_round_trip_wave_batch() {
    let (client, testnet) = setup().await;

    let original = vec![0x5au8; 4096];
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&original).expect("write temp file");
    input_file.flush().expect("flush temp file");

    // Phase 1: prepare as public.
    let prepared = client
        .file_prepare_upload_with_visibility(input_file.path(), Visibility::Public)
        .await
        .expect("public prepare should succeed");
    let data_map_address = prepared
        .data_map_address
        .expect("public prepare must record the DataMap address");

    // Phase 2: simulate an external signer by paying for the quotes with the
    // testnet wallet and collecting the resulting (quote_hash, tx_hash) map.
    let payments = match &prepared.payment_info {
        ExternalPaymentInfo::WaveBatch { payment_intent, .. } => payment_intent.payments.clone(),
        other => panic!("expected wave-batch payment for a 4KB file, got {other:?}"),
    };
    let (tx_hash_map, _gas) = testnet
        .wallet()
        .pay_for_quotes(payments)
        .await
        .expect("testnet wallet should pay for quotes");
    let tx_hash_map: HashMap<QuoteHash, TxHash> = tx_hash_map.into_iter().collect();

    // Phase 3: finalize. The data map chunk is stored alongside the data
    // chunks in this single call — no second network trip needed.
    let result = client
        .finalize_upload(prepared, &tx_hash_map)
        .await
        .expect("finalize_upload should succeed");
    assert_eq!(
        result.data_map_address,
        Some(data_map_address),
        "FileUploadResult must carry the DataMap address forward from PreparedUpload"
    );

    // Phase 4: a fresh retriever can fetch the data map using only the
    // shared address — they did not participate in the upload.
    let fetched_data_map = client
        .data_map_fetch(&data_map_address)
        .await
        .expect("data_map_fetch must retrieve the stored DataMap");

    // Phase 5: download + verify content.
    let output_dir = TempDir::new().expect("create output temp dir");
    let output_path = output_dir.path().join("round_trip_out.bin");
    let bytes_written = client
        .file_download(&fetched_data_map, &output_path)
        .await
        .expect("file_download should succeed");
    assert_eq!(
        bytes_written,
        original.len() as u64,
        "bytes_written should equal original size"
    );

    let downloaded = std::fs::read(&output_path).expect("read downloaded file");
    assert_eq!(
        downloaded, original,
        "downloaded bytes must equal the original file"
    );

    drop(client);
    testnet.teardown().await;
}
