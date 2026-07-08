//! E2E tests for file upload/download using streaming self-encryption.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{
    compute_address, Client, ClientConfig, Error, ExternalPaymentInfo, PaymentMode, Visibility,
};
use ant_protocol::evm::{QuoteHash, TxHash};
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

/// Streaming download: `file_download_to_sender` must yield exactly the bytes,
/// in order, that the file contained — without buffering the whole file. Uses
/// a multi-batch payload so the streaming-decrypt path runs more than one
/// batch, then reassembles the stream and asserts equality with the source.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_file_download_to_sender_multibatch_round_trip() {
    use tokio::sync::mpsc;

    let (client, testnet) = setup().await;

    let mut input_file = NamedTempFile::new().expect("create temp file");
    // ~1 MiB of varied bytes → many self-encryption chunks (multiple batches).
    let data: Vec<u8> = (0..1_048_576u32).map(|i| (i % 251) as u8).collect();
    input_file.write_all(&data).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let result = client
        .file_upload(input_file.path())
        .await
        .expect("file_upload should succeed");

    // Channel item type is inferred from `file_download_to_sender`'s signature.
    let (tx, mut rx) = mpsc::channel(8);
    let data_map = result.data_map.clone();
    let dl = tokio::spawn(async move { client.file_download_to_sender(&data_map, tx, None).await });

    let mut streamed: Vec<u8> = Vec::with_capacity(data.len());
    let mut chunk_count = 0usize;
    while let Some(item) = rx.recv().await {
        let chunk = item.expect("stream chunk should be Ok");
        // A buggy "send one empty/sentinel then drop" producer would still
        // close the channel; assert each delivered chunk carries real bytes.
        assert!(!chunk.is_empty(), "streamed chunk should be non-empty");
        chunk_count += 1;
        streamed.extend_from_slice(&chunk);
    }

    let bytes_streamed = dl
        .await
        .expect("download task should join")
        .expect("file_download_to_sender should succeed");

    // The whole point of the streaming path: a multi-batch payload must arrive
    // as more than one segment, not buffered and emitted in one shot.
    assert!(
        chunk_count >= 2,
        "multi-batch payload should stream as ≥2 segments, got {chunk_count}"
    );
    assert_eq!(streamed, data, "streamed content should match original");
    assert_eq!(
        bytes_streamed,
        data.len() as u64,
        "bytes_streamed should match original size"
    );

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

