use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::info;

use ant_core::data::{
    Client, CollisionPolicy, DownloadEvent, Error as DataError, PaymentMode, UploadEvent,
};
use ant_core::datamap_file::{original_name_from_datamap, read_datamap, write_datamap};

use super::chunk::parse_address;
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
        /// Override the store timeout (seconds). Applies only to this upload.
        #[arg(long)]
        store_timeout: Option<u64>,
        /// Override the store concurrency. Applies only to this upload.
        #[arg(long)]
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
            } => {
                let resolved_output = resolve_download_output(output, datamap.as_deref())?;
                handle_file_download(
                    client,
                    address.as_deref(),
                    datamap.as_deref(),
                    resolved_output,
                    json,
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
        // No progress bars in JSON mode
        client.file_upload_with_mode(path, mode).await
    } else {
        // Set up progress channel and drive progress bars
        let (tx, rx) = mpsc::channel(64);
        let pb_handle = tokio::spawn(drive_upload_progress(
            rx,
            path.display().to_string(),
            file_size,
        ));

        let upload_result = client.file_upload_with_progress(path, mode, Some(tx)).await;

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
            reason,
            ..
        }) => {
            if json_output {
                let out = UploadFailureJson {
                    error: "partial_upload",
                    total_chunks,
                    chunks_stored: stored_count,
                    chunks_failed: failed_count,
                    reason: &reason,
                };
                println!("{}", serde_json::to_string(&out)?);
            }
            anyhow::bail!(
                "Upload failed: {stored_count}/{total_chunks} stored, {failed_count} failed: {reason}"
            );
        }
        Err(e) => anyhow::bail!("File upload failed: {e}"),
    };

    let elapsed = start.elapsed();

    if public {
        let spinner = if !json_output {
            Some(progress::new_spinner("Storing public data map..."))
        } else {
            None
        };
        let dm_result = client.data_map_store(&result.data_map).await;
        if let Some(s) = &spinner {
            s.finish_and_clear();
        }
        let dm_address =
            dm_result.map_err(|e| anyhow::anyhow!("Failed to store public DataMap: {e}"))?;

        let hex_addr = hex::encode(dm_address);
        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);
        let total_chunks = result.chunks_stored + 1; // +1 for the public data map chunk

        if json_output {
            let out = UploadJsonResult {
                address: Some(hex_addr.clone()),
                datamap: None,
                mode: "public".into(),
                chunks: total_chunks,
                total_chunks,
                chunks_stored: total_chunks,
                chunks_failed: 0,
                size: file_size,
                storage_cost_atto: result.storage_cost_atto.clone(),
                gas_cost_wei: result.gas_cost_wei.to_string(),
                elapsed_secs: elapsed.as_secs_f64(),
            };
            println!("{}", serde_json::to_string(&out)?);
        } else {
            println!();
            println!("Upload complete!");
            println!("  Address: {hex_addr}");
            println!(
                "  Chunks:  {total_chunks} ({} + 1 data map)",
                result.chunks_stored
            );
            println!("  Size:    {}", format_size(file_size));
            println!("  Cost:    {cost_display}");
            println!("  Time:    {:.1}s", elapsed.as_secs_f64());
            println!();
            println!("Anyone can download this file with:");
            println!("  ant file download {hex_addr} -o <FILE>");
        }

        info!(
            "Public upload complete: address={hex_addr}, chunks={}",
            result.chunks_stored
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
            UploadEvent::WaveComplete {
                stored_so_far,
                total: _,
                ..
            } => {
                pb.set_position(stored_so_far as u64);
            }
        }
    }

    pb.finish_and_clear();
}

async fn handle_file_download(
    client: &Client,
    address: Option<&str>,
    datamap_path: Option<&Path>,
    output: PathBuf,
    json_output: bool,
) -> anyhow::Result<()> {
    let output_path = output;
    let start = Instant::now();

    let data_map = if let Some(addr_hex) = address {
        info!("Downloading public file from address {addr_hex}");
        if !json_output {
            let spinner = progress::new_spinner("Fetching data map...");
            let result = client.data_map_fetch(&parse_address(addr_hex)?).await;
            spinner.finish_and_clear();
            result.map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
        } else {
            client
                .data_map_fetch(&parse_address(addr_hex)?)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
        }
    } else {
        let dm_path = datamap_path
            .ok_or_else(|| anyhow::anyhow!("--datamap required for private download"))?;
        info!("Downloading file using datamap: {}", dm_path.display());
        read_datamap(dm_path).map_err(|e| anyhow::anyhow!("Failed to read datamap: {e}"))?
    };

    if json_output {
        client
            .file_download(&data_map, &output_path)
            .await
            .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
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

        let download_result = client
            .file_download_with_progress(&data_map, &output_path, Some(tx))
            .await;

        // Wait for progress bar cleanup (sender dropped → receiver exits)
        let _ = progress_handle.await;

        download_result.map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
    }

    let file_size = std::fs::metadata(&output_path)?.len();
    let elapsed = start.elapsed();

    if json_output {
        let out = DownloadJsonResult {
            file: output_path.display().to_string(),
            size: file_size,
            elapsed_secs: elapsed.as_secs_f64(),
        };
        println!("{}", serde_json::to_string(&out)?);
    } else {
        println!("Download complete!");
        println!("  File: {}", output_path.display());
        println!("  Size: {}", format_size(file_size));
        println!("  Time: {:.1}s", elapsed.as_secs_f64());
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

    let estimate = match raw_result {
        Ok(e) => e,
        Err(DataError::CostEstimationInconclusive(msg)) => {
            anyhow::bail!(
                "Cost estimation inconclusive: {msg}. The sampled chunks are \
                 already stored on the network, so we can't sample a representative \
                 price for the rest of the file. Try again later or upload a file \
                 that contains some new data."
            );
        }
        Err(e) => anyhow::bail!("Cost estimation failed: {e}"),
    };

    if json_output {
        println!("{}", serde_json::to_string(&estimate)?);
    } else {
        let gas_wei: u128 = estimate.estimated_gas_cost_wei.parse().unwrap_or(0);
        let cost_display = format_cost(&estimate.storage_cost_atto, gas_wei);

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
}

#[derive(Serialize)]
struct UploadFailureJson<'a> {
    error: &'a str,
    total_chunks: usize,
    chunks_stored: usize,
    chunks_failed: usize,
    reason: &'a str,
}

#[derive(Serialize)]
struct DownloadJsonResult {
    file: String,
    size: u64,
    elapsed_secs: f64,
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
}
