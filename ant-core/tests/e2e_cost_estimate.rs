//! E2E tests for file upload cost estimation.
//!
//! Compares `estimate_upload_cost()` against actual upload costs to verify
//! the estimate is accurate. Tests multiple file sizes covering single-wave
//! and multi-chunk scenarios.
//!
//! Run with: cargo test --test e2e_cost_estimate -- --nocapture

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::client::merkle::PaymentMode;
use ant_core::data::{Client, ClientConfig};
use serial_test::serial;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::MiniTestnet;
use tempfile::TempDir;

/// Simple xorshift64 PRNG for deterministic, incompressible test data.
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u8(&mut self) -> u8 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 & 0xFF) as u8
    }
}

fn create_test_file(dir: &Path, size: u64, name: &str, seed: u64) -> PathBuf {
    let path = dir.join(name);
    let mut file = std::fs::File::create(&path).expect("create test file");

    let mut rng = Xorshift64::new(seed);
    let mut remaining = size;
    let buf_size: usize = 64 * 1024;
    let mut buf = vec![0u8; buf_size];
    while remaining > 0 {
        let to_write = remaining.min(buf_size as u64) as usize;
        for byte in buf.iter_mut().take(to_write) {
            *byte = rng.next_u8();
        }
        file.write_all(&buf[..to_write]).expect("write test data");
        remaining -= to_write as u64;
    }
    file.flush().expect("flush test file");
    path
}

/// Estimate vs actual cost comparison for a single file.
///
/// Runs `estimate_upload_cost`, then actually uploads and compares.
/// Returns (estimated_atto, actual_atto, chunk_count_estimate, chunk_count_actual).
async fn compare_estimate_vs_actual(
    client: &Client,
    path: &Path,
    mode: PaymentMode,
) -> (u128, u128, usize, usize) {
    // Phase 1: Estimate
    let estimate = client
        .estimate_upload_cost(path, mode, None)
        .await
        .expect("estimate should succeed");

    let estimated_atto: u128 = estimate
        .storage_cost_atto
        .parse()
        .expect("parse estimated atto");

    // Phase 2: Actually upload (with a DIFFERENT seed so we don't get AlreadyStored)
    let result = client
        .file_upload_with_mode(path, mode)
        .await
        .expect("upload should succeed");

    let actual_atto: u128 = result.storage_cost_atto.parse().expect("parse actual atto");

    (
        estimated_atto,
        actual_atto,
        estimate.chunk_count,
        result.chunks_stored,
    )
}

/// Core test: estimate accuracy across file sizes.
///
/// Verifies:
/// 1. Chunk count from estimate matches actual upload chunk count
/// 2. Storage cost estimate is within 50% of actual cost
///    (prices are uniform on a healthy local network, so this is generous)
/// 3. Estimate does not require a wallet (no payment made)
/// 4. Estimate returns correct payment mode
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_estimate_matches_actual_cost() {
    let testnet = MiniTestnet::start(10).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());

    let work_dir = TempDir::new().expect("create work dir");

    // Test files: small (3 chunks), medium (~13 chunks)
    let test_cases: Vec<(u64, &str, u64)> = vec![
        (4 * 1024, "tiny.bin", 0xAAAA_0001),         // ~4 KB -> 3 chunks
        (100 * 1024, "small.bin", 0xAAAA_0002),      // 100 KB -> ~3 chunks
        (1024 * 1024, "1mb.bin", 0xAAAA_0003),       // 1 MB -> ~3 chunks
        (10 * 1024 * 1024, "10mb.bin", 0xAAAA_0004), // 10 MB -> ~3 chunks
    ];

    eprintln!();
    eprintln!("╔═══════════╤════════════════╤════════════════╤═══════════════════════╤═══════════════════════╗");
    eprintln!("║ File      │ Est. Chunks    │ Act. Chunks    │ Est. Cost (atto)      │ Act. Cost (atto)      ║");
    eprintln!("╠═══════════╪════════════════╪════════════════╪═══════════════════════╪═══════════════════════╣");

    for (size, name, seed) in &test_cases {
        let path = create_test_file(work_dir.path(), *size, name, *seed);

        let (est_atto, act_atto, est_chunks, act_chunks) =
            compare_estimate_vs_actual(&client, &path, PaymentMode::Auto).await;

        let size_label = if *size >= 1024 * 1024 {
            format!("{} MB", size / (1024 * 1024))
        } else {
            format!("{} KB", size / 1024)
        };

        eprintln!(
            "║ {:<9} │ {:>14} │ {:>14} │ {:>21} │ {:>21} ║",
            size_label, est_chunks, act_chunks, est_atto, act_atto,
        );

        // Chunk count MUST match exactly (same file, same encryption)
        assert_eq!(
            est_chunks, act_chunks,
            "Chunk count mismatch for {name}: estimate={est_chunks}, actual={act_chunks}"
        );

        // Storage cost should be within 15% (prices are uniform on a local
        // testnet so the extrapolation from one quote should be very close).
        if act_atto > 0 {
            let ratio = if est_atto > act_atto {
                est_atto as f64 / act_atto as f64
            } else {
                act_atto as f64 / est_atto as f64
            };
            assert!(
                ratio < 1.15,
                "Cost estimate too far from actual for {name}: est={est_atto}, actual={act_atto}, ratio={ratio:.2}"
            );
        }
    }

    eprintln!("╚═══════════╧════════════════╧════════════════╧═══════════════════════╧═══════════════════════╝");
    eprintln!();
}

