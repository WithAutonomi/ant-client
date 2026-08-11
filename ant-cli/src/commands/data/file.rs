use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::info;

use ant_core::data::{
    spawn_download_diagnostics_writer, Client, CollisionPolicy, CostEstimateConfidence,
    DownloadEvent, Error as DataError, FileChunkPeerReport, FileChunkPeerReportPeer,
    FileChunkPeerStatus, FileChunkPeerSweepReport, PaymentMode, UploadEvent,
};
use ant_core::datamap_file::{original_name_from_datamap, read_datamap, write_datamap};

use super::chunk::{parse_address, xor_distance_decimal};
use crate::progress;

/// File subcommands.
#[derive(Subcommand, Debug)]
pub enum FileAction {
    /// Upload a file to the network with EVM payment.
    Upload {
        /// Path to the file to upload.
        path: PathBuf,
        /// Public mode: store the data map on the network (anyone with the
        /// address can download). Default is private (data map saved locally).
        #[arg(long)]
        public: bool,
        /// Force merkle batch payment regardless of chunk count (min 2 chunks).
        /// Reduces gas costs for batches by paying in a single transaction.
        #[arg(long, conflicts_with = "no_merkle")]
        merkle: bool,
        /// Disable merkle batch payment, always use per-chunk payments.
        #[arg(long, conflicts_with = "merkle")]
        no_merkle: bool,
        /// **Deprecated.** Per-upload override for the store timeout
        /// (seconds). The adaptive controller now sizes timeouts from
        /// observed RTT; this override is retained for one release.
        #[arg(long, hide = true)]
        store_timeout: Option<u64>,
        /// **Deprecated.** Per-upload override for store concurrency.
        /// The adaptive controller now sizes concurrency from observed
        /// signals; this override is retained for one release.
        #[arg(long, hide = true)]
        store_concurrency: Option<usize>,
        /// Replace any existing `<filename>.datamap` instead of writing a
        /// suffixed `<filename>-2.datamap`. Restores the pre-helper behaviour
        /// for scripts that re-upload the same path repeatedly.
        #[arg(long)]
        overwrite: bool,
    },
    /// Download a file from the network.
    ///
    /// Public:  `ant file download ADDRESS -o output.pdf`
    /// Private: `ant file download --datamap photo.jpg.datamap`
    /// Private (custom output): `ant file download --datamap photo.jpg.datamap -o keep.jpg`
    Download {
        /// Hex-encoded address (public data map address).
        /// Required unless --datamap is provided.
        #[arg(required_unless_present = "datamap")]
        address: Option<String>,
        /// Path to a local data map file (for private downloads).
        #[arg(long)]
        datamap: Option<PathBuf>,
        /// Output file path. Required for `--address` downloads. Optional for
        /// `--datamap` downloads — defaults to the original filename derived
        /// from the datamap basename (e.g. `photo.jpg.datamap` → `photo.jpg`,
        /// written to the current directory).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Number of closest peers to try for each chunk fetch.
        #[arg(long, alias = "peer-count", value_name = "COUNT")]
        peers: Option<NonZeroUsize>,
        /// Fetch each file chunk from every selected closest peer and print
        /// ranked per-peer results after a successful download.
        #[arg(long, alias = "try-all-peers")]
        all_peers: bool,
        /// Write one JSONL record per normal-path chunk fetch attempt.
        #[arg(long, value_name = "PATH", conflicts_with = "all_peers")]
        download_diagnostics: Option<PathBuf>,
    },
    /// Estimate the cost of uploading a file without uploading.
    ///
    /// Encrypts the file locally to determine chunk count, then queries
    /// the network for a price quote. No payment or wallet required.
    Cost {
        /// Path to the file to estimate.
        path: PathBuf,
        /// Force merkle batch payment mode for the estimate.
        #[arg(long, conflicts_with = "no_merkle")]
        merkle: bool,
        /// Force single payment mode for the estimate.
        #[arg(long, conflicts_with = "merkle")]
        no_merkle: bool,
    },
}

impl FileAction {
    /// Return per-upload client config overrides, if any.
    pub fn upload_overrides(&self) -> (Option<u64>, Option<usize>) {
        match self {
            FileAction::Upload {
                store_timeout,
                store_concurrency,
                ..
            } => (*store_timeout, *store_concurrency),
            _ => (None, None),
        }
    }
}

/// Resolve the on-disk output path for `file download`.
///
/// `--address` downloads have nothing to derive from, so `-o/--output` is
/// mandatory. `--datamap` downloads default to the original filename baked
/// into the datamap basename (`photo.jpg.datamap` → `photo.jpg`), written
/// to the current working directory.
fn resolve_download_output(
    output: Option<PathBuf>,
    datamap: Option<&Path>,
) -> anyhow::Result<PathBuf> {
    if let Some(p) = output {
        return Ok(p);
    }
    let dm = datamap
        .ok_or_else(|| anyhow::anyhow!("-o/--output is required when downloading by --address"))?;
    let basename = original_name_from_datamap(dm).ok_or_else(|| {
        anyhow::anyhow!(
            "Cannot derive output filename from {}; pass -o/--output explicitly",
            dm.display()
        )
    })?;
    Ok(PathBuf::from(basename))
}

