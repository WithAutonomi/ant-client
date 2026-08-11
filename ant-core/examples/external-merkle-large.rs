//! Real-size external-signer multi-batch merkle upload against a local devnet.
//!
//! Proves ADR-0003 at the DEFAULT per-batch cap (no test seam): a >1 GiB
//! incompressible file partitions into multiple `MAX_LEAVES`-sized
//! sub-batches, an external signer (a standalone evmlib wallet — the client's
//! prepare/finalize never touch it) pays one on-chain transaction per batch,
//! finalize folds the winner hashes and stores from the on-disk spill, and
//! the file downloads back byte-identical. Peak client RSS is sampled
//! throughout to demonstrate the spill-backed prepared upload stays far
//! below file size.
//!
//! Nodes are real `ant-node` processes with an embedded Anvil chain
//! (`LocalDevnet`), so this exercises the released node-side merkle
//! verification, not an in-process test double.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --features devnet --example external-merkle-large
//! # env overrides: FILE_MB (default 1228), NODES (default 25)
//! ```

use ant_core::data::{ExternalPaymentInfo, LocalDevnet, PaymentMode, Visibility};
use ant_node::devnet::DevnetConfig;
use ant_protocol::evm::Wallet;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

/// Peak RSS sampler: polls `ps` for our own PID until stopped. Child
/// processes (nodes, Anvil) have their own RSS, so this measures the client
/// (plus the devnet supervisor thread) only.
fn spawn_rss_sampler() -> (Arc<AtomicU64>, Arc<AtomicBool>) {
    let peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (peak_c, stop_c) = (Arc::clone(&peak), Arc::clone(&stop));
    let pid = std::process::id();
    std::thread::spawn(move || {
        while !stop_c.load(Ordering::Relaxed) {
            if let Ok(out) = std::process::Command::new("ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output()
            {
                if let Ok(kb) = String::from_utf8_lossy(&out.stdout).trim().parse::<u64>() {
                    peak_c.fetch_max(kb, Ordering::Relaxed);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    });
    (peak, stop)
}

fn mb(kb: u64) -> u64 {
    kb / 1024
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let file_mb: usize = std::env::var("FILE_MB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1228);
    let nodes: usize = std::env::var("NODES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?;

    runtime.block_on(async move {
        let (peak_rss, stop_rss) = spawn_rss_sampler();
        let started = Instant::now();

        println!("[1/7] Starting {nodes}-node local devnet + Anvil...");
        let config = DevnetConfig {
            node_count: nodes,
            ..DevnetConfig::default()
        };
        let mut devnet = LocalDevnet::start(config).await?;
        println!("      up in {:?}", started.elapsed());

        // Funded client: connectivity + one-time token approval for the same
        // key the standalone signer wallet below uses. The external
        // prepare/finalize path never touches the client's wallet.
        let client = devnet.create_funded_client().await?;
        let signer = Wallet::new_from_private_key(
            devnet.evm_network().clone(),
            devnet.wallet_private_key().trim_start_matches("0x"),
        )?;

        println!("[2/7] Writing {file_mb} MiB incompressible file...");
        let tmp = tempfile::TempDir::new()?;
        let file_path = tmp.path().join("large.bin");
        {
            // Simple xorshift PRNG — incompressible, deterministic.
            let mut f = std::io::BufWriter::new(std::fs::File::create(&file_path)?);
            let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut buf = vec![0u8; 1024 * 1024];
            for _ in 0..file_mb {
                for chunk in buf.chunks_mut(8) {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
                }
                f.write_all(&buf)?;
            }
            f.flush()?;
        }

        println!("[3/7] Preparing external upload (Auto mode, DEFAULT batch cap)...");
        let t = Instant::now();
        let prepared = client
            .file_prepare_upload_with_mode(&file_path, Visibility::Public, PaymentMode::Auto, None)
            .await?;
        let public_address = prepared
            .data_map_address
            .expect("public prepare records the DataMap address");
        let batch_payloads: Vec<(u8, Vec<ant_protocol::evm::PoolCommitment>, u64)> =
            match &prepared.payment_info {
                ExternalPaymentInfo::Merkle {
                    prepared_batches, ..
                } => prepared_batches
                    .iter()
                    .map(|b| {
                        (
                            b.depth,
                            b.pool_commitments.clone(),
                            b.merkle_payment_timestamp,
                        )
                    })
                    .collect(),
                other => panic!("expected merkle payment info, got {other:?}"),
            };
        println!(
            "      prepared {} total chunks as {} sub-batch(es) in {:?}; RSS so far: {} MiB",
            prepared.total_chunks,
            batch_payloads.len(),
            t.elapsed(),
            mb(peak_rss.load(Ordering::Relaxed)),
        );
        assert!(
            batch_payloads.len() >= 2,
            "a >1 GiB file must partition into multiple batches at the default cap"
        );

        println!(
            "[4/7] Paying {} merkle sub-batches on-chain (one tx each)...",
            batch_payloads.len()
        );
        let t = Instant::now();
        let mut winner_hashes = Vec::with_capacity(batch_payloads.len());
        for (i, (depth, commitments, ts)) in batch_payloads.into_iter().enumerate() {
            let (winner, amount, _gas) = signer.pay_for_merkle_tree(depth, commitments, ts).await?;
            println!("      batch {i}: depth={depth}, paid {amount} atto");
            winner_hashes.push(Some(winner));
        }
        println!("      payments done in {:?}", t.elapsed());

        println!("[5/7] Finalizing (stores from spill, bounded fan-out)...");
        let t = Instant::now();
        let result = client
            .finalize_upload_merkle_multi(prepared, winner_hashes)
            .await?;
        println!(
            "      stored {}/{} chunks ({} failed) in {:?}; peak RSS: {} MiB",
            result.chunks_stored,
            result.total_chunks,
            result.chunks_failed,
            t.elapsed(),
            mb(peak_rss.load(Ordering::Relaxed)),
        );
        assert_eq!(result.chunks_failed, 0);
        assert_eq!(result.chunks_stored, result.total_chunks);

        println!("[6/7] Downloading via public DataMap address and verifying...");
        let t = Instant::now();
        let fetched_map = client.data_map_fetch(&public_address).await?;
        let out_path = tmp.path().join("roundtrip.bin");
        let written = client.file_download(&fetched_map, &out_path).await?;
        assert_eq!(written as usize, file_mb * 1024 * 1024, "size mismatch");
        // Stream-compare against the regenerated PRNG stream to avoid
        // holding either copy in memory.
        {
            use std::io::Read;
            let mut f = std::io::BufReader::new(std::fs::File::open(&out_path)?);
            let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
            let mut expected = vec![0u8; 1024 * 1024];
            let mut actual = vec![0u8; 1024 * 1024];
            for mib in 0..file_mb {
                for chunk in expected.chunks_mut(8) {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
                }
                f.read_exact(&mut actual)?;
                assert_eq!(actual, expected, "content mismatch in MiB {mib}");
            }
        }
        println!("      verified byte-identical in {:?}", t.elapsed());

        stop_rss.store(true, Ordering::Relaxed);
        let peak = mb(peak_rss.load(Ordering::Relaxed));
        println!("[7/7] DONE in {:?} total.", started.elapsed());
        println!(
            "      Peak client RSS: {peak} MiB for a {file_mb} MiB file \
             (spill-backed prepare: RSS must stay well under file size)"
        );

        devnet.shutdown().await?;
        Ok::<(), Box<dyn std::error::Error>>(())
    })
}
