//! E2E tests for file upload/download using streaming self-encryption.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, ClientConfig};
use serial_test::serial;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use support::MiniTestnet;
use tempfile::{NamedTempFile, TempDir};

const CLIENT_TIMEOUT_SECS: u64 = 30;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let config = ClientConfig {
        timeout_secs: CLIENT_TIMEOUT_SECS,
        ..Default::default()
    };
    let client = Client::from_node(Arc::clone(&node), config).with_wallet(testnet.wallet().clone());

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