impl FileAction {
    pub async fn execute(self, client: &Client, json: bool) -> anyhow::Result<()> {
        match self {
            FileAction::Upload {
                path,
                public,
                merkle,
                no_merkle,
                store_timeout: _,
                store_concurrency: _,
                overwrite,
            } => {
                let mode = if merkle {
                    PaymentMode::Merkle
                } else if no_merkle {
                    PaymentMode::Single
                } else {
                    PaymentMode::Auto
                };
                let policy = if overwrite {
                    CollisionPolicy::Overwrite
                } else {
                    CollisionPolicy::NumericSuffix
                };
                handle_file_upload(client, &path, public, mode, policy, json).await
            }
            FileAction::Download {
                address,
                datamap,
                output,
                peers,
                all_peers,
                download_diagnostics,
            } => {
                let resolved_output = resolve_download_output(output, datamap.as_deref())?;
                handle_file_download(
                    client,
                    address.as_deref(),
                    datamap.as_deref(),
                    resolved_output,
                    json,
                    peers,
                    all_peers,
                    download_diagnostics.as_deref(),
                )
                .await
            }
            FileAction::Cost {
                path,
                merkle,
                no_merkle,
            } => {
                let mode = if merkle {
                    PaymentMode::Merkle
                } else if no_merkle {
                    PaymentMode::Single
                } else {
                    PaymentMode::Auto
                };
                handle_file_cost(client, &path, mode, json).await
            }
        }
    }
}

async fn handle_file_upload(
    client: &Client,
    path: &Path,
    public: bool,
    mode: PaymentMode,
    collision_policy: CollisionPolicy,
    json_output: bool,
) -> anyhow::Result<()> {
    let file_size = std::fs::metadata(path)?.len();
    if file_size < 3 {
        anyhow::bail!("File too small: self-encryption requires at least 3 bytes");
    }
    let start = Instant::now();

    info!(
        "Uploading file: {} ({file_size} bytes, payment mode: {mode:?})",
        path.display()
    );

    let upload_outcome = if json_output {
        // No progress bars in JSON mode.
        if public {
            client.file_upload_public_with_mode(path, mode).await
        } else {
            client.file_upload_with_mode(path, mode).await
        }
    } else {
        // Set up progress channel and drive progress bars
        let (tx, rx) = mpsc::channel(64);
        let pb_handle = tokio::spawn(drive_upload_progress(
            rx,
            path.display().to_string(),
            file_size,
        ));

        let upload_result = if public {
            client
                .file_upload_public_with_progress(path, mode, Some(tx))
                .await
        } else {
            client.file_upload_with_progress(path, mode, Some(tx)).await
        };

        // Wait for progress display to finish (sender dropped → receiver exits)
        let _ = pb_handle.await;

        upload_result
    };

    let result = match upload_outcome {
        Ok(r) => r,
        Err(DataError::PartialUpload {
            stored_count,
            failed_count,
            total_chunks,
            spend,
            reason,
            ..
        }) => {
            if json_output {
                let out = UploadFailureJson {
                    error: "partial_upload",
                    total_chunks,
                    chunks_stored: stored_count,
                    chunks_failed: failed_count,
                    storage_cost_atto: spend.storage_cost_atto.clone(),
                    gas_cost_wei: spend.gas_cost_wei.to_string(),
                    reason: &reason,
                };
                println!("{}", serde_json::to_string(&out)?);
            }
            // The partial upload still spent money on-chain for the chunks it
            // paid for; report it so the user knows what the failed attempt cost.
            let cost_display = format_cost(&spend.storage_cost_atto, spend.gas_cost_wei);
            anyhow::bail!(
                "Upload failed: {stored_count}/{total_chunks} stored, {failed_count} failed \
                 (spent {cost_display}): {reason}"
            );
        }
        Err(e) => anyhow::bail!("File upload failed: {e}"),
    };

    let elapsed = start.elapsed();

    if public {
        let dm_address = result
            .data_map_address
            .ok_or_else(|| anyhow::anyhow!("Public upload completed without a DataMap address"))?;
        let hex_addr = hex::encode(dm_address);
        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);
        let total_chunks = result.total_chunks;
        let data_chunks = total_chunks.saturating_sub(1);

        if json_output {
            let out = UploadJsonResult {
                address: Some(hex_addr.clone()),
                datamap: None,
                mode: "public".into(),
                chunks: total_chunks,
                total_chunks,
                chunks_stored: result.chunks_stored,
                chunks_failed: result.chunks_failed,
                size: file_size,
                storage_cost_atto: result.storage_cost_atto.clone(),
                gas_cost_wei: result.gas_cost_wei.to_string(),
                elapsed_secs: elapsed.as_secs_f64(),
                chunk_attempts_total: result.chunk_attempts_total,
                store_durations_ms: result.store_durations_ms.clone(),
                retries_histogram: result.retries_histogram,
            };
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!();
            println!("Upload complete!");
            println!("  Address: {hex_addr}");
            println!("  Chunks:  {total_chunks} ({} + 1 data map)", data_chunks);
            println!("  Size:    {}", format_size(file_size));
            println!("  Cost:    {cost_display}");
            println!("  Time:    {:.1}s", elapsed.as_secs_f64());
            println!();
            println!("Anyone can download this file with:");
            println!("  ant file download {hex_addr} -o <FILE>");
        }

        info!(
            "Public upload complete: address={hex_addr}, chunks={}",
            result.total_chunks
        );
    } else {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let original_name = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .ok_or_else(|| {
                anyhow::anyhow!("Cannot determine source filename from {}", path.display())
            })?;
        let datamap_path =
            write_datamap(parent, &original_name, &result.data_map, collision_policy)
                .map_err(|e| anyhow::anyhow!("Failed to persist datamap: {e}"))?;

        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);

        if json_output {
            let out = UploadJsonResult {
                address: None,
                datamap: Some(datamap_path.display().to_string()),
                mode: "private".into(),
                chunks: result.chunks_stored,
                total_chunks: result.total_chunks,
                chunks_stored: result.chunks_stored,
                chunks_failed: result.chunks_failed,
                size: file_size,
                storage_cost_atto: result.storage_cost_atto.clone(),
                gas_cost_wei: result.gas_cost_wei.to_string(),
                elapsed_secs: elapsed.as_secs_f64(),
                chunk_attempts_total: result.chunk_attempts_total,
                store_durations_ms: result.store_durations_ms.clone(),
                retries_histogram: result.retries_histogram,
            };
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!();
            println!("Upload complete!");
            println!("  Datamap: {}", datamap_path.display());
            println!("  Chunks:  {}", result.chunks_stored);
            println!("  Size:    {}", format_size(file_size));
            println!("  Cost:    {cost_display}");
            println!("  Time:    {:.1}s", elapsed.as_secs_f64());
            println!();
            println!("Download this file with:");
            println!("  ant file download --datamap {}", datamap_path.display());
        }

        info!(
            "Upload complete: datamap saved to {}, chunks={}",
            datamap_path.display(),
            result.chunks_stored
        );
    }

    Ok(())
}

