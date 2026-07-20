use std::io::{Read as _, Write as _};
use std::num::NonZeroUsize;
use std::path::PathBuf;

use bytes::Bytes;
use clap::Subcommand;
use tracing::info;

use ant_core::data::client::chunk::ChunkPeerGetResult;
use ant_core::data::{Client, DataChunk, Error as DataError, MAX_CHUNK_SIZE};

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
        /// Try every selected closest peer and print ranked per-peer results.
        ///
        /// In this diagnostic mode chunk bytes are only written when
        /// -o/--output is supplied.
        #[arg(long, alias = "try-all-peers")]
        all_peers: bool,
        /// Diagnostic mode only. Number of closest peers to try with --all-peers.
        #[arg(long, alias = "peers", requires = "all_peers")]
        peer_count: Option<NonZeroUsize>,
    },
}

impl ChunkAction {
    pub async fn execute(self, client: &Client) -> anyhow::Result<()> {
        match self {
            ChunkAction::Put { file } => {
                let content = read_input(file.as_deref())?;
                let content_len = content.len();
                info!("Storing single chunk ({content_len} bytes)");

                let address = client
                    .chunk_put(Bytes::from(content))
                    .await
                    .map_err(|e| anyhow::anyhow!("Chunk put failed: {e}"))?;

                let hex_addr = hex::encode(address);
                info!("Chunk stored at {hex_addr}");
                println!("{hex_addr}");
            }
            ChunkAction::Get {
                address,
                output,
                all_peers,
                peer_count,
            } => {
                let addr = parse_address(&address)?;
                info!("Retrieving chunk {address}");
                let peer_count = peer_count.map(NonZeroUsize::get);

                if all_peers {
                    return get_chunk_from_all_peers(client, &addr, &address, output, peer_count)
                        .await;
                }

                if peer_count.is_some() {
                    anyhow::bail!("--peer-count requires --all-peers");
                }

                let result = client
                    .chunk_get(&addr)
                    .await
                    .map_err(|e| anyhow::anyhow!("Chunk get failed: {e}"))?;

                match result {
                    Some(chunk) => {
                        if let Some(path) = output {
                            std::fs::write(&path, &chunk.content)?;
                            let path_display = path.display();
                            info!("Chunk saved to {path_display}");
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
/// Base used for decimal rendering.
const DECIMAL_RADIX: u16 = 10;
/// Base represented by one byte.
const BYTE_RADIX: u16 = u8::MAX as u16 + 1;
/// ASCII value for the decimal zero digit.
const ASCII_ZERO: u8 = b'0';

#[derive(Default)]
struct PeerGetSummary {
    found: usize,
    not_found: usize,
    timeout: usize,
    network_error: usize,
    error: usize,
}

impl PeerGetSummary {
    fn record(
        &mut self,
        chunk_result: &std::result::Result<Option<DataChunk>, DataError>,
    ) -> String {
        match chunk_result {
            Ok(Some(chunk)) => {
                self.found += 1;
                let byte_count = chunk.content.len();
                format!("found bytes={byte_count}")
            }
            Ok(None) => {
                self.not_found += 1;
                "not_found".to_string()
            }
            Err(DataError::NotFound(e)) => {
                self.not_found += 1;
                format!("not_found message={e}")
            }
            Err(DataError::Timeout(e)) => {
                self.timeout += 1;
                format!("timeout message={e}")
            }
            Err(DataError::Network(e)) => {
                self.network_error += 1;
                format!("network_error message={e}")
            }
            Err(e) => {
                self.error += 1;
                format!("error message={e}")
            }
        }
    }
}

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

async fn get_chunk_from_all_peers(
    client: &Client,
    addr: &[u8; XORNAME_BYTE_LEN],
    address: &str,
    output: Option<PathBuf>,
    peer_count: Option<usize>,
) -> anyhow::Result<()> {
    let results = if let Some(peer_count) = peer_count {
        client
            .chunk_get_from_closest_peer_group(addr, peer_count)
            .await
    } else {
        client.chunk_get_from_close_group(addr).await
    }
    .map_err(|e| anyhow::anyhow!("Chunk get failed: {e}"))?;

    let summary = print_peer_get_results(address, &results);

    let chunk = results
        .iter()
        .find_map(|result| match &result.chunk_result {
            Ok(Some(chunk)) => Some(chunk),
            Ok(None) | Err(_) => None,
        });

    let Some(chunk) = chunk else {
        anyhow::bail!("Chunk not found for address {address}");
    };

    if let Some(path) = output {
        std::fs::write(&path, &chunk.content)?;
        let path_display = path.display();
        let queried = results.len();
        info!("Chunk saved to {path_display}");
        println!(
            "Saved chunk to {path_display} ({} / {queried} peers responded successfully)",
            summary.found
        );
    } else {
        let queried = results.len();
        println!(
            "Chunk found on {} / {queried} peer(s); content not written in --all-peers mode",
            summary.found
        );
    }

    Ok(())
}

fn print_peer_get_results(address: &str, results: &[ChunkPeerGetResult]) -> PeerGetSummary {
    let mut summary = PeerGetSummary::default();

    println!("Closest peer GET results for {address}:");
    for (index, result) in results.iter().enumerate() {
        let rank = index + 1;
        let distance = xor_distance_decimal(&result.xor_distance);
        let status = peer_get_status(result, &mut summary);
        let peer_id = result.peer_id;
        let addr_count = result.peer_addrs.len();
        println!("{rank}. peer={peer_id} distance={distance} addrs={addr_count} result={status}");
    }
    let found = summary.found;
    let not_found = summary.not_found;
    let timeout = summary.timeout;
    let network_error = summary.network_error;
    let error = summary.error;
    println!(
        "Summary: found={found} not_found={not_found} timeout={timeout} network_error={network_error} error={error}",
    );

    summary
}

fn peer_get_status(result: &ChunkPeerGetResult, summary: &mut PeerGetSummary) -> String {
    summary.record(&result.chunk_result)
}

pub(super) fn xor_distance_decimal(distance: &[u8; XORNAME_BYTE_LEN]) -> String {
    let mut digits = vec![0u8];

    for byte in distance {
        let mut carry = u16::from(*byte);
        for digit in &mut digits {
            let value = u16::from(*digit) * BYTE_RADIX + carry;
            *digit = (value % DECIMAL_RADIX) as u8;
            carry = value / DECIMAL_RADIX;
        }

        while carry > 0 {
            digits.push((carry % DECIMAL_RADIX) as u8);
            carry /= DECIMAL_RADIX;
        }
    }

    digits
        .iter()
        .rev()
        .map(|digit| char::from(ASCII_ZERO + *digit))
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct TestChunkCli {
        #[command(subcommand)]
        action: ChunkAction,
    }

    fn test_address() -> String {
        "00".repeat(XORNAME_BYTE_LEN)
    }

    #[test]
    fn xor_distance_decimal_formats_zero() {
        assert_eq!(xor_distance_decimal(&[0; XORNAME_BYTE_LEN]), "0");
    }

    #[test]
    fn xor_distance_decimal_formats_single_byte() {
        let mut distance = [0; XORNAME_BYTE_LEN];
        distance[XORNAME_BYTE_LEN - 1] = 42;

        assert_eq!(xor_distance_decimal(&distance), "42");
    }

    #[test]
    fn xor_distance_decimal_formats_multi_byte() {
        let mut distance = [0; XORNAME_BYTE_LEN];
        distance[XORNAME_BYTE_LEN - 2] = 1;

        assert_eq!(xor_distance_decimal(&distance), "256");
    }

    #[test]
    fn peer_count_requires_all_peers() {
        let address = test_address();
        let err =
            TestChunkCli::try_parse_from(["test", "get", address.as_str(), "--peer-count", "2"])
                .expect_err("--peer-count without --all-peers must fail");

        assert_eq!(err.kind(), clap::error::ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn peer_count_is_accepted_with_all_peers() {
        let address = test_address();
        let cli = TestChunkCli::try_parse_from([
            "test",
            "get",
            address.as_str(),
            "--all-peers",
            "--peer-count",
            "2",
        ])
        .expect("--peer-count with --all-peers must parse");

        match cli.action {
            ChunkAction::Get {
                all_peers,
                peer_count,
                ..
            } => {
                assert!(all_peers);
                assert_eq!(peer_count.map(NonZeroUsize::get), Some(2));
            }
            ChunkAction::Put { .. } => panic!("expected chunk get action"),
        }
    }

    #[test]
    fn peer_get_summary_records_tallies() {
        let mut summary = PeerGetSummary::default();
        let chunk = DataChunk::new([0; XORNAME_BYTE_LEN], Bytes::from_static(b"abc"));

        assert_eq!(summary.record(&Ok(Some(chunk))), "found bytes=3");
        assert_eq!(summary.record(&Ok(None)), "not_found");
        assert_eq!(
            summary.record(&Err(DataError::NotFound("nothing at abcd".to_string()))),
            "not_found message=nothing at abcd"
        );
        assert_eq!(
            summary.record(&Err(DataError::Timeout("slow".to_string()))),
            "timeout message=slow"
        );
        assert_eq!(
            summary.record(&Err(DataError::Network("offline".to_string()))),
            "network_error message=offline"
        );
        assert_eq!(
            summary.record(&Err(DataError::InvalidData("bad hash".to_string()))),
            "error message=invalid data: bad hash"
        );

        assert_eq!(summary.found, 1);
        assert_eq!(summary.not_found, 2);
        assert_eq!(summary.timeout, 1);
        assert_eq!(summary.network_error, 1);
        assert_eq!(summary.error, 1);
    }
}
