use std::path::{Path, PathBuf};
use std::time::Instant;

use clap::Subcommand;
use serde::Serialize;
use tracing::info;

use ant_core::data::{Client, DataMap, PaymentMode};

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
    },
    /// Download a file from the network.
    Download {
        /// Hex-encoded address (public data map address).
        address: Option<String>,
        /// Path to a local data map file (for private downloads).
        #[arg(long)]
        datamap: Option<PathBuf>,
        /// Output file path (defaults to `downloaded_file`).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

impl FileAction {
    pub async fn execute(self, client: &Client, json: bool) -> anyhow::Result<()> {
        match self {
            FileAction::Upload {
                path,
                public,
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
                handle_file_upload(client, &path, public, mode, json).await
            }
            FileAction::Download {
                address,
                datamap,
                output,
            } => {
                handle_file_download(client, address.as_deref(), datamap.as_deref(), output, json)
                    .await
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
) -> anyhow::Result<()> {
    let file_size = std::fs::metadata(path)?.len();
    if file_size < 3 {
        anyhow::bail!("File too small: self-encryption requires at least 3 bytes");
    }
    let start = Instant::now();

    if !json_output {
        eprintln!(
            "Uploading {} ({})...",
            path.display(),
            format_size(file_size)
        );
    }

    info!(
        "Uploading file: {} ({file_size} bytes, payment mode: {mode:?})",
        path.display()
    );

    let result = client
        .file_upload_with_mode(path, mode)
        .await
        .map_err(|e| anyhow::anyhow!("File upload failed: {e}"))?;

    let elapsed = start.elapsed();

    if public {
        // Store the DataMap as a chunk on the network
        if !json_output {
            eprint!("Storing public data map...");
        }
        let dm_address = client
            .data_map_store(&result.data_map)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store public DataMap: {e}"))?;

        let hex_addr = hex::encode(dm_address);

        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);

        if json_output {
            let out = UploadResult {
                address: Some(hex_addr.clone()),
                datamap: None,
                mode: "public".into(),
                chunks: result.chunks_stored,
                size: file_size,
                storage_cost_atto: result.storage_cost_atto.clone(),
                gas_cost_wei: result.gas_cost_wei,
                elapsed_secs: elapsed.as_secs_f64(),
            };
            println!("{}", serde_json::to_string(&out).unwrap_or_default());
        } else {
            eprintln!(" done");
            eprintln!();
            println!("Upload complete!");
            println!("  Address: {hex_addr}");
            println!("  Chunks:  {}", result.chunks_stored);
            println!("  Size:    {}", format_size(file_size));
            println!("  Cost:    {cost_display}");
            println!("  Time:    {:.1}s", elapsed.as_secs_f64());
            println!();
            println!("Anyone can download this file with:");
            println!("  ant file download {hex_addr}");
        }

        info!(
            "Public upload complete: address={hex_addr}, chunks={}",
            result.chunks_stored
        );
    } else {
        // Save DataMap to disk (private)
        let datamap_path = path.with_extension("datamap");
        let datamap_bytes = serialize_datamap(&result.data_map)?;
        std::fs::write(&datamap_path, &datamap_bytes)?;

        let cost_display = format_cost(&result.storage_cost_atto, result.gas_cost_wei);

        if json_output {
            let out = UploadResult {
                address: None,
                datamap: Some(datamap_path.display().to_string()),
                mode: "private".into(),
                chunks: result.chunks_stored,
                size: file_size,
                storage_cost_atto: result.storage_cost_atto.clone(),
                gas_cost_wei: result.gas_cost_wei,
                elapsed_secs: elapsed.as_secs_f64(),
            };
            println!("{}", serde_json::to_string(&out).unwrap_or_default());
        } else {
            eprintln!();
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

async fn handle_file_download(
    client: &Client,
    address: Option<&str>,
    datamap_path: Option<&Path>,
    output: Option<PathBuf>,
    json_output: bool,
) -> anyhow::Result<()> {
    let output_path = output.unwrap_or_else(|| PathBuf::from("downloaded_file"));
    let start = Instant::now();

    let data_map = if let Some(addr_hex) = address {
        if !json_output {
            eprintln!("Downloading from network...");
        }
        info!("Downloading public file from address {addr_hex}");
        client
            .data_map_fetch(&parse_address(addr_hex)?)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
    } else {
        let dm_path = datamap_path
            .ok_or_else(|| anyhow::anyhow!("--datamap required for private download"))?;
        if !json_output {
            eprintln!("Downloading from network...");
        }
        info!("Downloading file using datamap: {}", dm_path.display());
        let datamap_bytes = std::fs::read(dm_path)?;
        deserialize_datamap(&datamap_bytes)?
    };

    client
        .file_download(&data_map, &output_path)
        .await
        .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;

    let file_size = std::fs::metadata(&output_path)?.len();
    let elapsed = start.elapsed();

    if json_output {
        let out = DownloadResult {
            file: output_path.display().to_string(),
            size: file_size,
            elapsed_secs: elapsed.as_secs_f64(),
        };
        println!("{}", serde_json::to_string(&out).unwrap_or_default());
    } else {
        println!("Download complete!");
        println!("  File: {}", output_path.display());
        println!("  Size: {}", format_size(file_size));
        println!("  Time: {:.1}s", elapsed.as_secs_f64());
    }

    Ok(())
}

#[derive(Serialize)]
struct UploadResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    datamap: Option<String>,
    mode: String,
    chunks: usize,
    size: u64,
    storage_cost_atto: String,
    gas_cost_wei: u128,
    elapsed_secs: f64,
}

#[derive(Serialize)]
struct DownloadResult {
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