/// Drive upload progress from the event channel.
///
/// Bars and spinners are routed through the shared `MultiProgress` (see
/// `progress` module), so they coexist with tracing log lines emitted at any
/// verbosity level.
async fn drive_upload_progress(
    mut rx: mpsc::Receiver<UploadEvent>,
    filename: String,
    file_size: u64,
) {
    let bar_style = ProgressStyle::with_template(
        "{spinner:.cyan} {msg}\n  [{bar:40.cyan/dim}] {pos}/{len} chunks",
    )
    .expect("valid template")
    .progress_chars("━╸━")
    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);

    let mut pb = progress::new_spinner(&format!(
        "Encrypting {filename} ({})...",
        format_size(file_size)
    ));

    while let Some(event) = rx.recv().await {
        match event {
            UploadEvent::Encrypting { chunks_done } => {
                pb.set_message(format!("Encrypting {filename} ({chunks_done} chunks)..."));
            }
            UploadEvent::Encrypted { total_chunks } => {
                pb.finish_and_clear();
                eprintln!("Encrypted into {total_chunks} chunks");
                pb = progress::attach(ProgressBar::new(total_chunks as u64));
                pb.set_style(bar_style.clone());
                pb.set_message(format!("Uploading {filename}"));
                pb.enable_steady_tick(Duration::from_millis(80));
            }
            UploadEvent::QuotingChunks { .. } => {}
            UploadEvent::ChunkQuoted { quoted, total: _ } => {
                let pos = std::cmp::max(pb.position(), quoted as u64);
                pb.set_position(pos);
            }
            UploadEvent::ChunkStored { stored, total: _ } => {
                let pos = std::cmp::max(pb.position(), stored as u64);
                pb.set_position(pos);
            }
        }
    }

    pb.finish_and_clear();
}

