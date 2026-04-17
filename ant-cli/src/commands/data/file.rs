use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Subcommand;
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use tokio::sync::mpsc;
use tracing::info;

use ant_core::data::{Client, DataMap, DownloadEvent, PaymentMode, UploadEvent};

use super::chunk::parse_address;

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
    },
    /// Download a file from the network.
    ///
    /// Public:  `ant file download ADDRESS -o output.pdf`
    /// Private: `ant file download --datamap photo.datamap -o photo.jpg`
    Download {
        /// Hex-encoded address (public data map address).
        /// Required unless --datamap is provided.
        #[arg(required_unless_present = "datamap")]
        address: Option<String>,
        /// Path to a local data map file (for private downloads).
        #[arg(long)]
        datamap: Option<PathBuf>,
        /// Output file path (required).
        #[arg(short, long)]
        output: PathBuf,
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

    pub async fn execute(self, client: &Client, json: bool, verbose: u8) -> anyhow::Result<()> {
        match self {
            FileAction::Upload {
                path,
                public,
                merkle,
                no_merkle,
                store_timeout: _,
                store_concurrency: _,
            } => {
                let mode = if merkle {
                    PaymentMode::Merkle
                } else if no_merkle {
                    PaymentMode::Single
                } else {
                    PaymentMode::Auto
                };
                handle_file_upload(client, &path, public, mode, json, verbose).await
            }
            FileAction::Download {
                address,
                datamap,
                output,
            } => {
                handle_file_download(
                    client,
                    address.as_deref(),
                    datamap.as_deref(),
                    output,
                    json,
                    verbose,
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
    json_output: bool,
    verbose: u8,
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

    let result = if json_output {
        // No progress bars in JSON mode
        client
            .file_upload_with_mode(path, mode)
            .await
            .map_err(|e| anyhow::anyhow!("File upload failed: {e}"))?
    } else {
        // Set up progress channel and drive progress bars
        let (tx, rx) = mpsc::channel(64);
        let pb_handle = tokio::spawn(drive_upload_progress(
            rx,
            path.display().to_string(),
            file_size,
            verbose,
        ));

        let upload_result = client.file_upload_with_progress(path, mode, Some(tx)).await;

        // Wait for progress display to finish (sender dropped → receiver exits)
        let _ = pb_handle.await;

        upload_result.map_err(|e| anyhow::anyhow!("File upload failed: {e}"))?
    };

    let elapsed = start.elapsed();

    if public {
        let spinner = if !json_output && verbose == 0 {
            Some(new_spinner("Storing public data map..."))
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
        let datamap_path = path.with_extension("datamap");
        let datamap_bytes = serialize_datamap(&result.data_map)?;
        std::fs::write(&datamap_path, &datamap_bytes)?;

        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);

        if json_output {
            let out = UploadJsonResult {
                address: None,
                datamap: Some(datamap_path.display().to_string()),
                mode: "private".into(),
                chunks: result.chunks_stored,
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
            println!(
                "  ant file download --datamap {} -o <FILE>",
                datamap_path.display()
            );
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
/// When `verbose > 0`, progress bars are hidden to avoid garbling the
/// per-chunk `info!` lines that tracing emits to stderr.
async fn drive_upload_progress(
    mut rx: mpsc::Receiver<UploadEvent>,
    filename: String,
    file_size: u64,
    verbose: u8,
) {
    let bar_style = ProgressStyle::with_template(
        "{spinner:.cyan} {msg}\n  [{bar:40.cyan/dim}] {pos}/{len} chunks",
    )
    .expect("valid template")
    .progress_chars("━╸━")
    .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);

    let show_bars = verbose == 0 && std::io::IsTerminal::is_terminal(&std::io::stderr());

    let mut pb = if show_bars {
        new_spinner(&format!(
            "Encrypting {filename} ({})...",
            format_size(file_size)
        ))
    } else {
        ProgressBar::hidden()
    };

    while let Some(event) = rx.recv().await {
        match event {
            UploadEvent::Encrypting { chunks_done } => {
                pb.set_message(format!("Encrypting {filename} ({chunks_done} chunks)..."));
            }
            UploadEvent::Encrypted { total_chunks } => {
                pb.finish_and_clear();
                eprintln!("Encrypted into {total_chunks} chunks");
                if show_bars {
                    pb = ProgressBar::new(total_chunks as u64);
                    pb.set_style(bar_style.clone());
                    pb.set_message(format!("Uploading {filename}"));
                    pb.enable_steady_tick(Duration::from_millis(80));
                }
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
    verbose: u8,
) -> anyhow::Result<()> {
    let output_path = output;
    let start = Instant::now();

    let data_map = if let Some(addr_hex) = address {
        info!("Downloading public file from address {addr_hex}");
        if !json_output && verbose == 0 {
            let spinner = new_spinner("Fetching data map...");
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
        let datamap_bytes = std::fs::read(dm_path)?;
        deserialize_datamap(&datamap_bytes)?
    };

    if json_output {
        client
            .file_download(&data_map, &output_path)
            .await
            .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;
    } else {
        let (tx, mut rx) = mpsc::channel(64);

        let show_bars = verbose == 0 && std::io::IsTerminal::is_terminal(&std::io::stderr());
        let progress_handle = tokio::spawn(async move {
            let mut pb = if show_bars {
                let s = ProgressBar::new_spinner();
                s.set_style(
                    ProgressStyle::with_template("{spinner:.cyan} {msg}")
                        .expect("valid template")
                        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
                );
                s.set_message("Resolving data map...");
                s.enable_steady_tick(Duration::from_millis(80));
                s
            } else {
                ProgressBar::hidden()
            };

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
                        pb = if show_bars {
                            ProgressBar::new(total_chunks as u64)
                        } else {
                            ProgressBar::hidden()
                        };
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
    let spinner = if !json_output {
        Some(new_spinner("Encrypting file to estimate cost..."))
    } else {
        None
    };

    let estimate = client
        .estimate_upload_cost(path, mode, None)
        .await
        .map_err(|e| anyhow::anyhow!("Cost estimation failed: {e}"))?;

    if let Some(s) = &spinner {
        s.finish_and_clear();
    }

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
    size: u64,
    storage_cost_atto: String,
    gas_cost_wei: String,
    elapsed_secs: f64,
}

#[derive(Serialize)]
struct DownloadJsonResult {
    file: String,
    size: u64,
    elapsed_secs: f64,
}

fn new_spinner(msg: &str) -> ProgressBar {
    let pb = if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        ProgressBar::new_spinner()
    } else {
        ProgressBar::hidden()
    };
    pb.set_style(
        ProgressStyle::with_template("{spinner:.cyan} {msg}")
            .expect("valid template")
            .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));
    pb
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

fn serialize_datamap(data_map: &DataMap) -> anyhow::Result<Vec<u8>> {
    rmp_serde::to_vec(data_map).map_err(|e| anyhow::anyhow!("DataMap serialization failed: {e}"))
}

fn deserialize_datamap(bytes: &[u8]) -> anyhow::Result<DataMap> {
    rmp_serde::from_slice(bytes).map_err(|e| anyhow::anyhow!("DataMap deserialization failed: {e}"))
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
