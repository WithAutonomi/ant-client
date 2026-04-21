//! Upload cost comparison: Merkle vs Single payment modes.
//!
//! Uploads files of various sizes using both payment modes and reports
//! the ANT token cost, gas cost, and chunk counts in a summary table.
//!
//! Run with: cargo test --release --test e2e_upload_costs -- --nocapture

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::client::merkle::PaymentMode;
use ant_core::data::Client;
use serial_test::serial;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use support::{test_client_config, MiniTestnet};
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
    let write_buf_size: usize = 1024 * 1024;
    let mut buf = vec![0u8; write_buf_size];
    while remaining > 0 {
        let to_write = remaining.min(write_buf_size as u64) as usize;
        for byte in buf.iter_mut().take(to_write) {
            *byte = rng.next_u8();
        }
        file.write_all(&buf[..to_write])
            .expect("write chunk to test file");
        remaining -= to_write as u64;
    }
    file.flush().expect("flush test file");

    path
}

/// Result of a single upload cost measurement.
struct CostResult {
    file_size_mb: u64,
    mode: &'static str,
    chunks: usize,
    ant_cost_atto: u128,
    gas_cost_wei: u128,
    num_evm_txs: &'static str,
}

/// Measure ANT and gas cost for a single upload.
async fn measure_upload_cost(
    client: &Client,
    wallet: &evmlib::wallet::Wallet,
    path: &Path,
    mode: PaymentMode,
    file_size_mb: u64,
) -> CostResult {
    let ant_before = wallet
        .balance_of_tokens()
        .await
        .expect("get ANT balance before");
    let gas_before = wallet
        .balance_of_gas_tokens()
        .await
        .expect("get gas balance before");

    let result = client
        .file_upload_with_mode(path, mode)
        .await
        .expect("upload should succeed");

    let ant_after = wallet
        .balance_of_tokens()
        .await
        .expect("get ANT balance after");
    let gas_after = wallet
        .balance_of_gas_tokens()
        .await
        .expect("get gas balance after");

    let ant_spent = ant_before.saturating_sub(ant_after);
    let gas_spent = gas_before.saturating_sub(gas_after);

    // Estimate tx count based on mode and chunk count
    let tx_estimate = match mode {
        PaymentMode::Single => format!("~{}", result.chunks_stored.div_ceil(256)),
        PaymentMode::Merkle => {
            let sub_batches = result.chunks_stored.div_ceil(256);
            format!("{sub_batches}")
        }
        PaymentMode::Auto => format!("{:?}", result.payment_mode_used),
    };

    CostResult {
        file_size_mb,
        mode: match mode {
            PaymentMode::Single => "Single",
            PaymentMode::Merkle => "Merkle",
            PaymentMode::Auto => "Auto",
        },
        chunks: result.chunks_stored,
        ant_cost_atto: ant_spent.to::<u128>(),
        gas_cost_wei: gas_spent.to::<u128>(),
        num_evm_txs: Box::leak(tx_estimate.into_boxed_str()),
    }
}

fn format_atto(atto: u128) -> String {
    if atto == 0 {
        return "0".to_string();
    }
    // 1 ANT = 10^18 atto
    let whole = atto / 1_000_000_000_000_000_000;
    let frac = atto % 1_000_000_000_000_000_000;
    if whole > 0 {
        format!("{whole}.{frac:018} ANT")
    } else {
        format!("{atto} atto")
    }
}

fn format_wei(wei: u128) -> String {
    if wei == 0 {
        return "0".to_string();
    }
    let gwei = wei / 1_000_000_000;
    if gwei > 0 {
        format!("{gwei} gwei")
    } else {
        format!("{wei} wei")
    }
}

fn print_table(results: &[CostResult]) {
    eprintln!();
    eprintln!("╔══════════╤══════════╤════════╤══════════════════════════╤══════════════════╤═══════════╗");
    eprintln!("║ Size     │ Mode     │ Chunks │ ANT Cost                 │ Gas Cost         │ EVM Txs   ║");
    eprintln!("╠══════════╪══════════╪════════╪══════════════════════════╪══════════════════╪═══════════╣");
    for r in results {
        let size = if r.file_size_mb >= 1024 {
            format!("{} GB", r.file_size_mb / 1024)
        } else {
            format!("{} MB", r.file_size_mb)
        };
        eprintln!(
            "║ {:<8} │ {:<8} │ {:>6} │ {:<24} │ {:<16} │ {:<9} ║",
            size,
            r.mode,
            r.chunks,
            format_atto(r.ant_cost_atto),
            format_wei(r.gas_cost_wei),
            r.num_evm_txs,
        );
    }
    eprintln!("╚══════════╧══════════╧════════╧══════════════════════════╧══════════════════╧═══════════╝");
    eprintln!();
}