#[allow(clippy::too_many_arguments)]
async fn handle_file_download(
    client: &Client,
    address: Option<&str>,
    datamap_path: Option<&Path>,
    output: PathBuf,
    json_output: bool,
    peer_count: Option<NonZeroUsize>,
    all_peers: bool,
    download_diagnostics: Option<&Path>,
) -> anyhow::Result<()> {
    let output_path = output;
    let start = Instant::now();
    let (diagnostics, diagnostics_writer) = match download_diagnostics {
        Some(path) => {
            let (sender, writer) = spawn_download_diagnostics_writer(path).map_err(|e| {
                anyhow::anyhow!(
                    "Failed to open download diagnostics sidecar {}: {e}",
                    path.display()
                )
            })?;
            (Some(sender), Some(writer))
        }
        None => (None, None),
    };

    let data_map = if let Some(addr_hex) = address {
        info!("Downloading public file from address {addr_hex}");
        let address = parse_address(addr_hex)?;
        if !json_output {
            let spinner = progress::new_spinner("Fetching data map...");
            let result = if let Some(peer_count) = peer_count {
                client
                    .data_map_fetch_from_closest_peers(&address, peer_count)
                    .await
            } else {
                client.data_map_fetch(&address).await
            };
            spinner.finish_and_clear();
            result.map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
        } else {
            if let Some(peer_count) = peer_count {
                client
                    .data_map_fetch_from_closest_peers(&address, peer_count)
                    .await
            } else {
                client.data_map_fetch(&address).await
            }
            .map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
        }
    } else {
        let dm_path = datamap_path
            .ok_or_else(|| anyhow::anyhow!("--datamap required for private download"))?;
        info!("Downloading file using datamap: {}", dm_path.display());
        read_datamap(dm_path).map_err(|e| anyhow::anyhow!("Failed to read datamap: {e}"))?
    };

    let chunk_peer_check = if json_output {
        if all_peers {
            let peer_count = download_peer_check_count(client, peer_count)?;
            let report = client
                .file_download_with_peer_report_from_closest_peers(
                    &data_map,
                    &output_path,
                    None,
                    peer_count,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
            Some(file_peer_check_from_reports(report.chunk_reports))
        } else {
            let download_result = if let Some(diagnostics) = diagnostics.clone() {
                let peer_count = download_peer_check_count(client, peer_count)?;
                client
                    .file_download_with_progress_and_diagnostics_from_closest_peers(
                        &data_map,
                        &output_path,
                        None,
                        peer_count,
                        Some(diagnostics),
                    )
                    .await
            } else if let Some(peer_count) = peer_count {
                client
                    .file_download_from_closest_peers(&data_map, &output_path, peer_count)
                    .await
            } else {
                client.file_download(&data_map, &output_path).await
            };

            download_result.map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
            None
        }
    } else {
        let (tx, mut rx) = mpsc::channel(64);

        let progress_handle = tokio::spawn(async move {
            let mut pb = progress::new_spinner("Resolving data map...");

            let bar_style = ProgressStyle::with_template(
                "{spinner:.cyan} Downloading\n  [{bar:40.cyan/dim}] {pos}/{len} chunks",
            )
            .expect("valid template")
            .progress_chars("━╸━")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);

            while let Some(event) = rx.recv().await {
                match event {
                    DownloadEvent::ResolvingDataMap {
                        total_map_chunks: _,
                    } => {
                        pb.set_message("Resolving data map...".to_string());
                    }
                    DownloadEvent::MapChunkFetched { fetched } => {
                        pb.set_message(format!("Resolving data map... ({fetched} chunks)"));
                    }
                    DownloadEvent::DataMapResolved { total_chunks } => {
                        pb.finish_and_clear();
                        pb = progress::attach(ProgressBar::new(total_chunks as u64));
                        pb.set_style(bar_style.clone());
                        pb.set_message("Downloading");
                        pb.enable_steady_tick(Duration::from_millis(80));
                    }
                    DownloadEvent::ChunksFetched { fetched, total: _ } => {
                        pb.set_position(fetched as u64);
                    }
                }
            }
            pb.finish_and_clear();
        });

        let chunk_peer_check = if all_peers {
            let peer_count = download_peer_check_count(client, peer_count)?;
            let report = client
                .file_download_with_peer_report_from_closest_peers(
                    &data_map,
                    &output_path,
                    Some(tx),
                    peer_count,
                )
                .await
                .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
            Some(file_peer_check_from_reports(report.chunk_reports))
        } else {
            let download_result = if let Some(diagnostics) = diagnostics.clone() {
                let peer_count = download_peer_check_count(client, peer_count)?;
                client
                    .file_download_with_progress_and_diagnostics_from_closest_peers(
                        &data_map,
                        &output_path,
                        Some(tx),
                        peer_count,
                        Some(diagnostics),
                    )
                    .await
            } else if let Some(peer_count) = peer_count {
                client
                    .file_download_with_progress_from_closest_peers(
                        &data_map,
                        &output_path,
                        Some(tx),
                        peer_count,
                    )
                    .await
            } else {
                client
                    .file_download_with_progress(&data_map, &output_path, Some(tx))
                    .await
            };
            download_result.map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
            None
        };

        // Wait for progress bar cleanup (sender dropped → receiver exits)
        let _ = progress_handle.await;

        chunk_peer_check
    };

    drop(diagnostics);
    if let Some(writer) = diagnostics_writer {
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("Download diagnostics writer thread panicked"))?;
    }

    let file_size = std::fs::metadata(&output_path)?.len();
    let elapsed = start.elapsed();

    if json_output {
        let out = DownloadJsonResult {
            file: output_path.display().to_string(),
            size: file_size,
            elapsed_secs: elapsed.as_secs_f64(),
            chunk_peer_check,
        };
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!("Download complete!");
        println!("  File: {}", output_path.display());
        println!("  Size: {}", format_size(file_size));
        println!("  Time: {:.1}s", elapsed.as_secs_f64());

        if let Some(ref chunk_peer_check) = chunk_peer_check {
            print_file_peer_check(chunk_peer_check);
        }
    }

    Ok(())
}

