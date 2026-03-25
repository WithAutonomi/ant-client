use std::path::{Path, PathBuf};

use clap::Subcommand;
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
    pub async fn execute(self, client: &Client) -> anyhow::Result<()> {
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
                handle_file_upload(client, &path, public, mode).await
            }
            FileAction::Download {
                address,
                datamap,
                output,
            } => handle_file_download(client, address.as_deref(), datamap.as_deref(), output).await,
        }
    }
}

async fn handle_file_upload(
    client: &Client,
    path: &Path,
    public: bool,
    mode: PaymentMode,
) -> anyhow::Result<()> {
    let file_size = std::fs::metadata(path)?.len();
    info!(
        "Uploading file: {} ({file_size} bytes, payment mode: {mode:?})",
        path.display()
    );

    let result = client
        .file_upload_with_mode(path, mode)
        .await
        .map_err(|e| anyhow::anyhow!("File upload failed: {e}"))?;

    if public {
        // Store the DataMap as a chunk on the network — anyone with the
        // returned address can download the file.
        let dm_address = client
            .data_map_store(&result.data_map)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store public DataMap: {e}"))?;

        let hex_addr = hex::encode(dm_address);
        println!("ADDRESS={hex_addr}");
        println!("MODE=public");
        println!("CHUNKS={}", result.chunks_stored);
        println!("TOTAL_SIZE={file_size}");

        info!(
            "Public upload complete: address={hex_addr}, chunks={}",
            result.chunks_stored
        );
    } else {
        // Save DataMap to disk (private — only the holder can download).
        let datamap_path = path.with_extension("datamap");
        let datamap_bytes = serialize_datamap(&result.data_map)?;
        std::fs::write(&datamap_path, &datamap_bytes)?;

        println!("DATAMAP_FILE={}", datamap_path.display());
        println!("MODE=private");
        println!("CHUNKS={}", result.chunks_stored);
        println!("TOTAL_SIZE={file_size}");

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
) -> anyhow::Result<()> {
    let output_path = output.unwrap_or_else(|| PathBuf::from("downloaded_file"));

    let data_map = if let Some(addr_hex) = address {
        // Public download: fetch the DataMap from the network by address.
        let addr = parse_address(addr_hex)?;
        info!("Downloading public file from address {addr_hex}");
        client
            .data_map_fetch(&addr)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to fetch public DataMap: {e}"))?
    } else {
        // Private download: load DataMap from local file.
        let dm_path = datamap_path
            .ok_or_else(|| anyhow::anyhow!("--datamap required for private download"))?;
        info!("Downloading file using datamap: {}", dm_path.display());
        let datamap_bytes = std::fs::read(dm_path)?;
        deserialize_datamap(&datamap_bytes)?
    };

    // Download file
    client
        .file_download(&data_map, &output_path)
        .await
        .map_err(|e| anyhow::anyhow!("Download failed: {e}"))?;

    let file_size = std::fs::metadata(&output_path)?.len();
    println!("Downloaded {file_size} bytes to {}", output_path.display());

    Ok(())
}

fn serialize_datamap(data_map: &DataMap) -> anyhow::Result<Vec<u8>> {
    rmp_serde::to_vec(data_map).map_err(|e| anyhow::anyhow!("DataMap serialization failed: {e}"))
}

fn deserialize_datamap(bytes: &[u8]) -> anyhow::Result<DataMap> {
    rmp_serde::from_slice(bytes).map_err(|e| anyhow::anyhow!("DataMap deserialization failed: {e}"))
}