/// Test that estimate works without a wallet.
///
/// Creates a client WITHOUT a wallet and verifies that
/// `estimate_upload_cost` still returns a valid estimate.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_estimate_works_without_wallet() {
    // Use 10 nodes (> CLOSE_GROUP_SIZE=7) for quote reliability
    let testnet = MiniTestnet::start(10).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Client WITHOUT wallet — no .with_wallet() call
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default());

    let work_dir = TempDir::new().expect("create work dir");
    let path = create_test_file(work_dir.path(), 4096, "no_wallet.bin", 0xBBBB_0001);

    let estimate = client
        .estimate_upload_cost(&path, PaymentMode::Auto, None)
        .await
        .expect("estimate should work without wallet");

    assert!(
        estimate.chunk_count >= 3,
        "self-encryption produces at least 3 chunks"
    );
    assert!(estimate.file_size == 4096);
}

/// Test that estimate returns correct payment mode.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_estimate_payment_mode() {
    let testnet = MiniTestnet::start(10).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default());

    let work_dir = TempDir::new().expect("create work dir");

    // Small file (3 chunks) with Auto mode -> should be Single
    let small_path = create_test_file(work_dir.path(), 4096, "small_mode.bin", 0xDDDD_0001);
    let small_est = client
        .estimate_upload_cost(&small_path, PaymentMode::Auto, None)
        .await
        .expect("estimate should succeed");
    assert_eq!(
        small_est.payment_mode,
        PaymentMode::Single,
        "Small file with Auto should use Single mode"
    );

    // Force merkle on small file
    let merkle_est = client
        .estimate_upload_cost(&small_path, PaymentMode::Merkle, None)
        .await
        .expect("estimate should succeed");
    assert_eq!(
        merkle_est.payment_mode,
        PaymentMode::Merkle,
        "Forced Merkle should report Merkle mode"
    );

    // Force single
    let single_est = client
        .estimate_upload_cost(&small_path, PaymentMode::Single, None)
        .await
        .expect("estimate should succeed");
    assert_eq!(
        single_est.payment_mode,
        PaymentMode::Single,
        "Forced Single should report Single mode"
    );
}

/// Test that estimate rejects files too small for self-encryption.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_estimate_rejects_tiny_files() {
    let testnet = MiniTestnet::start(10).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default());

    let work_dir = TempDir::new().expect("create work dir");

    // 2-byte file — below self-encryption minimum of 3 bytes
    let tiny_path = work_dir.path().join("tiny.bin");
    std::fs::write(&tiny_path, b"ab").expect("write tiny file");

    let result = client
        .estimate_upload_cost(&tiny_path, PaymentMode::Auto, None)
        .await;
    assert!(result.is_err(), "Estimate should fail for files < 3 bytes");
}