async fn handle_file_cost(
    client: &Client,
    path: &Path,
    mode: PaymentMode,
    json_output: bool,
) -> anyhow::Result<()> {
    let file_size = std::fs::metadata(path)?.len();

    let raw_result = if json_output {
        client.estimate_upload_cost(path, mode, None).await
    } else {
        let (tx, rx) = mpsc::channel(64);
        let pb_handle = tokio::spawn(drive_upload_progress(
            rx,
            path.display().to_string(),
            file_size,
        ));

        let result = client.estimate_upload_cost(path, mode, Some(tx)).await;
        let _ = pb_handle.await;
        result
    };

    let estimate = raw_result.map_err(|e| anyhow::anyhow!("Cost estimation failed: {e}"))?;

    if json_output {
        println!("{}", serde_json::to_string(&estimate)?);
    } else {
        // The estimate is display-only; the real upload reconciles the true
        // cost at payment time. When every sampled chunk is already stored we
        // say so rather than print a misleading priced number.
        let cost_display = match estimate.confidence {
            CostEstimateConfidence::VerifiedAllAlreadyStored => {
                "already stored on the network — free".to_string()
            }
            CostEstimateConfidence::AllSamplesAlreadyStoredIncomplete => {
                "likely already stored — free (confirmed at payment)".to_string()
            }
            CostEstimateConfidence::PricedSample => {
                let gas_wei: u128 = estimate.estimated_gas_cost_wei.parse().unwrap_or(0);
                format_cost(&estimate.storage_cost_atto, gas_wei)
            }
        };

        println!();
        println!("Estimated upload cost for {}", path.display());
        println!("  Size:    {}", format_size(estimate.file_size));
        println!("  Chunks:  {}", estimate.chunk_count);
        println!("  Cost:    {cost_display}");
    }

    Ok(())
}

#[derive(Serialize)]
struct UploadJsonResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    datamap: Option<String>,
    mode: String,
    chunks: usize,
    total_chunks: usize,
    chunks_stored: usize,
    chunks_failed: usize,
    size: u64,
    storage_cost_atto: String,
    gas_cost_wei: String,
    elapsed_secs: f64,
    /// Sum of chunk-store RPC attempts; `>= chunks_stored` on success.
    chunk_attempts_total: usize,
    /// Per-chunk store wall-clock in ms. Empty for upload paths that
    /// don't run the wave store loop.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    store_durations_ms: Vec<u64>,
    /// Stored-chunk count by retry round (index 0 = first attempt).
    retries_histogram: [usize; 4],
}

#[derive(Serialize)]
struct UploadFailureJson<'a> {
    error: &'a str,
    total_chunks: usize,
    chunks_stored: usize,
    chunks_failed: usize,
    /// Storage cost paid on-chain so far, in atto-tokens. A partial upload
    /// still spends money for the chunks it paid for.
    storage_cost_atto: String,
    /// Gas cost paid on-chain so far, in wei.
    gas_cost_wei: String,
    reason: &'a str,
}

#[derive(Serialize)]
struct DownloadJsonResult {
    file: String,
    size: u64,
    elapsed_secs: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    chunk_peer_check: Option<FilePeerCheckJson>,
}

#[derive(Serialize)]
struct FilePeerCheckJson {
    summary: FilePeerCheckSummaryJson,
    chunks: Vec<FileChunkPeerCheckJson>,
}

#[derive(Default, Serialize)]
struct FilePeerCheckSummaryJson {
    chunks: usize,
    sweeps: usize,
    failed_sweeps: usize,
    peers_queried: usize,
    found: usize,
    not_found: usize,
    timeout: usize,
    network_error: usize,
    error: usize,
}

impl FilePeerCheckSummaryJson {
    fn record_chunk(&mut self, summary: &PeerGetSummaryJson) {
        self.peers_queried += summary.peers_queried;
        self.found += summary.found;
        self.not_found += summary.not_found;
        self.timeout += summary.timeout;
        self.network_error += summary.network_error;
        self.error += summary.error;
    }
}