/// #140 (wave-batch), Tier 1: a post-payment store failure on a real
/// (in-process) testnet must surface `FinalizeStorePaidFailed` carrying a
/// `PaidRetryState`, the wallet must NOT be charged again by the failed
/// finalize, and the retry state must be re-drivable via `finalize_resume`
/// without re-paying.
///
/// This proves the *real* store path (not a mocked `WaveResult`) produces the
/// retry material and that resume re-enters storage with no second payment. The
/// success-after-resume half needs the ability to restart a downed node, which
/// `MiniTestnet` does not support — it is tracked as a devnet follow-up
/// (WithAutonomi/ant-client#144).
///
/// `#[ignore]`: this drives storage against deliberately-killed peers, so its
/// wall-clock is dominated by transport dial timeouts (several minutes, and
/// markedly slower on virtualised macOS runners). The deterministic coverage of
/// the same logic lives in the `assemble_*_result` unit tests, which run in CI;
/// this test is a real-network proof run on demand:
/// `cargo test -p ant-core --test e2e_file -- --ignored <name>`.
#[tokio::test(flavor = "multi_thread")]
#[serial]
#[ignore = "slow real-network test (~6 min, dead-peer dial timeouts); run on demand"]
async fn test_finalize_after_payment_failure_yields_retryable_state_wave() {
    let mut testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    // Prepare/quote against the healthy network needs the usual generous quote
    // budget, but the store phase runs after we intentionally collapse the
    // network, so a short store timeout lets each doomed attempt give up
    // quickly instead of waiting the full 60 s per round.
    let config = ClientConfig {
        quote_timeout_secs: 60,
        store_timeout_secs: 3,
        ..Default::default()
    };
    let client = Client::from_node(Arc::clone(&node), config).with_wallet(testnet.wallet().clone());

    let original = vec![0x5au8; 4096];
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&original).expect("write temp file");
    input_file.flush().expect("flush temp file");

    // Phase 1: prepare (public → wave-batch for 4 KB) while the network is healthy.
    let prepared = client
        .file_prepare_upload_with_visibility(input_file.path(), Visibility::Public)
        .await
        .expect("prepare should succeed");
    let total_chunks = prepared.total_chunks;

    // Phase 2: pay the quotes (external-signer simulation).
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

    // Baseline token balance AFTER payment: neither finalize nor resume may
    // spend any more tokens (the whole point of #140 is "pay once").
    let balance_after_payment = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");

    // Phase 3: collapse the network below store quorum AFTER payment. Keep the
    // client's own node (index 3) plus two others; kill the rest so no chunk's
    // close group can reach quorum. `CLOSE_GROUP_SIZE` is 7, so 3 survivors
    // cannot satisfy any chunk.
    let keep = [0usize, 1, 3];
    for i in 0..DEFAULT_NODE_COUNT {
        if !keep.contains(&i) {
            testnet.shutdown_node(i);
        }
    }
    assert_eq!(
        testnet.running_node_count(),
        keep.len(),
        "only the kept nodes should remain"
    );

    // Phase 4: finalize stores against a network that cannot reach quorum. The
    // payment is already spent, so the failure MUST be a retryable
    // `FinalizeStorePaidFailed`, not a payment-stranding error.
    let err = client
        .finalize_upload(prepared, &tx_hash_map)
        .await
        .expect_err("finalize must fail with the network below quorum");

    let (unstored_after_finalize, retry) = match err {
        Error::FinalizeStorePaidFailed {
            retry,
            failed_count,
            total_chunks: reported_total,
            ..
        } => {
            assert_eq!(reported_total, total_chunks);
            assert!(failed_count > 0, "some chunks must be unstored");
            assert_eq!(retry.total_chunks(), total_chunks);
            assert!(
                retry.unstored_count() > 0,
                "retry state must carry the paid-but-unstored chunks"
            );
            (retry.unstored_count(), retry)
        }
        other => panic!("expected FinalizeStorePaidFailed, got {other:?}"),
    };

    // The failed finalize must not have spent any more tokens.
    let balance_after_finalize = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");
    assert_eq!(
        balance_after_payment, balance_after_finalize,
        "a failed finalize must not spend additional tokens"
    );

    // Phase 5: resume is re-drivable end-to-end. The network is still below
    // quorum, so this resume also fails — but it exercises the *real* resume
    // path (dispatch → store → assemble) and must not re-pay or lose the retry
    // material: it hands back an equivalent `PaidRetryState`.
    let err2 = client
        .finalize_resume(*retry)
        .await
        .expect_err("resume against a still-collapsed network fails again");
    match err2 {
        Error::FinalizeStorePaidFailed { retry, .. } => {
            assert_eq!(
                retry.unstored_count(),
                unstored_after_finalize,
                "no progress against a dead network, but no loss of retry material"
            );
        }
        other => panic!("expected FinalizeStorePaidFailed on resume, got {other:?}"),
    }

    // Resume must not have spent any tokens either.
    let balance_after_resume = client
        .wallet()
        .expect("wallet should be set")
        .balance_of_tokens()
        .await
        .expect("balance query should succeed");
    assert_eq!(
        balance_after_payment, balance_after_resume,
        "finalize_resume must not enter any payment path"
    );

    drop(client);
    testnet.teardown().await;
}

/// Full wallet-backed public upload round-trip (direct CLI-style path).
///
/// This covers the non-external-signer path used by `ant file upload --public`:
/// the serialized DataMap must be appended to the upload chunk set before
/// payment, so the returned address is immediately retrievable without a
/// second `data_map_store` payment.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_public_file_upload_direct_batches_datamap() {
    let (client, testnet) = setup().await;

    let original = vec![0x6bu8; 4096];
    let mut input_file = NamedTempFile::new().expect("create temp file");
    input_file.write_all(&original).expect("write temp file");
    input_file.flush().expect("flush temp file");

    let result = client
        .file_upload_public_with_mode(input_file.path(), PaymentMode::Single)
        .await
        .expect("public upload should succeed");

    let data_map_address = result
        .data_map_address
        .expect("public upload must return a DataMap address");
    let expected_bytes = rmp_serde::to_vec(&result.data_map).expect("serialize DataMap");
    assert_eq!(
        data_map_address,
        compute_address(&expected_bytes),
        "data_map_address must point to the serialized DataMap chunk"
    );
    assert_eq!(
        result.chunks_stored, result.total_chunks,
        "public upload should store every chunk, including the DataMap"
    );

    let fetched_data_map = client
        .data_map_fetch(&data_map_address)
        .await
        .expect("public DataMap should be retrievable by returned address");

    let output_dir = TempDir::new().expect("create output temp dir");
    let output_path = output_dir.path().join("direct_public_out.bin");
    let bytes_written = client
        .file_download(&fetched_data_map, &output_path)
        .await
        .expect("file_download should succeed");

    assert_eq!(bytes_written, original.len() as u64);
    let downloaded = std::fs::read(&output_path).expect("read downloaded file");
    assert_eq!(downloaded, original);

    drop(client);
    testnet.teardown().await;
}