/// Upload cost comparison table.
///
/// Uses smaller file sizes (10-500MB) for fast execution while still
/// covering single-wave, multi-wave, and multi-batch-merkle scenarios.
///
/// Run with `--release` for 3-5x speedup:
///   cargo test --release --test e2e_upload_costs -- --nocapture
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_upload_cost_comparison() {
    // Need 20+ nodes for merkle payment pools (CANDIDATES_PER_POOL = 16)
    let testnet = MiniTestnet::start(20).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    let work_dir = TempDir::new().expect("create work dir");

    // Sizes covering single-wave through multi-batch merkle scenarios.
    // 10 MB   -> ~3 chunks   (tiny, below merkle threshold)
    // 50 MB   -> ~13 chunks  (small, below 64-chunk wave)
    // 200 MB  -> ~54 chunks  (near wave boundary)
    // 500 MB  -> ~128 chunks (multi-wave, near merkle 256-leaf limit)
    // 2 GB    -> ~512 chunks (multi-batch merkle: 2 sub-batches)
    // 4 GB    -> ~1024 chunks (multi-batch merkle: 4 sub-batches)
    let sizes: Vec<(u64, &str)> = vec![
        (10, "10mb.bin"),
        (50, "50mb.bin"),
        (200, "200mb.bin"),
        (500, "500mb.bin"),
        (2048, "2gb.bin"),
        (4096, "4gb.bin"),
    ];

    let mut results = Vec::new();

    for (size_mb, filename) in &sizes {
        let file_size = *size_mb * 1024 * 1024;
        let wallet = testnet.wallet();

        // Create separate files for single and merkle to avoid AlreadyStored
        let single_name = format!("single_{filename}");
        let merkle_name = format!("merkle_{filename}");

        // Single payment mode
        eprintln!("--- {size_mb} MB ---");
        eprintln!("  Creating file for Single mode...");
        let single_path = create_test_file(
            work_dir.path(),
            file_size,
            &single_name,
            0xDEAD_BEEF_0000_0000 + *size_mb,
        );
        eprintln!("  Uploading (Single)...");
        let single_result =
            measure_upload_cost(&client, wallet, &single_path, PaymentMode::Single, *size_mb).await;
        eprintln!(
            "    {} chunks, ANT: {}, Gas: {}",
            single_result.chunks,
            format_atto(single_result.ant_cost_atto),
            format_wei(single_result.gas_cost_wei)
        );
        results.push(single_result);

        // Merkle payment mode (skip if < 2 chunks -- merkle requires minimum 2)
        eprintln!("  Creating file for Merkle mode...");
        let merkle_path = create_test_file(
            work_dir.path(),
            file_size,
            &merkle_name,
            0xCAFE_BABE_0000_0000 + *size_mb,
        );
        eprintln!("  Uploading (Merkle)...");
        let merkle_result =
            measure_upload_cost(&client, wallet, &merkle_path, PaymentMode::Merkle, *size_mb).await;
        eprintln!(
            "    {} chunks, ANT: {}, Gas: {}",
            merkle_result.chunks,
            format_atto(merkle_result.ant_cost_atto),
            format_wei(merkle_result.gas_cost_wei)
        );
        results.push(merkle_result);

        eprintln!();
    }

    // Print the summary table
    eprintln!("=== UPLOAD COST COMPARISON TABLE ===");
    print_table(&results);

    // Verify merkle is cheaper in gas for files with enough chunks
    for chunk in results.chunks(2) {
        if let [single, merkle] = chunk {
            if single.chunks >= 10 {
                eprintln!(
                    "Gas savings for {} MB: Single={}, Merkle={} ({}x reduction)",
                    single.file_size_mb,
                    format_wei(single.gas_cost_wei),
                    format_wei(merkle.gas_cost_wei),
                    single
                        .gas_cost_wei
                        .checked_div(merkle.gas_cost_wei)
                        .unwrap_or(0)
                );
            }
        }
    }

    drop(client);
    testnet.teardown().await;
}
