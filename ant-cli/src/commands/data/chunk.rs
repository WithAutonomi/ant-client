use std::io::{Read as _, Write as _};
use std::path::PathBuf;

use bytes::Bytes;
use clap::Subcommand;
use tracing::info;

use ant_core::data::{Client, MAX_CHUNK_SIZE};

/// Chunk subcommands.
#[derive(Subcommand, Debug)]
pub enum ChunkAction {
    /// Store a single chunk. Reads from FILE or stdin.
    Put {
        /// Input file (reads from stdin if omitted).
        file: Option<PathBuf>,
    },
    /// Retrieve a single chunk. Writes to FILE or stdout.
    Get {
        /// Hex-encoded chunk address (64 hex chars).
        address: String,
        /// Output file (writes to stdout if omitted).
        #[arg(long, short)]
        output: Option<PathBuf>,
    },
}

impl ChunkAction {
    pub async fn execute(self, client: &Client) -> anyhow::Result<()> {
        match self {
            ChunkAction::Put { file } => {
                let content = read_input(file.as_deref())?;
                info!("Storing single chunk ({} bytes)", content.len());

                let address = client
                    .chunk_put(Bytes::from(content))
                    .await
                    .map_err(|e| anyhow::anyhow!("Chunk put failed: {e}"))?;

                let hex_addr = hex::encode(address);
                info!("Chunk stored at {hex_addr}");
                println!("{hex_addr}");
            }
            ChunkAction::Get { address, output } => {
                let addr = parse_address(&address)?;
                info!("Retrieving chunk {address}");

                let result = client
                    .chunk_get(&addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("Chunk get failed: {e}"))?;

                match result {
                    Some(chunk) => {
                        if let Some(path) = output {
                            std::fs::write(&path, &chunk.content)?;
                            info!("Chunk saved to {}", path.display());
                        } else {
                            std::io::stdout().write_all(&chunk.content)?;
                        }
                    }
                    None => {
                        anyhow::bail!("Chunk not found for address {address}");
                    }
                }
            }
        }
        Ok(())
    }
}

/// Length of an `XorName` address in bytes.
const XORNAME_BYTE_LEN: usize = 32;

fn read_input(file: Option<&std::path::Path>) -> anyhow::Result<Vec<u8>> {
    if let Some(path) = file {
        let meta = std::fs::metadata(path)?;
        if meta.len() > MAX_CHUNK_SIZE as u64 {
            anyhow::bail!(
                "Input file exceeds MAX_CHUNK_SIZE ({MAX_CHUNK_SIZE} bytes): {} bytes",
                meta.len()
            );
        }
        return Ok(std::fs::read(path)?);
    }
    let limit = (MAX_CHUNK_SIZE + 1) as u64;
    let mut buf = Vec::new();
    std::io::stdin().take(limit).read_to_end(&mut buf)?;
    if buf.len() > MAX_CHUNK_SIZE {
        anyhow::bail!("Stdin input exceeds MAX_CHUNK_SIZE ({MAX_CHUNK_SIZE} bytes)");
    }
    Ok(buf)
}

pub fn parse_address(address: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(address)?;
    if bytes.len() != XORNAME_BYTE_LEN {
        anyhow::bail!(
            "Invalid address length: expected {XORNAME_BYTE_LEN} bytes, got {}",
            bytes.len()
        );
    }
    let mut out = [0u8; XORNAME_BYTE_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}