#[derive(Serialize)]
struct FileChunkPeerCheckJson {
    index: usize,
    address: String,
    summary: PeerGetSummaryJson,
    sweeps: Vec<PeerSweepResultJson>,
}

#[derive(Serialize)]
struct PeerSweepResultJson {
    attempt: usize,
    deferred_retry: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    summary: PeerGetSummaryJson,
    peers: Vec<PeerGetResultJson>,
}

#[derive(Default, Serialize)]
struct PeerGetSummaryJson {
    peers_queried: usize,
    found: usize,
    not_found: usize,
    timeout: usize,
    network_error: usize,
    error: usize,
}

impl PeerGetSummaryJson {
    fn record_summary(&mut self, summary: &PeerGetSummaryJson) {
        self.peers_queried += summary.peers_queried;
        self.found += summary.found;
        self.not_found += summary.not_found;
        self.timeout += summary.timeout;
        self.network_error += summary.network_error;
        self.error += summary.error;
    }

    fn record_status(&mut self, status: &FileChunkPeerStatus) -> String {
        match status {
            FileChunkPeerStatus::Found { bytes } => {
                self.found += 1;
                format!("found bytes={bytes}")
            }
            FileChunkPeerStatus::NotFound => {
                self.not_found += 1;
                "not_found".to_string()
            }
            FileChunkPeerStatus::Timeout { message } => {
                self.timeout += 1;
                format!("timeout message={message}")
            }
            FileChunkPeerStatus::NetworkError { message } => {
                self.network_error += 1;
                format!("network_error message={message}")
            }
            FileChunkPeerStatus::Error { message } => {
                self.error += 1;
                format!("error message={message}")
            }
        }
    }
}

#[derive(Serialize)]
struct PeerGetResultJson {
    rank: usize,
    peer: String,
    distance: String,
    addrs: usize,
    result: String,
}

fn download_peer_check_count(
    client: &Client,
    peer_count: Option<NonZeroUsize>,
) -> anyhow::Result<NonZeroUsize> {
    if let Some(peer_count) = peer_count {
        return Ok(peer_count);
    }

    NonZeroUsize::new(client.config().close_group_size)
        .ok_or_else(|| anyhow::anyhow!("Client close group size must be greater than 0"))
}

fn file_peer_check_from_reports(reports: Vec<FileChunkPeerReport>) -> FilePeerCheckJson {
    let mut summary = FilePeerCheckSummaryJson {
        chunks: reports.len(),
        ..FilePeerCheckSummaryJson::default()
    };
    let chunks = reports
        .iter()
        .map(|report| {
            let chunk = file_chunk_peer_report(report);
            summary.sweeps += chunk.sweeps.len();
            summary.failed_sweeps += chunk
                .sweeps
                .iter()
                .filter(|sweep| peer_sweep_failed(sweep))
                .count();
            summary.record_chunk(&chunk.summary);
            chunk
        })
        .collect();

    FilePeerCheckJson { summary, chunks }
}

fn file_chunk_peer_report(report: &FileChunkPeerReport) -> FileChunkPeerCheckJson {
    let mut summary = PeerGetSummaryJson::default();
    let sweeps = report
        .sweeps
        .iter()
        .map(|sweep| {
            let sweep = file_chunk_peer_sweep_report(sweep);
            summary.record_summary(&sweep.summary);
            sweep
        })
        .collect();

    FileChunkPeerCheckJson {
        index: report.index,
        address: hex::encode(report.address),
        summary,
        sweeps,
    }
}

fn file_chunk_peer_sweep_report(sweep: &FileChunkPeerSweepReport) -> PeerSweepResultJson {
    let mut summary = PeerGetSummaryJson {
        peers_queried: sweep.peers.len(),
        ..PeerGetSummaryJson::default()
    };
    if sweep.error.is_some() {
        summary.error += 1;
    }
    let peers = sweep
        .peers
        .iter()
        .enumerate()
        .map(|(peer_index, peer)| peer_get_result_json(peer_index, peer, &mut summary))
        .collect();

    PeerSweepResultJson {
        attempt: sweep.attempt,
        deferred_retry: sweep.deferred_retry,
        error: sweep.error.clone(),
        summary,
        peers,
    }
}

fn peer_get_result_json(
    peer_index: usize,
    peer: &FileChunkPeerReportPeer,
    summary: &mut PeerGetSummaryJson,
) -> PeerGetResultJson {
    PeerGetResultJson {
        rank: peer_index + 1,
        peer: peer.peer_id.to_string(),
        distance: xor_distance_decimal(&peer.xor_distance),
        addrs: peer.peer_addrs.len(),
        result: summary.record_status(&peer.status),
    }
}

fn peer_sweep_failed(sweep: &PeerSweepResultJson) -> bool {
    sweep.error.is_some() || sweep.summary.found == 0
}

fn print_file_peer_check(report: &FilePeerCheckJson) {
    let total_chunks = report.summary.chunks;
    println!();
    println!("Closest peer GET results for {total_chunks} file chunk(s):");

    for chunk in &report.chunks {
        println!();
        println!("Chunk {}/{}:", chunk.index, total_chunks);
        for sweep in &chunk.sweeps {
            println!();
            let attempt = sweep.attempt;
            if sweep.deferred_retry {
                println!("Attempt {attempt} (deferred retry):");
            } else {
                println!("Attempt {attempt}:");
            }
            if let Some(error) = &sweep.error {
                println!("Sweep error: {error}");
            }
            println!("Closest peer GET results for {}:", chunk.address);
            for peer in &sweep.peers {
                let rank = peer.rank;
                let peer_id = &peer.peer;
                let distance = &peer.distance;
                let addr_count = peer.addrs;
                let status = &peer.result;
                println!(
                    "{rank}. peer={peer_id} distance={distance} addrs={addr_count} result={status}"
                );
            }
            print_peer_get_summary(&sweep.summary);
        }
        println!("Chunk aggregate:");
        print_peer_get_summary(&chunk.summary);
    }

    println!();
    print_file_peer_check_summary(&report.summary);
}

fn print_peer_get_summary(summary: &PeerGetSummaryJson) {
    let found = summary.found;
    let not_found = summary.not_found;
    let timeout = summary.timeout;
    let network_error = summary.network_error;
    let error = summary.error;
    println!(
        "Summary: found={found} not_found={not_found} timeout={timeout} network_error={network_error} error={error}",
    );
}

fn print_file_peer_check_summary(summary: &FilePeerCheckSummaryJson) {
    let chunks = summary.chunks;
    let sweeps = summary.sweeps;
    let failed_sweeps = summary.failed_sweeps;
    let peers_queried = summary.peers_queried;
    let found = summary.found;
    let not_found = summary.not_found;
    let timeout = summary.timeout;
    let network_error = summary.network_error;
    let error = summary.error;
    println!(
        "File chunk peer check summary: chunks={chunks} sweeps={sweeps} failed_sweeps={failed_sweeps} peers_queried={peers_queried} found={found} not_found={not_found} timeout={timeout} network_error={network_error} error={error}",
    );
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Format storage cost for human display.
///
/// Always shows the most readable denomination:
/// - >= 1 ANT (1e18 atto): "1.25 ANT"
/// - >= 0.001 ANT: "0.250 ANT"
/// - < 0.001 ANT: "X nanoANT"
/// - 0: "free"
fn format_storage_cost(atto_str: &str) -> String {
    let atto: u128 = atto_str.parse().unwrap_or(0);
    if atto == 0 {
        return "free".to_string();
    }
    let ant = atto as f64 / 1e18;
    if ant >= 1.0 {
        format!("{ant:.2} ANT")
    } else if ant >= 0.001 {
        format!("{ant:.4} ANT")
    } else {
        let nano = atto as f64 / 1e9;
        format!("{nano:.2} nanoANT")
    }
}

/// Format gas cost as ETH.
fn format_gas_cost(wei: u128) -> String {
    if wei == 0 {
        return "free".to_string();
    }
    let eth = wei as f64 / 1e18;
    if eth >= 0.01 {
        format!("{eth:.4} ETH")
    } else {
        format!("{eth:.6} ETH")
    }
}

/// Combined cost display.
fn format_cost(storage_cost_atto: &str, gas_cost_wei: u128) -> String {
    let atto: u128 = storage_cost_atto.parse().unwrap_or(0);
    if atto == 0 && gas_cost_wei == 0 {
        return "free (already stored)".to_string();
    }
    let storage = format_storage_cost(storage_cost_atto);
    let gas = format_gas_cost(gas_cost_wei);
    format!("{storage} (gas: {gas})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestFileCli {
        #[command(subcommand)]
        action: FileAction,
    }

    const TEST_ADDRESS_BYTE_LEN: usize = 32;
    const PUBLIC_DOWNLOAD_PEERS: usize = 12;
    const PRIVATE_DOWNLOAD_PEERS: usize = 9;
    const DIAGNOSTIC_DOWNLOAD_PEERS: usize = 5;

    fn test_address() -> String {
        "00".repeat(TEST_ADDRESS_BYTE_LEN)
    }

    #[test]
    fn resolve_download_output_returns_explicit_output_unchanged() {
        let explicit = PathBuf::from("custom/path.bin");
        let datamap = PathBuf::from("photo.jpg.datamap");
        let resolved =
            resolve_download_output(Some(explicit.clone()), Some(datamap.as_path())).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_download_output_explicit_wins_even_without_datamap() {
        let explicit = PathBuf::from("out.bin");
        let resolved = resolve_download_output(Some(explicit.clone()), None).unwrap();
        assert_eq!(resolved, explicit);
    }

    #[test]
    fn resolve_download_output_derives_from_datamap_basename() {
        let datamap = PathBuf::from("photo.jpg.datamap");
        let resolved = resolve_download_output(None, Some(datamap.as_path())).unwrap();
        assert_eq!(resolved, PathBuf::from("photo.jpg"));
    }

    #[test]
    fn resolve_download_output_derives_from_full_datamap_path() {
        let datamap = PathBuf::from("/tmp/sub/archive.tar.gz.datamap");
        let resolved = resolve_download_output(None, Some(datamap.as_path())).unwrap();
        assert_eq!(resolved, PathBuf::from("archive.tar.gz"));
    }

    #[test]
    fn resolve_download_output_errors_on_address_download_without_output() {
        let err = resolve_download_output(None, None).unwrap_err();
        assert!(
            err.to_string().contains("--output"),
            "expected --output guidance, got: {err}"
        );
    }

    #[test]
    fn resolve_download_output_errors_on_bare_dot_datamap() {
        // `.datamap` strips to an empty stem; we refuse to default-save
        // to "" and instead instruct the user to pass -o.
        let datamap = PathBuf::from(".datamap");
        let err = resolve_download_output(None, Some(datamap.as_path())).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("Cannot derive"), "got: {msg}");
        assert!(msg.contains("-o/--output"), "got: {msg}");
    }

    #[test]
    fn resolve_download_output_errors_on_non_datamap_extension() {
        let datamap = PathBuf::from("photo.jpg");
        let err = resolve_download_output(None, Some(datamap.as_path())).unwrap_err();
        assert!(err.to_string().contains("Cannot derive"));
    }

    #[test]
    fn download_peers_is_accepted_for_public_download() {
        let address = test_address();
        let peer_count = PUBLIC_DOWNLOAD_PEERS.to_string();
        let cli = TestFileCli::try_parse_from([
            "test",
            "download",
            address.as_str(),
            "--peers",
            peer_count.as_str(),
            "--output",
            "out.bin",
        ])
        .expect("--peers must parse for address downloads");

        match cli.action {
            FileAction::Download { peers, address, .. } => {
                assert!(address.is_some());
                assert_eq!(peers.map(NonZeroUsize::get), Some(PUBLIC_DOWNLOAD_PEERS));
            }
            FileAction::Upload { .. } | FileAction::Cost { .. } => {
                panic!("expected file download action")
            }
        }
    }

    #[test]
    fn download_peers_is_accepted_for_private_download() {
        let peer_count = PRIVATE_DOWNLOAD_PEERS.to_string();
        let cli = TestFileCli::try_parse_from([
            "test",
            "download",
            "--datamap",
            "photo.jpg.datamap",
            "--peers",
            peer_count.as_str(),
        ])
        .expect("--peers must parse for datamap downloads");

        match cli.action {
            FileAction::Download { peers, datamap, .. } => {
                assert!(datamap.is_some());
                assert_eq!(peers.map(NonZeroUsize::get), Some(PRIVATE_DOWNLOAD_PEERS));
            }
            FileAction::Upload { .. } | FileAction::Cost { .. } => {
                panic!("expected file download action")
            }
        }
    }

    #[test]
    fn download_peers_rejects_zero() {
        let address = test_address();
        let err = TestFileCli::try_parse_from([
            "test",
            "download",
            address.as_str(),
            "--peers",
            "0",
            "--output",
            "out.bin",
        ])
        .expect_err("--peers=0 must fail");

        assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    }

    #[test]
    fn download_all_peers_is_accepted() {
        let address = test_address();
        let cli = TestFileCli::try_parse_from([
            "test",
            "download",
            address.as_str(),
            "--all-peers",
            "--output",
            "out.bin",
        ])
        .expect("--all-peers must parse for file downloads");

        match cli.action {
            FileAction::Download { all_peers, .. } => assert!(all_peers),
            FileAction::Upload { .. } | FileAction::Cost { .. } => {
                panic!("expected file download action")
            }
        }
    }

    #[test]
    fn download_try_all_peers_alias_is_accepted_with_peer_count_alias() {
        let address = test_address();
        let peer_count = DIAGNOSTIC_DOWNLOAD_PEERS.to_string();
        let cli = TestFileCli::try_parse_from([
            "test",
            "download",
            address.as_str(),
            "--try-all-peers",
            "--peer-count",
            peer_count.as_str(),
            "--output",
            "out.bin",
        ])
        .expect("--try-all-peers and --peer-count must parse for file downloads");

        match cli.action {
            FileAction::Download {
                all_peers, peers, ..
            } => {
                assert!(all_peers);
                assert_eq!(
                    peers.map(NonZeroUsize::get),
                    Some(DIAGNOSTIC_DOWNLOAD_PEERS)
                );
            }
            FileAction::Upload { .. } | FileAction::Cost { .. } => {
                panic!("expected file download action")
            }
        }
    }
}
