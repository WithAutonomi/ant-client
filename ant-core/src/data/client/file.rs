//! File operations using streaming self-encryption.
//!
//! Upload files directly from disk without loading them entirely into memory.
//! Uses `stream_encrypt` to process files in 8KB chunks, encrypting and
//! uploading each piece as it's produced.
//!
//! Encrypted chunks are spilled to a temporary directory during encryption
//! so that peak memory usage is bounded to one wave (~256 MB for 64 × 4 MB
//! chunks) regardless of file size.
//!
//! For in-memory data uploads, see the `data` module.

use crate::data::client::adaptive::{observe_op, rebucketed_unordered};
use crate::data::client::batch::{
    finalize_batch_payment, PaidChunk, PaymentIntent, PreparedChunk, WaveAggregateStats, WaveResult,
};
use crate::data::client::chunk::{ChunkFetchDiagnostics, ChunkPeerGetResult};
use crate::data::client::classify_error;
use crate::data::client::diagnostics::DownloadDiagnosticsSender;
use crate::data::client::merkle::{
    finalize_merkle_batch, merge_merkle_batch_results, merkle_batch_sizes, merkle_billable_leaves,
    merkle_deferred_retry, merkle_store_with_retry, should_use_merkle, MerkleBatchPaymentResult,
    PaymentMode, PreparedMerkleBatch, DEFERRED_ROUND_DELAYS_SECS,
};
use crate::data::client::payment::SINGLE_NODE_PAYMENT_MULTIPLIER;
use crate::data::client::Client;
use crate::data::error::{Error, PartialUploadSpend, Result};
use ant_protocol::evm::{Amount, PaymentQuote, QuoteHash, TxHash, MAX_LEAVES};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{compute_address, XorName as ChunkAddress, DATA_TYPE_CHUNK};
use bytes::Bytes;
use fs2::FileExt;
use futures::stream::{self, StreamExt};
use self_encryption::{
    get_root_data_map_parallel, stream_decrypt_batch_size, stream_encrypt,
    streaming_decrypt_with_batch_size, DataMap,
};
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use xor_name::XorName;

/// Progress events emitted during file upload for UI feedback.
#[derive(Debug, Clone)]
pub enum UploadEvent {
    /// A chunk has been encrypted and spilled to disk.
    Encrypting { chunks_done: usize },
    /// File encryption complete.
    Encrypted { total_chunks: usize },
    /// Starting quote collection for a wave.
    QuotingChunks {
        wave: usize,
        total_waves: usize,
        chunks_in_wave: usize,
    },
    /// A chunk has been quoted (peer discovery + price received).
    /// This is the slow phase — each quote involves network round-trips.
    ChunkQuoted { quoted: usize, total: usize },
    /// A chunk has been stored on the network.
    ChunkStored { stored: usize, total: usize },
}

/// Progress events emitted during file download for UI feedback.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    /// Resolving hierarchical DataMap to discover real chunk count.
    ResolvingDataMap { total_map_chunks: usize },
    /// A DataMap chunk has been fetched during resolution.
    MapChunkFetched { fetched: usize },
    /// DataMap resolved — total data chunk count now known.
    DataMapResolved { total_chunks: usize },
    /// Data chunks are being fetched from the network.
    ChunksFetched { fetched: usize, total: usize },
}

/// File download result when peer-health diagnostics are enabled.
#[derive(Debug, Clone)]
pub struct FileDownloadWithPeerReport {
    /// Number of plaintext bytes written to the destination.
    pub bytes_written: u64,
    /// Per-file-chunk closest-peer GET results collected during the actual download.
    pub chunk_reports: Vec<FileChunkPeerReport>,
}

/// Closest-peer GET results for one file chunk.
#[derive(Debug, Clone)]
pub struct FileChunkPeerReport {
    /// 1-based chunk index in the resolved file DataMap.
    pub index: usize,
    /// Chunk address.
    pub address: ChunkAddress,
    /// All diagnostic GET sweeps attempted for this chunk.
    pub sweeps: Vec<FileChunkPeerSweepReport>,
}

/// One all-peer diagnostic GET sweep for a file chunk.
#[derive(Debug, Clone)]
pub struct FileChunkPeerSweepReport {
    /// 1-based attempt number for this chunk.
    pub attempt: usize,
    /// Whether this sweep happened during a deferred retry round.
    pub deferred_retry: bool,
    /// DHT lookup / sweep-level error, if the closest-peer group could not be queried.
    pub error: Option<String>,
    /// Per-peer results, sorted closest first.
    pub peers: Vec<FileChunkPeerReportPeer>,
}

/// One peer result in a [`FileChunkPeerReport`].
#[derive(Debug, Clone)]
pub struct FileChunkPeerReportPeer {
    /// Peer queried for the chunk.
    pub peer_id: PeerId,
    /// Known network addresses used for the peer.
    pub peer_addrs: Vec<MultiAddr>,
    /// XOR distance from `peer_id` to the chunk address.
    pub xor_distance: ChunkAddress,
    /// Whether this peer returned the chunk or why it did not.
    pub status: FileChunkPeerStatus,
}

/// Peer-level file chunk GET diagnostic status.
#[derive(Debug, Clone)]
pub enum FileChunkPeerStatus {
    /// The peer returned the chunk.
    Found { bytes: usize },
    /// The peer responded authoritatively that it does not store the chunk.
    NotFound,
    /// The peer did not respond before the timeout.
    Timeout { message: String },
    /// The transport/network path to the peer failed.
    NetworkError { message: String },
    /// Any other per-peer error.
    Error { message: String },
}

/// One entry in the per-chunk quote list returned by
/// [`Client::get_store_quotes`]: the responding peer, its addresses, the
/// signed quote it returned, the payment amount it is demanding, and (ADR-0004)
/// the opaque signed-commitment blob the node shipped with the quote.
type QuoteEntry = (
    PeerId,
    Vec<MultiAddr>,
    PaymentQuote,
    Amount,
    Option<Vec<u8>>,
);

type DownloadBatchEntry = (usize, std::result::Result<Bytes, XorName>);

#[derive(Debug, Clone)]
struct RecordedFileChunkPeerSweep {
    index: usize,
    address: ChunkAddress,
    sweep: FileChunkPeerSweepReport,
}

#[derive(Clone)]
struct FileDownloadFetchContext {
    total_chunks: usize,
    peer_count: usize,
    fetched_ref: Arc<std::sync::atomic::AtomicUsize>,
    progress_ref: Option<mpsc::Sender<DownloadEvent>>,
    peer_reports: Option<Arc<Mutex<Vec<RecordedFileChunkPeerSweep>>>>,
    /// Optional runtime-gated download diagnostics sender. `None` when
    /// `--download-diagnostics` was not passed, so the chunk-fetch path
    /// skips all record construction and allocation.
    diagnostics: Option<DownloadDiagnosticsSender>,
}

/// Number of chunks per upload wave (matches batch.rs PAYMENT_WAVE_SIZE).
const UPLOAD_WAVE_SIZE: usize = 64;

/// Hard ceiling on chunk bodies held in memory at once by the merkle whole-file
/// store fan-out (`upload_merkle_from_spill`). Each in-flight store holds one
/// spilled body (≤ `MAX_CHUNK_SIZE` = 4 MiB), so this bounds peak resident store
/// memory at ~256 MiB — the same bound the old fixed 64-chunk waves gave. The
/// adaptive store cap can legitimately exceed this (`AdaptiveConfig::sanitize`
/// permits `adaptive.max.store` above 64), so the fan-out clamps its cap here to
/// keep a high configured max from pinning gigabytes of chunk bodies (PR #137
/// review). Throughput is unaffected at the default cap, which is already 64.
const MERKLE_STORE_MAX_IN_FLIGHT: usize = 64;

/// The merkle whole-file store fan-out concurrency: the adaptive store cap,
/// clamped to [`MERKLE_STORE_MAX_IN_FLIGHT`] (memory bound) and floored at 1.
fn merkle_store_cap(limiter_current: usize) -> usize {
    limiter_current.clamp(1, MERKLE_STORE_MAX_IN_FLIGHT)
}

/// Stream decrypt batches should be larger than fetch fan-out so
/// the rolling fetch scheduler can keep launching new chunk GETs as earlier
/// ones complete, instead of stopping at each self-encryption batch boundary.
const DOWNLOAD_STREAM_BATCH_FETCH_MULTIPLIER: usize = 4;

/// Use at most this fraction of currently usable RAM for one decrypt batch.
const DOWNLOAD_STREAM_BATCH_MEMORY_BUDGET_DIVISOR: u64 = 4;

/// A decrypt batch briefly holds encrypted chunk bytes, decrypted chunk bytes,
/// and Vec/Bytes overhead. Use a conservative multiplier rather than assuming
/// payload bytes alone.
const DOWNLOAD_STREAM_BATCH_BYTES_PER_CHUNK_MULTIPLIER: u64 = 3;

/// Maximum number of distinct chunk addresses to sample when probing for a
/// representative quote in [`Client::estimate_upload_cost`].
///
/// Bounded small so we never spend more than a couple of round-trips on the
/// `AlreadyStored` retry path, which only matters when many leading chunks
/// of a file already live on the network.
const ESTIMATE_SAMPLE_CAP: usize = 5;

/// First normal-path diagnostic fetch attempt.
const FIRST_DIAGNOSTIC_FETCH_ATTEMPT: usize = 1;

/// Deferred retry attempt number for retry round 0.
const DEFERRED_RETRY_ATTEMPT_OFFSET: usize = 2;

/// Pick up to `cap` chunk indices spread evenly across `[0, total)`, always
/// including the first and last chunk.
///
/// Sampling the *first* N chunks biases the probe: a file sharing a leading
/// prefix with a prior upload (compressed archives, similar headers) reports
/// those chunks as `AlreadyStored` even when the tail is new, so a positional
/// sample looks in the worst possible place. Spreading the sample means a
/// single new chunk anywhere in the file yields a real price.
///
/// Returns `[0]` for a single chunk and every index when `total <= cap`, so
/// [`Client::estimate_upload_cost`] can still detect the "whole file sampled"
/// case. Indices are strictly increasing.
fn distributed_sample_indices(total: usize, cap: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let sample_limit = total.min(cap);
    if sample_limit <= 1 {
        return vec![0];
    }
    let mut indices: Vec<usize> = (0..sample_limit)
        .map(|i| i * (total - 1) / (sample_limit - 1))
        .collect();
    indices.dedup(); // defensive: already strictly increasing for cap >= 2
    indices
}

fn file_chunk_sweep_report_from_peer_results(
    attempt: usize,
    deferred_retry: bool,
    results: &[ChunkPeerGetResult],
) -> (Option<Bytes>, FileChunkPeerSweepReport) {
    let mut content = None;
    let peers = results
        .iter()
        .map(|result| {
            if content.is_none() {
                if let Ok(Some(chunk)) = &result.chunk_result {
                    content = Some(chunk.content.clone());
                }
            }

            FileChunkPeerReportPeer {
                peer_id: result.peer_id,
                peer_addrs: result.peer_addrs.clone(),
                xor_distance: result.xor_distance,
                status: file_chunk_peer_status(&result.chunk_result),
            }
        })
        .collect();

    (
        content,
        FileChunkPeerSweepReport {
            attempt,
            deferred_retry,
            error: None,
            peers,
        },
    )
}

fn file_chunk_sweep_report_from_error(
    attempt: usize,
    deferred_retry: bool,
    error: &Error,
) -> FileChunkPeerSweepReport {
    FileChunkPeerSweepReport {
        attempt,
        deferred_retry,
        error: Some(error.to_string()),
        peers: Vec::new(),
    }
}

fn file_chunk_reports_from_recorded_sweeps(
    mut sweeps: Vec<RecordedFileChunkPeerSweep>,
) -> Vec<FileChunkPeerReport> {
    sweeps.sort_by_key(|record| (record.index, record.sweep.attempt));

    let mut reports: Vec<FileChunkPeerReport> = Vec::new();
    for record in sweeps {
        if let Some(report) = reports
            .last_mut()
            .filter(|report| report.index == record.index)
        {
            report.sweeps.push(record.sweep);
            continue;
        }

        reports.push(FileChunkPeerReport {
            index: record.index,
            address: record.address,
            sweeps: vec![record.sweep],
        });
    }

    reports
}

fn file_chunk_peer_status(
    chunk_result: &std::result::Result<Option<ant_protocol::DataChunk>, Error>,
) -> FileChunkPeerStatus {
    match chunk_result {
        Ok(Some(chunk)) => FileChunkPeerStatus::Found {
            bytes: chunk.content.len(),
        },
        Ok(None) => FileChunkPeerStatus::NotFound,
        Err(Error::Timeout(e)) => FileChunkPeerStatus::Timeout { message: e.clone() },
        Err(Error::Network(e)) => FileChunkPeerStatus::NetworkError { message: e.clone() },
        Err(e) => FileChunkPeerStatus::Error {
            message: e.to_string(),
        },
    }
}

/// Gas used by one `pay_for_quotes` transaction that packs up to
/// `UPLOAD_WAVE_SIZE` (quote_hash, rewards_address, amount) entries.
///
/// `batch_pay` in `batch.rs` flattens every chunk's close-group quotes into a
/// single EVM call, so the dominant cost is the SSTOREs for each entry plus
/// the base tx overhead. On Arbitrum that is roughly
/// `21_000 + 64 × (20_000 + small)` ≈ 1.3M; we round up to 1.5M as a
/// conservative per-wave upper bound.
const GAS_PER_WAVE_TX: u128 = 1_500_000;

/// Gas used by one merkle batch payment transaction.
///
/// One on-chain tx per merkle sub-batch, but each tx verifies a merkle tree
/// and posts a pool commitment, so budget higher than a plain transfer.
const GAS_PER_MERKLE_TX: u128 = 500_000;

/// Advisory gas price (wei/gas) used to turn the gas estimate into an ETH
/// figure when no live gas oracle is consulted.
///
/// Arbitrum One typically settles around 0.1 gwei on quiet blocks; we use
/// that as the default so the CLI prints a sensible order-of-magnitude
/// number. Users should treat the reported gas cost as an estimate, not a
/// commitment — real gas is bid at submission time.
const ARBITRUM_GAS_PRICE_WEI: u128 = 100_000_000;

/// Extra headroom percentage for disk space check.
///
/// Encrypted chunks are slightly larger than the source data due to padding
/// and self-encryption overhead. We require file_size + 10% free space in
/// the temp directory to account for this.
const DISK_SPACE_HEADROOM_PERCENT: u64 = 10;

/// Temporary on-disk buffer for encrypted chunks.
///
/// During file encryption, chunks are written to a temp directory so that
/// only their 32-byte addresses stay in memory. At upload time chunks are
/// read back one wave at a time, keeping peak RAM at ~`UPLOAD_WAVE_SIZE × 4 MB`.
/// Grace period (in seconds) before a spill dir is eligible for stale cleanup.
///
/// This is a small TOCTOU guard covering the sub-millisecond window inside
/// [`ChunkSpill::new`] between `create_dir` and `try_lock_exclusive`. Once a
/// dir is older than this and its lockfile is releasable, the owning process
/// is gone and the dir is safe to reap — regardless of how old it is.
///
/// The previous policy waited 24 h before reaping any orphan, which meant
/// that any non-graceful exit (SIGKILL, kernel OOM, panic abort) leaked its
/// spill dir until the next day's upload — and on a host being restart-looped
/// by systemd, orphans could fill the disk well within that window.
const SPILL_STALE_GRACE_SECS: u64 = 30;

/// Prefix for spill directory names to distinguish from user files.
const SPILL_DIR_PREFIX: &str = "spill_";

/// Lockfile name inside each spill dir to signal active use.
const SPILL_LOCK_NAME: &str = ".lock";

struct ChunkSpill {
    /// Directory holding spilled chunk files (named by hex address).
    dir: PathBuf,
    /// Lockfile held for the lifetime of this spill (prevents stale cleanup).
    _lock: std::fs::File,
    /// Deduplicated list of chunk addresses.
    addresses: Vec<[u8; 32]>,
    /// Tracks seen addresses for deduplication.
    seen: HashSet<[u8; 32]>,
    /// Byte size per spilled chunk address.
    sizes: HashMap<[u8; 32], u64>,
    /// Running total of unique chunk byte sizes (for average-size calculation).
    total_bytes: u64,
}

impl ChunkSpill {
    /// Return the parent directory for all spill dirs: `<data_dir>/spill/`.
    fn spill_root() -> Result<PathBuf> {
        use crate::config;
        let root = config::data_dir()
            .map_err(|e| Error::Config(format!("cannot determine data dir for spill: {e}")))?
            .join("spill");
        Ok(root)
    }

    /// Create a new spill directory under `<data_dir>/spill/`.
    ///
    /// Directory name is `spill_<timestamp>_<random>` so orphans can be
    /// identified by prefix and cleaned up by age. A lockfile inside the
    /// dir prevents concurrent cleanup from deleting an active spill.
    fn new() -> Result<Self> {
        let root = Self::spill_root()?;
        std::fs::create_dir_all(&root)?;

        // Clean up stale spill dirs from previous crashed runs.
        Self::cleanup_stale(&root);

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let unique: u64 = rand::random();
        let dir = root.join(format!("{SPILL_DIR_PREFIX}{now}_{unique}"));
        std::fs::create_dir(&dir)?;

        // Create and hold a lockfile for the lifetime of this spill.
        // cleanup_stale() will skip dirs with locked files.
        let lock_path = dir.join(SPILL_LOCK_NAME);
        let lock_file = std::fs::File::create(&lock_path).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("failed to create spill lockfile: {e}"),
            ))
        })?;
        lock_file.try_lock_exclusive().map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("failed to lock spill lockfile: {e}"),
            ))
        })?;

        Ok(Self {
            dir,
            _lock: lock_file,
            addresses: Vec::new(),
            seen: HashSet::new(),
            sizes: HashMap::new(),
            total_bytes: 0,
        })
    }

    /// Clean up stale spill directories. Best-effort, errors are logged.
    ///
    /// A spill dir is reaped when:
    /// 1. Its name starts with `SPILL_DIR_PREFIX` (ignores unrelated files)
    /// 2. It is an actual directory, not a symlink (prevents symlink attacks)
    /// 3. Its timestamp is older than `SPILL_STALE_GRACE_SECS` (TOCTOU guard)
    /// 4. Its lockfile is releasable — i.e. no live process holds it
    ///
    /// The lockfile is the primary correctness gate: a releasable lock means
    /// the owning `ChunkSpill` has been dropped or the process is gone, so
    /// the dir is fair game. The grace period covers only the brief window
    /// inside [`Self::new`] between `create_dir` and `try_lock_exclusive`.
    ///
    /// Safe to call concurrently from multiple processes.
    fn cleanup_stale(root: &Path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        if now == 0 {
            // Clock is broken (before Unix epoch). Skip cleanup to avoid
            // misidentifying dirs as stale.
            warn!("System clock before Unix epoch, skipping spill cleanup");
            return;
        }

        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();

            // Only process dirs with our prefix.
            let suffix = match name_str.strip_prefix(SPILL_DIR_PREFIX) {
                Some(s) => s,
                None => continue,
            };

            // Parse timestamp: "spill_<timestamp>_<random>"
            let timestamp: u64 = match suffix.split('_').next().and_then(|s| s.parse().ok()) {
                Some(ts) => ts,
                None => continue,
            };

            if now.saturating_sub(timestamp) < SPILL_STALE_GRACE_SECS {
                continue;
            }

            // Safety: only delete actual directories, not symlinks.
            let file_type = match entry.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if !file_type.is_dir() {
                continue;
            }

            let path = entry.path();

            // Check lockfile: if locked, the dir is in active use -- skip it.
            let lock_path = path.join(SPILL_LOCK_NAME);
            if let Ok(lock_file) = std::fs::File::open(&lock_path) {
                use fs2::FileExt;
                if lock_file.try_lock_exclusive().is_err() {
                    // Lock held by another process -- dir is active.
                    debug!("Skipping active spill dir: {}", path.display());
                    continue;
                }
                // We acquired the lock, so no one else holds it.
                // Drop it before deleting.
                drop(lock_file);
            }

            info!("Cleaning up stale spill dir: {}", path.display());
            if let Err(e) = std::fs::remove_dir_all(&path) {
                warn!("Failed to clean up stale spill dir {}: {e}", path.display());
            }
        }
    }

    /// Run stale spill cleanup. Call at client startup or periodically.
    #[allow(dead_code)]
    pub(crate) fn run_cleanup() {
        if let Ok(root) = Self::spill_root() {
            Self::cleanup_stale(&root);
        }
    }

    /// Write one encrypted chunk to disk and record its address.
    ///
    /// Deduplicates by content address: if the same chunk was already
    /// spilled, the write and accounting are skipped. This prevents
    /// double-uploads and inflated quoting metrics.
    fn push(&mut self, content: &[u8]) -> Result<()> {
        let address = compute_address(content);
        if !self.seen.insert(address) {
            return Ok(());
        }
        let path = self.dir.join(hex::encode(address));
        std::fs::write(&path, content)?;
        let content_len = content.len() as u64;
        self.sizes.insert(address, content_len);
        self.total_bytes += content_len;
        self.addresses.push(address);
        Ok(())
    }

    /// Number of chunks stored.
    fn len(&self) -> usize {
        self.addresses.len()
    }

    /// Total bytes of all spilled chunks.
    fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Address and byte-size pairs for all spilled chunks.
    fn chunk_entries(&self) -> Result<Vec<([u8; 32], u64)>> {
        self.addresses
            .iter()
            .map(|address| {
                self.sizes
                    .get(address)
                    .copied()
                    .map(|size| (*address, size))
                    .ok_or_else(|| {
                        Error::Storage(format!(
                            "missing size for spilled chunk {}",
                            hex::encode(address)
                        ))
                    })
            })
            .collect()
    }

    /// Read a single chunk back from disk by address.
    fn read_chunk(&self, address: &[u8; 32]) -> Result<Bytes> {
        let path = self.dir.join(hex::encode(address));
        let data = std::fs::read(&path).map_err(|e| {
            Error::Io(std::io::Error::new(
                e.kind(),
                format!("reading spilled chunk {}: {e}", hex::encode(address)),
            ))
        })?;
        Ok(Bytes::from(data))
    }

    /// Read the bodies for `addresses` back from disk, in the given order.
    fn read_chunks(&self, addresses: &[[u8; 32]]) -> Result<Vec<Bytes>> {
        addresses.iter().map(|addr| self.read_chunk(addr)).collect()
    }

    /// Read every spilled body back, in insertion order.
    fn read_all_chunks(&self) -> Result<Vec<Bytes>> {
        self.read_chunks(&self.addresses)
    }

    /// Clean up the spill directory.
    fn cleanup(&self) {
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            warn!(
                "Failed to clean up chunk spill dir {}: {e}",
                self.dir.display()
            );
        }
    }
}

impl Drop for ChunkSpill {
    fn drop(&mut self) {
        self.cleanup();
    }
}

fn cached_merkle_covers_addresses(
    cached: &MerkleBatchPaymentResult,
    addresses: &[[u8; 32]],
) -> bool {
    addresses
        .iter()
        .all(|addr| cached.proofs.contains_key(addr))
}

/// Split `addresses` into `(to_store, missing_proof)`: those that have a merkle
/// proof in `proofs`, and those that don't.
///
/// A partial [`MerkleBatchPaymentResult`] (from a `pay_for_merkle_multi_batch`
/// where a later sub-batch's payment failed) carries proofs only for the
/// already-paid sub-batches, so unpaid chunks reach the upload path with no
/// proof. `upload_merkle_from_spill` reports those as failed via
/// [`Error::PartialUpload`] rather than aborting the whole file. Order within
/// each group follows `addresses`.
fn partition_addresses_by_proof(
    addresses: &[[u8; 32]],
    proofs: &HashMap<[u8; 32], Vec<u8>>,
) -> (Vec<[u8; 32]>, Vec<[u8; 32]>) {
    addresses
        .iter()
        .copied()
        .partition(|addr| proofs.contains_key(addr))
}

/// Build a `PartialUpload` after a fatal merkle store error, with accurate
/// counts.
///
/// A fatal abort can leave chunks in three states: confirmed stored (in
/// `stored_addresses`), known-failed (in `known_failed` — missing proofs, the
/// quorum shortfalls and the fatal chunk seen so far), and "in flight when the
/// abort hit" (neither). Rather than trust the helpers to enumerate the last
/// group, this derives the failed set authoritatively as *every* `addresses`
/// entry not in `stored_addresses`, preferring a known per-chunk message and
/// falling back to the fatal `reason`. That guarantees
/// `stored_count + failed_count` accounts for the whole file — fixing the
/// under-reporting where a fatal wave could surface `failed_count = 0` and omit
/// same-pass successes.
fn partial_upload_after_fatal(
    addresses: &[[u8; 32]],
    stored_addresses: Vec<[u8; 32]>,
    stored_count: usize,
    total_chunks: usize,
    known_failed: Vec<([u8; 32], String)>,
    spend: PartialUploadSpend,
    reason: String,
) -> Error {
    let stored_set: HashSet<[u8; 32]> = stored_addresses.iter().copied().collect();
    let mut failed_map: HashMap<[u8; 32], String> = HashMap::new();
    for (addr, msg) in known_failed {
        if !stored_set.contains(&addr) {
            failed_map.entry(addr).or_insert(msg);
        }
    }
    for addr in addresses {
        if !stored_set.contains(addr) {
            failed_map.entry(*addr).or_insert_with(|| reason.clone());
        }
    }
    let failed: Vec<([u8; 32], String)> = failed_map.into_iter().collect();
    let failed_count = failed.len();
    Error::PartialUpload {
        stored: stored_addresses,
        stored_count,
        failed,
        failed_count,
        total_chunks,
        spend: Box::new(spend),
        reason,
    }
}

/// Require every sub-batch of a *resumable* merkle finalize to be paid.
///
/// A [`MerkleFinalizeResume`] re-drives storage against the proofs folded at
/// finalize time and accepts no new payment material, so a chunk whose
/// sub-batch was never paid could never acquire a proof on resume: every
/// [`Client::finalize_resume`] call would report it as missing-proof again and
/// the handle would never drain to [`FinalizeOutcome::Complete`]. Rejecting
/// partial payment up front keeps resume handles always drainable. A caller
/// that intends to pay only some sub-batches must use the non-resumable
/// [`Client::finalize_upload_merkle_multi`], which surfaces the unpaid chunks
/// through [`Error::PartialUpload`] (ADR-0003).
fn require_fully_paid_for_resumable(winner_pool_hashes: &[Option<[u8; 32]>]) -> Result<()> {
    let unpaid = winner_pool_hashes.iter().filter(|h| h.is_none()).count();
    if unpaid > 0 {
        return Err(Error::Payment(format!(
            "{unpaid}/{} sub-batch(es) unpaid: the resumable finalize requires every \
             sub-batch to be paid, because a resume handle cannot acquire proofs for \
             unpaid chunks and would never drain to Complete. Pay every sub-batch, or \
             use finalize_upload_merkle_multi() to finalize a partial payment (its \
             unpaid chunks are reported through PartialUpload).",
            winner_pool_hashes.len()
        )));
    }
    Ok(())
}

/// Fold the per-batch winner hashes of an external merkle upload into one
/// combined payment receipt.
///
/// Validates that `winner_pool_hashes` aligns with `prepared_batches` (one
/// entry per batch, in order), requires at least one paid batch, finalizes
/// each paid batch, and merges the receipts the way the wallet path folds
/// its sub-batch payments. Unpaid (`None`) batches contribute no proofs, so
/// the store phase reports their chunks through [`Error::PartialUpload`]
/// (ADR-0003) — the resumable path rejects them up front instead
/// ([`require_fully_paid_for_resumable`]).
fn fold_external_merkle_payments(
    prepared_batches: Vec<PreparedMerkleBatch>,
    winner_pool_hashes: Vec<Option<[u8; 32]>>,
) -> Result<MerkleBatchPaymentResult> {
    let batch_count = prepared_batches.len();
    if winner_pool_hashes.len() != batch_count {
        return Err(Error::Payment(format!(
            "Expected {batch_count} winner pool hash entries (one per \
             prepared sub-batch), got {}.",
            winner_pool_hashes.len()
        )));
    }

    let mut paid = Vec::with_capacity(batch_count);
    let mut unpaid_batches = 0usize;
    for (batch, hash) in prepared_batches.into_iter().zip(winner_pool_hashes) {
        match hash {
            Some(h) => paid.push(finalize_merkle_batch(batch, h)?),
            None => unpaid_batches += 1,
        }
    }
    if paid.is_empty() {
        return Err(Error::Payment(
            "No merkle sub-batch was paid — nothing to finalize. \
             Pay at least one batch or drop the prepared upload."
                .to_string(),
        ));
    }
    if unpaid_batches > 0 {
        warn!(
            "External merkle finalize: {unpaid_batches}/{batch_count} sub-batch(es) \
             unpaid; their chunks will be reported as failed"
        );
    }
    Ok(merge_merkle_batch_results(paid))
}

/// Assemble the outcome of one external-signer merkle store pass into
/// [`FinalizeOutcome`]. Pure (no `self`/network) so the resume-handoff contract
/// is unit-testable.
///
/// `Ok` from the store becomes [`FinalizeOutcome::Complete`]. A recoverable
/// [`Error::PartialUpload`] becomes [`FinalizeOutcome::Partial`], moving the
/// retained spill and proofs into a [`MerkleFinalizeResume`] whose
/// `unstored_addresses` are the failed chunks (to store next) and whose
/// `stored_addresses` is the cumulative stored set (carried forward as the next
/// attempt's already-stored input). Any other error is fatal and propagates
/// unchanged.
fn assemble_merkle_finalize_outcome(
    store_result: Result<(usize, String, u128, WaveAggregateStats)>,
    data_map: DataMap,
    data_map_address: Option<[u8; 32]>,
    total_chunks: usize,
    chunk_store: ExternalChunkStore,
    batch_result: MerkleBatchPaymentResult,
) -> Result<FinalizeOutcome> {
    match store_result {
        Ok((chunks_stored, _storage_cost, _gas_cost, stats)) => {
            info!("External-signer merkle upload finalized: {chunks_stored} chunks stored");
            Ok(FinalizeOutcome::Complete(FileUploadResult {
                data_map,
                chunks_stored,
                chunks_failed: 0,
                total_chunks,
                payment_mode_used: PaymentMode::Merkle,
                // The external signer pays on-chain out-of-band, so the spend
                // is unknown to the library here.
                storage_cost_atto: "0".into(),
                gas_cost_wei: 0,
                data_map_address,
                chunk_attempts_total: stats.chunk_attempts_total,
                store_durations_ms: stats.store_durations_ms,
                retries_histogram: stats.retries_histogram,
            }))
        }
        Err(Error::PartialUpload {
            stored,
            stored_count,
            failed,
            failed_count,
            spend,
            ..
        }) => {
            // Recoverable: retain the spill and the already-signed proofs so the
            // caller can drain the remainder against the same payment.
            let unstored_addresses: Vec<[u8; 32]> = failed.iter().map(|(addr, _)| *addr).collect();
            let result = FileUploadResult {
                data_map: data_map.clone(),
                chunks_stored: stored_count,
                chunks_failed: failed_count,
                total_chunks,
                payment_mode_used: PaymentMode::Merkle,
                storage_cost_atto: spend.storage_cost_atto.clone(),
                gas_cost_wei: spend.gas_cost_wei,
                data_map_address,
                // Per-attempt store telemetry is not carried on a partial.
                chunk_attempts_total: 0,
                store_durations_ms: Vec::new(),
                retries_histogram: [0; 4],
            };
            let resume = MerkleFinalizeResume {
                data_map,
                data_map_address,
                total_chunks,
                chunk_store,
                unstored_addresses,
                batch_result,
                // Cumulative stored set (already-stored + stored this pass),
                // carried forward as the next attempt's already-stored input.
                stored_addresses: stored,
            };
            Ok(FinalizeOutcome::Partial {
                result,
                resume: FinalizeResume::Merkle(Box::new(resume)),
            })
        }
        Err(e) => Err(e),
    }
}

/// Assemble the outcome of one wave-batch external store pass into
/// [`FinalizeOutcome`]. Pure (no `self`/network) so the resume-handoff contract
/// is unit-testable.
///
/// `retained` maps every paid chunk's address to its [`PaidChunk`] (body +
/// proof + PUT targets). If [`WaveResult`] reports no failures the result is
/// [`FinalizeOutcome::Complete`]; otherwise the failed chunks' [`PaidChunk`]s
/// are pulled out of `retained` into a [`WaveFinalizeResume`] so the caller can
/// re-store just those against the same payment — the store never returns an
/// `Err` for a partial, so this function is infallible.
fn assemble_wave_finalize_outcome(
    wave_result: WaveResult,
    mut retained: HashMap<[u8; 32], PaidChunk>,
    data_map: DataMap,
    data_map_address: Option<[u8; 32]>,
    total_chunks: usize,
    already_stored_count: usize,
    storage_cost_atto: String,
) -> FinalizeOutcome {
    let stored_count = already_stored_count + wave_result.stored.len();
    if wave_result.failed.is_empty() {
        info!("External-signer upload finalized: {stored_count} chunks stored");
        let mut stats = WaveAggregateStats::default();
        stats.absorb(&wave_result);
        return FinalizeOutcome::Complete(FileUploadResult {
            data_map,
            chunks_stored: stored_count,
            chunks_failed: 0,
            total_chunks,
            payment_mode_used: PaymentMode::Single,
            // Storage spend is known from the payment intent; gas is paid by the
            // external signer out-of-band (unknown here).
            storage_cost_atto,
            gas_cost_wei: 0,
            data_map_address,
            chunk_attempts_total: stats.chunk_attempts_total,
            store_durations_ms: stats.store_durations_ms,
            retries_histogram: stats.retries_histogram,
        });
    }

    // Recoverable: pull the already-paid chunks that still need storing back out
    // so the caller can re-store them against the same payment.
    let failed_count = wave_result.failed.len();
    let failed_paid_chunks: Vec<PaidChunk> = wave_result
        .failed
        .iter()
        .filter_map(|(addr, _)| retained.remove(addr))
        .collect();
    let result = FileUploadResult {
        data_map: data_map.clone(),
        chunks_stored: stored_count,
        chunks_failed: failed_count,
        total_chunks,
        payment_mode_used: PaymentMode::Single,
        storage_cost_atto: storage_cost_atto.clone(),
        gas_cost_wei: 0,
        data_map_address,
        // Per-attempt store telemetry is not carried on a partial.
        chunk_attempts_total: 0,
        store_durations_ms: Vec::new(),
        retries_histogram: [0; 4],
    };
    let resume = WaveFinalizeResume {
        data_map,
        data_map_address,
        total_chunks,
        stored_count,
        failed_paid_chunks,
        storage_cost_atto,
    };
    FinalizeOutcome::Partial {
        result,
        resume: FinalizeResume::Wave(Box::new(resume)),
    }
}

/// One wave's contribution to a single-node upload, distilled from its
/// `batch_upload_chunks_with_events` result.
#[derive(Debug)]
struct SingleWaveOutcome {
    /// Addresses confirmed stored in this wave.
    stored: Vec<[u8; 32]>,
    /// Chunks that failed after retries in this wave.
    failed: Vec<([u8; 32], String)>,
    /// Storage cost paid on-chain for this wave, in atto-tokens.
    storage_atto: Amount,
    /// Gas paid on-chain for this wave, in wei.
    gas_wei: u128,
    /// Per-wave store/retry statistics. Empty for a quorum-short wave, whose
    /// `PartialUpload` carries no stats.
    stats: WaveAggregateStats,
}

/// Fold one wave's batch-upload result for the single-node path.
///
/// A `PartialUpload` (chunks short of quorum after retries) is **recoverable**:
/// its stored/failed chunks and on-chain spend are returned so the caller
/// records them and continues to the next wave, making the file make maximum
/// progress exactly like `upload_merkle_from_spill`. Every other error is **fatal**
/// (wallet/payment-infrastructure failures, missing proofs, spill reads) and is
/// returned via `Err` to abort the file. Because `UPLOAD_WAVE_SIZE ==
/// PAYMENT_WAVE_SIZE`, each batch call is exactly one payment wave, so folding a
/// `PartialUpload` leaves nothing un-attempted within the wave.
fn fold_single_wave(
    result: Result<(Vec<[u8; 32]>, String, u128, WaveAggregateStats)>,
) -> Result<SingleWaveOutcome> {
    match result {
        Ok((stored, storage, gas, stats)) => Ok(SingleWaveOutcome {
            stored,
            failed: Vec::new(),
            storage_atto: storage.parse().unwrap_or(Amount::ZERO),
            gas_wei: gas,
            stats,
        }),
        Err(Error::PartialUpload {
            stored,
            failed,
            spend,
            ..
        }) => Ok(SingleWaveOutcome {
            stored,
            failed,
            storage_atto: spend.storage_cost_atto.parse().unwrap_or(Amount::ZERO),
            gas_wei: spend.gas_cost_wei,
            stats: WaveAggregateStats::default(),
        }),
        Err(e) => Err(e),
    }
}

/// Check that the spill directory has enough free space for the spilled chunks.
///
/// `file_size` is the source file's byte count. We require
/// `file_size + 10%` free space to account for self-encryption overhead.
fn check_disk_space_for_spill(file_size: u64) -> Result<()> {
    let spill_root = ChunkSpill::spill_root()?;

    // Ensure the root exists so fs2 can query it.
    std::fs::create_dir_all(&spill_root)?;

    let available = fs2::available_space(&spill_root).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!(
                "failed to query disk space on {}: {e}",
                spill_root.display()
            ),
        ))
    })?;

    // Use integer arithmetic to avoid f64 precision loss on large file sizes.
    let headroom = file_size / DISK_SPACE_HEADROOM_PERCENT;
    let required = file_size.saturating_add(headroom);

    if available < required {
        let avail_mb = available / (1024 * 1024);
        let req_mb = required / (1024 * 1024);
        return Err(Error::InsufficientDiskSpace(format!(
            "need ~{req_mb} MB in spill dir ({}) but only {avail_mb} MB available",
            spill_root.display()
        )));
    }

    debug!(
        "Disk space check passed: {available} bytes available, {required} bytes required (spill: {})",
        spill_root.display()
    );
    Ok(())
}

fn usable_memory_bytes() -> Option<u64> {
    let mut system = sysinfo::System::new();
    system.refresh_memory();

    let available_memory = system.available_memory();
    let free_memory = system.free_memory();
    let used_memory = system.used_memory();
    let total_memory = system.total_memory();
    let unused_memory = total_memory.saturating_sub(used_memory);

    let mut usable = [available_memory, free_memory, unused_memory]
        .into_iter()
        .filter(|bytes| *bytes > 0)
        .max();

    let cgroup_free_memory = system
        .cgroup_limits()
        .filter(|limits| limits.total_memory > 0)
        .map(|limits| limits.free_memory);
    if let Some(cgroup_free_memory) = cgroup_free_memory {
        usable = Some(usable.unwrap_or(u64::MAX).min(cgroup_free_memory));
    }

    debug!(
        available_memory,
        free_memory,
        used_memory,
        total_memory,
        cgroup_free_memory,
        usable_memory = ?usable,
        "Detected usable memory for stream decrypt batch sizing"
    );

    usable
}

fn stream_decrypt_batch_memory_cap(usable_memory_bytes: u64) -> usize {
    let budget = usable_memory_bytes / DOWNLOAD_STREAM_BATCH_MEMORY_BUDGET_DIVISOR;
    let estimated_bytes_per_chunk = (self_encryption::MAX_CHUNK_SIZE as u64)
        .saturating_mul(DOWNLOAD_STREAM_BATCH_BYTES_PER_CHUNK_MULTIPLIER)
        .max(1);
    let cap = (budget / estimated_bytes_per_chunk).max(1);

    usize::try_from(cap).unwrap_or(usize::MAX)
}

fn adaptive_stream_decrypt_batch_size(
    total_chunks: usize,
    fetch_cap: usize,
    configured_batch_floor: usize,
    usable_memory_bytes: Option<u64>,
) -> usize {
    let fetch_target = fetch_cap
        .max(1)
        .saturating_mul(DOWNLOAD_STREAM_BATCH_FETCH_MULTIPLIER);
    let requested = match usable_memory_bytes {
        Some(bytes) => {
            let memory_cap = stream_decrypt_batch_memory_cap(bytes);
            configured_batch_floor
                .max(fetch_target)
                .max(1)
                .min(memory_cap)
        }
        None => configured_batch_floor.max(1),
    };

    requested.min(total_chunks.max(1)).max(1)
}

/// Whether the data map is published to the network for address-based retrieval.
///
/// A private upload stores only the data chunks and returns the `DataMap` to
/// the caller — only someone holding that `DataMap` can reconstruct the file.
/// A public upload additionally stores the serialized `DataMap` as a chunk on
/// the network, yielding a single chunk address that anyone can use to
/// retrieve the `DataMap` (via [`Client::data_map_fetch`]) and then the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Visibility {
    /// Keep the data map local; only the holder can retrieve the file.
    #[default]
    Private,
    /// Publish the data map as a network chunk so anyone with the returned
    /// address can retrieve and decrypt the file.
    Public,
}

/// Confidence attached to an [`UploadCostEstimate`]'s `storage_cost_atto`.
///
/// `estimate_upload_cost` prices a file by sampling a few of its chunk
/// addresses and extrapolating. When every sampled chunk is already stored
/// there is no live price to extrapolate from, so a `"0"` cost can mean either
/// "provably free" (the whole file was sampled) or only "probably free" (the
/// tail was unsampled). This lets callers tell those apart instead of treating
/// every `"0"` as unconditionally free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostEstimateConfidence {
    /// At least one sampled chunk returned a live quote; `storage_cost_atto`
    /// is extrapolated from a real per-chunk price. The normal case.
    #[default]
    PricedSample,
    /// Every chunk in the file was sampled and every one was already stored.
    /// `storage_cost_atto` is exactly `"0"` — the upload is genuinely free.
    VerifiedAllAlreadyStored,
    /// Every *sampled* chunk was already stored, but not all chunks were
    /// sampled. `storage_cost_atto` is `"0"` as a best-effort guess; the real
    /// upload reconciles the true cost at payment time. Render this as "likely
    /// already stored", not a guaranteed-free price.
    AllSamplesAlreadyStoredIncomplete,
}

/// Estimated cost of uploading a file, returned by
/// [`Client::estimate_upload_cost`].
///
/// Marked `#[non_exhaustive]` so adding a field later is not a breaking change
/// for downstream consumers that construct or pattern-match on this struct.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct UploadCostEstimate {
    /// Original file size in bytes.
    pub file_size: u64,
    /// Number of chunks the file would be split into (data chunks only,
    /// does not include the DataMap chunk added during public uploads).
    pub chunk_count: usize,
    /// Estimated total storage cost in atto (token smallest unit).
    pub storage_cost_atto: String,
    /// Estimated gas cost in wei as a string. This is a rough heuristic
    /// based on chunk count and payment mode, NOT a live gas price query.
    pub estimated_gas_cost_wei: String,
    /// Payment mode that would be used.
    pub payment_mode: PaymentMode,
    /// How much to trust `storage_cost_atto`. See [`CostEstimateConfidence`].
    #[serde(default)]
    pub confidence: CostEstimateConfidence,
}

/// Result of a file upload: the `DataMap` needed to retrieve the file.
///
/// Marked `#[non_exhaustive]` so adding a new field in future is not a
/// breaking change for downstream consumers that construct or pattern-match
/// on this struct.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FileUploadResult {
    /// The data map containing chunk metadata for reconstruction.
    pub data_map: DataMap,
    /// Number of chunks stored on the network.
    pub chunks_stored: usize,
    /// Number of chunks that failed to store. Always 0 for a successful
    /// upload — partial-failure information is conveyed via
    /// [`crate::data::Error::PartialUpload`] instead.
    pub chunks_failed: usize,
    /// Total number of chunks in the upload, including chunks that were
    /// already stored and skipped. On full success this equals `chunks_stored`.
    pub total_chunks: usize,
    /// Which payment mode was actually used (not just requested).
    pub payment_mode_used: PaymentMode,
    /// Total storage cost paid in token units (atto). "0" if all chunks already existed.
    pub storage_cost_atto: String,
    /// Total gas cost in wei. 0 if no on-chain transactions were made.
    pub gas_cost_wei: u128,
    /// Chunk address of the serialized `DataMap`, set only for
    /// [`Visibility::Public`] uploads. **`Some` means this address is
    /// retrievable from the network (via [`Client::data_map_fetch`])**, not
    /// necessarily that *this* upload paid to store it — if the serialized
    /// `DataMap` hashed to a chunk that was already on the network (same
    /// file uploaded before; deterministic via self-encryption), the address
    /// is still returned but no storage payment was made for it.
    pub data_map_address: Option<[u8; 32]>,
    /// Sum of chunk-store RPC attempts across the upload
    /// (`>= chunks_stored` on full success; more if any chunk retried).
    /// `0` for paths that don't run the wave store loop.
    pub chunk_attempts_total: usize,
    /// Per-chunk store wall-clock in ms (length == `chunks_stored` on full
    /// success, empty for paths that don't run the wave store loop).
    pub store_durations_ms: Vec<u64>,
    /// Count of stored chunks that succeeded on each retry round
    /// (index 0 = first attempt, 1 = first retry, etc.). All zeros for
    /// paths that don't run the wave store loop.
    pub retries_histogram: [usize; 4],
}

/// Payment information for external signing — either wave-batch or merkle.
// ADR-0004 added the signed commitment fields (`committed_key_count`,
// `commitment_pin`) to the merkle candidate quotes carried inside
// `PreparedMerkleBatch`, which grew the `Merkle` variant past the
// `large_enum_variant` threshold. This enum is constructed one-off per payment
// (never held in bulk collections), so the size delta is harmless; allow it
// rather than box a field on the security-sensitive merkle-finalize path.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum ExternalPaymentInfo {
    /// Wave-batch: individual (quote_hash, rewards_address, amount) tuples.
    WaveBatch {
        /// Chunks ready for payment (needed for finalize).
        prepared_chunks: Vec<PreparedChunk>,
        /// Payment intent for external signing.
        payment_intent: PaymentIntent,
    },
    /// Merkle: one on-chain payment call per prepared sub-batch.
    Merkle {
        /// The prepared merkle sub-batches, in address order (public fields
        /// sent to the frontend, private fields stay in Rust). The external
        /// signer submits one `payForMerkleTree` transaction per batch;
        /// finalize takes one winner hash per batch in the same order
        /// (ADR-0003). A fresh upload below `MAX_LEAVES` chunks prepares as
        /// exactly one batch, so single-payment consumers keep working
        /// until they exceed it.
        prepared_batches: Vec<PreparedMerkleBatch>,
        /// Bodies of the chunks that still need upload, held in the
        /// encryption spill on disk — NOT resident in memory (ADR-0003).
        chunk_store: ExternalChunkStore,
        /// Chunk addresses that still need upload after the preflight check.
        chunk_addresses: Vec<[u8; 32]>,
    },
}

/// Opaque on-disk store of the chunk bodies carried by a prepared external
/// merkle upload.
///
/// Wraps the encryption spill: bodies stay on disk from prepare until
/// finalize reads them back ≤ store-cap at a time, so peak RAM for the
/// external path matches the wallet path's ~256 MB bound instead of the file
/// size (ADR-0003). The spill directory lives exactly as long as this value:
/// dropping the `PreparedUpload` (e.g. a consumer's session TTL expiring or
/// an explicit cancel) removes it from disk.
pub struct ExternalChunkStore(ChunkSpill);

impl ExternalChunkStore {
    fn from_spill(spill: ChunkSpill) -> Self {
        Self(spill)
    }

    fn spill(&self) -> &ChunkSpill {
        &self.0
    }
}

impl std::fmt::Debug for ExternalChunkStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalChunkStore")
            .field("chunks", &self.0.len())
            .field("bytes", &self.0.total_bytes())
            .finish()
    }
}

/// Prepared upload ready for external payment.
///
/// Contains everything needed to construct the on-chain payment transaction
/// externally (e.g. via WalletConnect in a desktop app) and then finalize
/// the upload without a Rust-side wallet.
///
/// Note: This struct stays in Rust memory — only the public fields of
/// `payment_info` are sent to the frontend. `PreparedChunk` contains
/// non-serializable network types, so the full struct cannot derive `Serialize`.
///
/// Marked `#[non_exhaustive]` so adding a new field in future is not a
/// breaking change for downstream consumers.
#[derive(Debug)]
#[non_exhaustive]
pub struct PreparedUpload {
    /// The data map for later retrieval.
    pub data_map: DataMap,
    /// Payment information for chunks that still need payment after the
    /// already-stored preflight. This may be wave-batch even when the original
    /// chunk count was merkle-eligible if the remaining count is below the
    /// merkle threshold.
    pub payment_info: ExternalPaymentInfo,
    /// Chunk address of the serialized `DataMap` when this upload was
    /// prepared with [`Visibility::Public`]. `Some` means the address is
    /// retrievable on the network after finalization — either because this
    /// upload paid to store the chunk in `payment_info`, or because the
    /// chunk was already on the network (deterministic self-encryption).
    /// Carried through to [`FileUploadResult::data_map_address`].
    pub data_map_address: Option<[u8; 32]>,
    /// Chunk addresses already present on the network when this upload was
    /// prepared. These do not require payment or PUT during finalization.
    pub already_stored_addresses: Vec<[u8; 32]>,
    /// Total chunk count for the upload, including already-stored chunks.
    pub total_chunks: usize,
}

/// Outcome of a resumable external-signer finalize
/// ([`Client::finalize_upload_resumable`] /
/// [`Client::finalize_upload_merkle_multi_resumable`] /
/// [`Client::finalize_resume`]).
///
/// `Complete` means every chunk is stored. `Partial` means some chunks are
/// still unstored after retries — short of quorum, or cut off by a store
/// abort; its [`FinalizeResume`] handle owns the retained payment material, so
/// the caller can store the remainder against the **same** on-chain payment
/// without re-quoting or re-signing (issue #140). Persistent store failures
/// also surface as `Partial`, so loops that retry a handle must bound their
/// attempts (see [`Client::finalize_resume`]).
#[derive(Debug)]
pub enum FinalizeOutcome {
    /// All chunks stored; the file is fully retrievable.
    Complete(FileUploadResult),
    /// Some chunks remain unstored after retries.
    Partial {
        /// Progress snapshot for this attempt (stored/failed counts, on-chain
        /// spend, `data_map_address`). Per-attempt store telemetry
        /// (`chunk_attempts_total`, `store_durations_ms`, `retries_histogram`)
        /// is not carried on a partial and reads as empty/zero.
        result: FileUploadResult,
        /// Hand back to [`Client::finalize_resume`] to store the still-unstored
        /// chunks against the same payment.
        resume: FinalizeResume,
    },
}

/// Opaque handle to resume an external-signer finalize that stored some but not
/// all chunks after retries, carrying the material needed to store the
/// remainder against the original, already-signed payment — no new quote, no
/// second signature, no double payment (issue #140).
///
/// One variant per external payment path; a caller obtains it from
/// [`FinalizeOutcome::Partial`] and passes it back to [`Client::finalize_resume`]
/// without needing to know which path produced it. Boxed variants keep the enum
/// small. Dropping it abandons the upload (the wave path frees its retained
/// chunk bodies; the merkle path removes its spill directory from disk).
#[derive(Debug)]
#[non_exhaustive]
pub enum FinalizeResume {
    /// Resume a wave-batch (single-payment) external finalize.
    Wave(Box<WaveFinalizeResume>),
    /// Resume a merkle (multi-batch) external finalize.
    Merkle(Box<MerkleFinalizeResume>),
}

/// Opaque handle to resume a wave-batch external finalize that stored some but
/// not all chunks after retries.
///
/// Owns the already-paid [`PaidChunk`]s (body + payment proof + PUT targets)
/// that still need storing; re-storing reuses those proofs, so the same
/// on-chain payment is honoured without re-signing. Dropping it frees the
/// retained chunk bodies (the upload is abandoned).
///
/// `#[non_exhaustive]` so future fields are not a breaking change. `Debug` is
/// redacted to counts only — it never prints chunk bodies, proofs, or the data
/// map.
#[non_exhaustive]
pub struct WaveFinalizeResume {
    data_map: DataMap,
    data_map_address: Option<[u8; 32]>,
    total_chunks: usize,
    stored_count: usize,
    failed_paid_chunks: Vec<PaidChunk>,
    storage_cost_atto: String,
}

impl std::fmt::Debug for WaveFinalizeResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaveFinalizeResume")
            .field("total_chunks", &self.total_chunks)
            .field("stored", &self.stored_count)
            .field("unstored", &self.failed_paid_chunks.len())
            .field("public", &self.data_map_address.is_some())
            .finish_non_exhaustive()
    }
}

/// Opaque handle to resume an external-signer merkle finalize that stored some
/// but not all chunks after retries.
///
/// Owns the on-disk chunk spill and the merkle proofs from the original,
/// already-signed payment, plus the addresses still to store. Passing it to
/// [`Client::finalize_resume`] re-drives storage for only those chunks — no new
/// quote, no second signature, no double payment (issue #140). Dropping it
/// removes the spill directory from disk (the upload is abandoned).
///
/// `#[non_exhaustive]` so future fields are not a breaking change. `Debug` is
/// redacted to counts only — it never prints chunk bodies, the data map, or
/// merkle proof material.
#[non_exhaustive]
pub struct MerkleFinalizeResume {
    data_map: DataMap,
    data_map_address: Option<[u8; 32]>,
    total_chunks: usize,
    chunk_store: ExternalChunkStore,
    unstored_addresses: Vec<[u8; 32]>,
    batch_result: MerkleBatchPaymentResult,
    stored_addresses: Vec<[u8; 32]>,
}

impl std::fmt::Debug for MerkleFinalizeResume {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MerkleFinalizeResume")
            .field("total_chunks", &self.total_chunks)
            .field("stored", &self.stored_addresses.len())
            .field("unstored", &self.unstored_addresses.len())
            .field("public", &self.data_map_address.is_some())
            .finish_non_exhaustive()
    }
}

/// Return type for [`spawn_file_encryption`]: chunk receiver, `DataMap` oneshot, join handle.
type EncryptionChannels = (
    tokio::sync::mpsc::Receiver<Bytes>,
    tokio::sync::oneshot::Receiver<DataMap>,
    tokio::task::JoinHandle<Result<()>>,
);

/// Spawn a blocking task that streams file encryption through a channel.
fn spawn_file_encryption(path: PathBuf) -> Result<EncryptionChannels> {
    let metadata = std::fs::metadata(&path)?;
    let data_size = usize::try_from(metadata.len())
        .map_err(|e| Error::Encryption(format!("file size exceeds platform usize: {e}")))?;

    let (chunk_tx, chunk_rx) = tokio::sync::mpsc::channel(2);
    let (datamap_tx, datamap_rx) = tokio::sync::oneshot::channel();

    let handle = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&path)?;
        let mut reader = std::io::BufReader::new(file);

        let read_error: Arc<Mutex<Option<std::io::Error>>> = Arc::new(Mutex::new(None));
        let read_error_clone = Arc::clone(&read_error);

        let data_iter = std::iter::from_fn(move || {
            let mut buffer = vec![0u8; 8192];
            match std::io::Read::read(&mut reader, &mut buffer) {
                Ok(0) => None,
                Ok(n) => {
                    buffer.truncate(n);
                    Some(Bytes::from(buffer))
                }
                Err(e) => {
                    let mut guard = read_error_clone
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    *guard = Some(e);
                    None
                }
            }
        });

        let mut stream = stream_encrypt(data_size, data_iter)
            .map_err(|e| Error::Encryption(format!("stream_encrypt failed: {e}")))?;

        for chunk_result in stream.chunks() {
            // Check for captured read errors immediately after each chunk.
            // stream_encrypt sees None (EOF) when a read fails, so it stops
            // producing chunks. We must detect this before sending the
            // partial results to avoid uploading a truncated DataMap.
            {
                let guard = read_error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(ref e) = *guard {
                    return Err(Error::Io(std::io::Error::new(e.kind(), e.to_string())));
                }
            }

            let (_hash, content) = chunk_result
                .map_err(|e| Error::Encryption(format!("chunk encryption failed: {e}")))?;
            if chunk_tx.blocking_send(content).is_err() {
                return Err(Error::Encryption("upload receiver dropped".to_string()));
            }
        }

        // Final check: read error after last chunk (stream saw EOF).
        {
            let guard = read_error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(ref e) = *guard {
                return Err(Error::Io(std::io::Error::new(e.kind(), e.to_string())));
            }
        }

        let datamap = stream
            .into_datamap()
            .ok_or_else(|| Error::Encryption("no DataMap after encryption".to_string()))?;
        if datamap_tx.send(datamap).is_err() {
            warn!("DataMap receiver dropped — upload may have been cancelled");
        }
        Ok(())
    });

    Ok((chunk_rx, datamap_rx, handle))
}

/// RAII guard for the staging temp file used during a disk download.
///
/// Removes the file on drop — including a panic unwind out of the
/// `block_in_place` decrypt loop — unless [`commit`](Self::commit) has
/// promoted it to its final path. Centralizes the cleanup the explicit error
/// arms used to repeat.
struct TempDownload {
    /// `Some` while the staging file may need cleanup; `None` once committed.
    path: Option<PathBuf>,
}

impl TempDownload {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Path of the staging file (valid until `commit`).
    fn path(&self) -> &Path {
        self.path
            .as_deref()
            .expect("TempDownload::path called after commit")
    }

    /// Rename the staged file to `dest`. On success the guard is defused so
    /// `Drop` is a no-op; on failure the guard stays armed and `Drop` removes
    /// the orphaned temp file.
    fn commit(mut self, dest: &Path) -> std::io::Result<()> {
        std::fs::rename(self.path(), dest)?; // err → guard armed → Drop cleans up
        self.path = None; // success → nothing left to clean
        Ok(())
    }
}

impl Drop for TempDownload {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            if let Err(e) = std::fs::remove_file(&path) {
                // Absent file is fine (never created / already gone).
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!(
                        "Failed to remove temp download file {}: {e}",
                        path.display()
                    );
                }
            }
        }
    }
}

impl Client {
    /// Upload a file to the network using streaming self-encryption.
    ///
    /// Automatically selects merkle batch payment for files that produce
    /// 64+ chunks (saves gas). Encrypted chunks are spilled to a temp
    /// directory so peak memory stays at ~256 MB regardless of file size.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, encryption fails,
    /// or any chunk cannot be stored.
    pub async fn file_upload(&self, path: &Path) -> Result<FileUploadResult> {
        self.file_upload_with_mode(path, PaymentMode::Auto).await
    }

    /// Estimate the cost of uploading a file without actually uploading.
    ///
    /// Encrypts the file to determine chunk count and sizes, then requests
    /// a single quote from the network for a representative chunk. The
    /// per-chunk price is extrapolated to the total chunk count.
    ///
    /// The estimate is fast (~2-5s) and does not require a wallet. Spilled
    /// chunks are cleaned up automatically when the function returns.
    ///
    /// Gas cost is an advisory heuristic, not a live gas-oracle query. It is
    /// derived from realistic per-transaction budgets (`GAS_PER_WAVE_TX`,
    /// `GAS_PER_MERKLE_TX`) priced at `ARBITRUM_GAS_PRICE_WEI`. Real gas
    /// varies with network conditions.
    ///
    /// Sampled chunk addresses are spread across the whole file (not the first
    /// N) so a shared leading prefix doesn't bias the sample. When a sample
    /// returns a live quote the per-chunk price is extrapolated and the result
    /// is tagged [`CostEstimateConfidence::PricedSample`].
    ///
    /// When every sampled chunk is already stored the result is still `Ok`
    /// with `storage_cost_atto: "0"`, tagged either
    /// [`CostEstimateConfidence::VerifiedAllAlreadyStored`] when the whole file
    /// was sampled (exactly free) or
    /// [`CostEstimateConfidence::AllSamplesAlreadyStoredIncomplete`] when the
    /// tail was unsampled (a best-effort guess that payment reconciles).
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, encryption fails, or the
    /// network cannot provide a quote.
    pub async fn estimate_upload_cost(
        &self,
        path: &Path,
        mode: PaymentMode,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<UploadCostEstimate> {
        let file_size = std::fs::metadata(path).map_err(Error::Io)?.len();

        if file_size < 3 {
            return Err(Error::InvalidData(
                "File too small: self-encryption requires at least 3 bytes".into(),
            ));
        }

        check_disk_space_for_spill(file_size)?;

        info!(
            "Estimating upload cost for {} ({file_size} bytes)",
            path.display()
        );

        let (spill, _data_map) = self.encrypt_file_to_spill(path, progress.as_ref()).await?;
        let chunk_count = spill.len();

        if let Some(ref tx) = progress {
            let _ = tx
                .send(UploadEvent::Encrypted {
                    total_chunks: chunk_count,
                })
                .await;
        }

        info!("Encrypted into {chunk_count} chunks, requesting quote");
        let uses_merkle = should_use_merkle(chunk_count, mode);

        // Sample chunk addresses spread evenly across the file (see
        // `distributed_sample_indices`) rather than the first N. A single
        // AlreadyStored result says nothing about the rest of the file, and a
        // positional sample lands on a shared leading prefix in the worst case,
        // so we spread the probe and only treat the whole file as "fully
        // stored" when every sample comes back stored.
        let sample_indices = distributed_sample_indices(spill.addresses.len(), ESTIMATE_SAMPLE_CAP);
        let mut sampled = 0usize;
        let mut all_already_stored = true;
        let mut quotes_opt: Option<Vec<QuoteEntry>> = None;

        for &idx in &sample_indices {
            let addr = &spill.addresses[idx];
            sampled += 1;
            let chunk_bytes = spill.read_chunk(addr)?;
            let data_size = u64::try_from(chunk_bytes.len())
                .map_err(|e| Error::InvalidData(format!("chunk size too large: {e}")))?;
            let result = if uses_merkle {
                self.get_store_quotes_with_fault_tolerance(addr, data_size, DATA_TYPE_CHUNK)
                    .await
            } else {
                self.get_store_quotes(addr, data_size, DATA_TYPE_CHUNK)
                    .await
            };
            match result {
                Ok(q) => {
                    quotes_opt = Some(q);
                    all_already_stored = false;
                    break;
                }
                Err(Error::AlreadyStored) => {
                    debug!(
                        "Sample chunk {} already stored; trying next address ({sampled}/{})",
                        hex::encode(addr),
                        sample_indices.len()
                    );
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        let quotes = match quotes_opt {
            Some(q) => q,
            None if all_already_stored && sampled == chunk_count => {
                // Every address in the file was sampled and every one is
                // already on the network — a zero-cost estimate is exact here.
                info!("All {chunk_count} chunks already stored; returning zero-cost estimate");
                return Ok(UploadCostEstimate {
                    file_size,
                    chunk_count,
                    storage_cost_atto: "0".into(),
                    estimated_gas_cost_wei: "0".into(),
                    payment_mode: if uses_merkle {
                        PaymentMode::Merkle
                    } else {
                        PaymentMode::Single
                    },
                    confidence: CostEstimateConfidence::VerifiedAllAlreadyStored,
                });
            }
            None => {
                // Every sampled chunk was already stored but the tail was not
                // sampled, so there is no live price to extrapolate. The
                // estimate is display-only and payment reconciles the true
                // cost, so return an optimistic zero flagged as incomplete
                // rather than erroring — callers still get a value to show.
                info!(
                    "All {sampled}/{chunk_count} sampled chunks already stored; \
                     returning incomplete zero-cost estimate"
                );
                return Ok(UploadCostEstimate {
                    file_size,
                    chunk_count,
                    storage_cost_atto: "0".into(),
                    estimated_gas_cost_wei: "0".into(),
                    payment_mode: if uses_merkle {
                        PaymentMode::Merkle
                    } else {
                        PaymentMode::Single
                    },
                    confidence: CostEstimateConfidence::AllSamplesAlreadyStoredIncomplete,
                });
            }
        };

        // Use the median price × 3, matching the settlement multiplier both
        // payment paths now apply.
        let mut prices: Vec<Amount> = quotes.iter().map(|(_, _, _, price, _)| *price).collect();
        prices.sort();
        let median_price = prices
            .get(prices.len() / 2)
            .copied()
            .unwrap_or(Amount::ZERO);
        let per_chunk_cost = median_price * Amount::from(SINGLE_NODE_PAYMENT_MULTIPLIER);

        let chunk_count_u64 = u64::try_from(chunk_count).unwrap_or(u64::MAX);
        // Merkle settles per *padded* leaf, not per chunk: the contract charges
        // `median16 × 2^depth` per batch and the tree rounds up to a power of
        // two, so a 65-chunk batch pays for 128 leaves. The leaf total is
        // summed over the batches the payment path really builds
        // (`merkle_batch_sizes`), so the estimate cannot drift from execution.
        let billable_units = if uses_merkle {
            merkle_billable_leaves(chunk_count_u64)
        } else {
            chunk_count_u64
        };
        let total_storage = per_chunk_cost * Amount::from(billable_units);

        // Estimate gas cost from realistic per-transaction budgets rather
        // than a flat per-chunk or per-wave number.
        //
        // - Single mode: `batch_pay` packs up to UPLOAD_WAVE_SIZE chunks'
        //   close-group quotes into one `pay_for_quotes` call on Arbitrum.
        //   The dominant cost is one SSTORE per entry plus base tx overhead,
        //   so we use GAS_PER_WAVE_TX (≈1.5M) as a conservative upper bound
        //   on a full wave and multiply by the number of waves. The previous
        //   per-wave figure of 150k was closer to a single-entry transfer
        //   and understated cost by 5–10x for full waves.
        // - Merkle mode: one tx per sub-batch that verifies a merkle tree
        //   and posts a pool commitment (GAS_PER_MERKLE_TX ≈ 500k each).
        //
        // Gas is priced at ARBITRUM_GAS_PRICE_WEI (~0.1 gwei, a typical
        // Arbitrum baseline). Treat the result as advisory, not a commitment.
        let waves = u128::try_from(chunk_count.div_ceil(UPLOAD_WAVE_SIZE)).unwrap_or(u128::MAX);
        // One tx per batch the payment path builds — same partition the leaf
        // total above is derived from.
        let merkle_batches =
            u128::try_from(merkle_batch_sizes(chunk_count).len()).unwrap_or(u128::MAX);
        let estimated_gas: u128 = if uses_merkle {
            merkle_batches
                .saturating_mul(GAS_PER_MERKLE_TX)
                .saturating_mul(ARBITRUM_GAS_PRICE_WEI)
        } else {
            waves
                .saturating_mul(GAS_PER_WAVE_TX)
                .saturating_mul(ARBITRUM_GAS_PRICE_WEI)
        };

        info!(
            "Estimate: {chunk_count} chunks, storage={total_storage} atto, gas~={estimated_gas} wei"
        );

        Ok(UploadCostEstimate {
            file_size,
            chunk_count,
            storage_cost_atto: total_storage.to_string(),
            estimated_gas_cost_wei: estimated_gas.to_string(),
            payment_mode: if uses_merkle {
                PaymentMode::Merkle
            } else {
                PaymentMode::Single
            },
            confidence: CostEstimateConfidence::PricedSample,
        })
    }

    /// Phase 1 of external-signer upload: encrypt file and prepare chunks.
    ///
    /// Equivalent to [`Client::file_prepare_upload_with_visibility`] with
    /// [`Visibility::Private`] — see that method for details.
    pub async fn file_prepare_upload(&self, path: &Path) -> Result<PreparedUpload> {
        self.file_prepare_upload_with_progress(path, Visibility::Private, None)
            .await
    }

    /// Phase 1 of external-signer upload with explicit [`Visibility`] control.
    ///
    /// Equivalent to [`Client::file_prepare_upload_with_progress`] with
    /// `progress: None` — see that method for details.
    pub async fn file_prepare_upload_with_visibility(
        &self,
        path: &Path,
        visibility: Visibility,
    ) -> Result<PreparedUpload> {
        self.file_prepare_upload_with_progress(path, visibility, None)
            .await
    }

    /// Phase 1 of external-signer upload with progress events.
    ///
    /// Equivalent to [`Client::file_prepare_upload_with_mode`] with
    /// [`PaymentMode::Auto`] — see that method for details.
    pub async fn file_prepare_upload_with_progress(
        &self,
        path: &Path,
        visibility: Visibility,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<PreparedUpload> {
        self.file_prepare_upload_with_mode(path, visibility, PaymentMode::Auto, progress)
            .await
    }

    /// Phase 1 of external-signer upload with an explicit [`PaymentMode`].
    ///
    /// Requires an EVM network (for contract price queries) but NOT a wallet.
    /// Returns a [`PreparedUpload`] containing the data map and either a
    /// [`PaymentIntent`] (wave-batch) or prepared merkle sub-batches that
    /// the external signer uses to construct and submit the on-chain payment
    /// transaction(s) — one per sub-batch (ADR-0003).
    ///
    /// `mode` mirrors the wallet path's [`Client::file_upload_with_mode`]:
    /// [`PaymentMode::Auto`] picks merkle at the chunk threshold,
    /// [`PaymentMode::Merkle`] forces merkle for ≥ 2 upload chunks (this is
    /// how tests exercise the external merkle flow with small files), and
    /// [`PaymentMode::Single`] forces wave-batch.
    ///
    /// When `visibility` is [`Visibility::Public`], the serialized `DataMap`
    /// is bundled into the payment batch as an additional chunk and its
    /// address is recorded on the returned [`PreparedUpload`]. After
    /// [`Client::finalize_upload`] (or `_merkle`) succeeds, that address is
    /// surfaced via [`FileUploadResult::data_map_address`] so the uploader
    /// can share a single address from which anyone can retrieve the file.
    ///
    /// When `progress` is `Some`, [`UploadEvent`]s are emitted on the channel
    /// during encryption ([`UploadEvent::Encrypting`] / [`UploadEvent::Encrypted`])
    /// and per-chunk quoting ([`UploadEvent::ChunkQuoted`]). Storage events are
    /// emitted later by [`Client::finalize_upload_with_progress`] /
    /// [`Client::finalize_upload_merkle_with_progress`].
    ///
    /// **Memory note:** on the merkle path, chunk bodies stay in the on-disk
    /// encryption spill inside the returned [`PreparedUpload`] and are read
    /// back ≤ store-cap at a time during finalize, so peak RAM stays bounded
    /// (~256 MB) regardless of file size (ADR-0003). The spill directory
    /// lives as long as the `PreparedUpload` does. The wave-batch path —
    /// below the merkle threshold, so < ~64 × 4 MiB of chunks (unless
    /// [`PaymentMode::Single`] forces it for a larger file) — still holds
    /// its chunk bodies resident.
    ///
    /// # Errors
    ///
    /// Returns an error if there is insufficient disk space, the file cannot
    /// be read, encryption fails, or quote collection fails.
    pub async fn file_prepare_upload_with_mode(
        &self,
        path: &Path,
        visibility: Visibility,
        mode: PaymentMode,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<PreparedUpload> {
        debug!(
            "Preparing file upload for external signing (visibility={visibility:?}, mode={mode:?}): {}",
            path.display()
        );

        let file_size = std::fs::metadata(path)?.len();
        check_disk_space_for_spill(file_size)?;

        let (mut spill, data_map) = self.encrypt_file_to_spill(path, progress.as_ref()).await?;

        info!(
            "Encrypted {} into {} chunks for external signing (spilled to disk)",
            path.display(),
            spill.len()
        );

        // For public uploads, bundle the serialized DataMap as an extra chunk
        // in the same payment batch. This lets the external signer pay for
        // the data chunks and the DataMap chunk in one flow, and lets the
        // finalize step return the DataMap's chunk address as the shareable
        // retrieval address. It joins the spill like any data chunk so the
        // merkle path stays disk-backed; `push` dedups by address.
        let data_map_address = match visibility {
            Visibility::Private => None,
            Visibility::Public => {
                let serialized = rmp_serde::to_vec(&data_map).map_err(|e| {
                    Error::Serialization(format!("Failed to serialize DataMap: {e}"))
                })?;
                let address = compute_address(&serialized);
                info!(
                    "Public upload: bundling DataMap chunk ({} bytes) at address {}",
                    serialized.len(),
                    hex::encode(address)
                );
                spill.push(&serialized)?;
                Some(address)
            }
        };

        let chunk_count = spill.len();

        if let Some(ref tx) = progress {
            let _ = tx
                .send(UploadEvent::Encrypted {
                    total_chunks: chunk_count,
                })
                .await;
        }

        let (payment_info, already_stored_addresses) = if should_use_merkle(chunk_count, mode) {
            // Merkle path: build tree(s), collect candidate pools, return for
            // external payment. Chunk bodies stay in the spill on disk.
            info!("Using merkle batch preparation for {chunk_count} file chunks");

            let chunk_entries = spill.chunk_entries()?;

            let merkle_plan = self
                .plan_merkle_upload(chunk_entries, DATA_TYPE_CHUNK, progress.as_ref())
                .await?;

            if merkle_plan.to_upload.is_empty() {
                info!("All {chunk_count} file chunks already stored; no external payment needed");
                (
                    ExternalPaymentInfo::WaveBatch {
                        prepared_chunks: Vec::new(),
                        payment_intent: PaymentIntent::from_prepared_chunks(&[]),
                    },
                    merkle_plan.already_stored,
                )
            } else if !should_use_merkle(merkle_plan.to_upload.len(), mode) {
                info!(
                    "{} file chunks need upload after merkle preflight; preparing wave-batch payment",
                    merkle_plan.to_upload.len()
                );
                let chunk_data = spill.read_chunks(&merkle_plan.to_upload)?;
                let (payment_info, mut wave_already_stored) = self
                    .prepare_wave_batch_external_chunks(chunk_data, progress.as_ref(), chunk_count)
                    .await?;
                let mut already_stored = merkle_plan.already_stored;
                already_stored.append(&mut wave_already_stored);
                (payment_info, already_stored)
            } else {
                // One signature pays one tree, so the to-upload set is
                // partitioned into `MerkleTree`-sized sub-batches and the
                // signer pays each — the external equivalent of the wallet
                // path's multi-transaction split (ADR-0003).
                match self
                    .prepare_merkle_batches_external(
                        &merkle_plan.to_upload,
                        DATA_TYPE_CHUNK,
                        merkle_plan.to_upload_avg_size(),
                        self.merkle_external_batch_cap(),
                    )
                    .await
                {
                    Ok(prepared_batches) => {
                        info!(
                            "File prepared for external merkle signing: {} chunks in {} sub-batch(es) ({})",
                            merkle_plan.to_upload.len(),
                            prepared_batches.len(),
                            path.display()
                        );

                        (
                            ExternalPaymentInfo::Merkle {
                                prepared_batches,
                                chunk_store: ExternalChunkStore::from_spill(spill),
                                chunk_addresses: merkle_plan.to_upload,
                            },
                            merkle_plan.already_stored,
                        )
                    }
                    Err(Error::InsufficientPeers(ref msg)) => {
                        info!(
                            "External merkle preparation needs more peers ({msg}); preparing wave-batch payment"
                        );
                        let chunk_data = spill.read_chunks(&merkle_plan.to_upload)?;
                        let (payment_info, mut wave_already_stored) = self
                            .prepare_wave_batch_external_chunks(
                                chunk_data,
                                progress.as_ref(),
                                chunk_count,
                            )
                            .await?;
                        let mut already_stored = merkle_plan.already_stored;
                        already_stored.append(&mut wave_already_stored);
                        (payment_info, already_stored)
                    }
                    Err(e) => return Err(e),
                }
            }
        } else {
            // Wave path: below the merkle threshold (or PaymentMode::Single),
            // chunk bodies come back resident for per-chunk quoting.
            let chunk_data = spill.read_all_chunks()?;
            self.prepare_wave_batch_external_chunks(chunk_data, progress.as_ref(), chunk_count)
                .await?
        };

        // Surface the "DataMap chunk was already on the network" case
        // so debugging "why is data_map_address set but no storage cost
        // appears for it?" doesn't require reading the source. See the
        // `data_map_address` doc comment for why this is still a valid
        // `Some(addr)` outcome.
        if let Some(addr) = data_map_address {
            let data_map_needs_payment = match &payment_info {
                ExternalPaymentInfo::WaveBatch {
                    prepared_chunks, ..
                } => prepared_chunks.iter().any(|c| c.address == addr),
                ExternalPaymentInfo::Merkle {
                    chunk_addresses, ..
                } => chunk_addresses.contains(&addr),
            };
            if !data_map_needs_payment {
                info!(
                    "Public upload: DataMap chunk {} was already stored \
                     on the network — address is retrievable without a \
                     new payment",
                    hex::encode(addr)
                );
            }
        }

        Ok(PreparedUpload {
            data_map,
            payment_info,
            data_map_address,
            already_stored_addresses,
            total_chunks: chunk_count,
        })
    }

    async fn prepare_wave_batch_external_chunks(
        &self,
        chunk_data: Vec<Bytes>,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        progress_total: usize,
    ) -> Result<(ExternalPaymentInfo, Vec<[u8; 32]>)> {
        let chunk_count = chunk_data.len();
        let chunks_with_addr: Vec<(Bytes, [u8; 32])> = chunk_data
            .into_iter()
            .map(|content| {
                let address = compute_address(&content);
                (content, address)
            })
            .collect();

        // Wave-batch path: collect quotes per chunk concurrently, emitting
        // a `ChunkQuoted` event after each completion so callers can drive
        // a progress bar through the slow quote phase.
        let quote_limiter = self.controller().quote.clone();
        let quote_concurrency = quote_limiter.current().min(chunk_count.max(1));
        let mut quote_stream = stream::iter(chunks_with_addr)
            .map(|(content, address)| {
                let limiter = quote_limiter.clone();
                async move {
                    let result = observe_op(
                        &limiter,
                        || async move { self.prepare_chunk_payment(content).await },
                        classify_error,
                    )
                    .await;
                    (address, result)
                }
            })
            .buffer_unordered(quote_concurrency);

        let mut prepared_chunks = Vec::with_capacity(chunk_count);
        let mut already_stored = Vec::new();
        let mut quoted = 0usize;
        while let Some((address, result)) = quote_stream.next().await {
            match result? {
                Some(prepared) => prepared_chunks.push(prepared),
                None => already_stored.push(address),
            }
            quoted += 1;
            if let Some(tx) = progress {
                let _ = tx.try_send(UploadEvent::ChunkQuoted {
                    quoted,
                    total: progress_total,
                });
            }
        }

        let payment_intent = PaymentIntent::from_prepared_chunks(&prepared_chunks);
        info!(
            "Prepared external wave-batch payment: {} chunks, {} already stored, total {} atto",
            prepared_chunks.len(),
            already_stored.len(),
            payment_intent.total_amount,
        );

        Ok((
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent,
            },
            already_stored,
        ))
    }

    /// Phase 2 of external-signer upload (wave-batch): finalize with externally-signed tx hashes.
    ///
    /// Takes a [`PreparedUpload`] that used wave-batch payment and a map
    /// of `quote_hash -> tx_hash` provided by the external signer after on-chain
    /// payment. Builds payment proofs and stores chunks on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if the prepared upload used merkle payment (use
    /// [`Client::finalize_upload_merkle`] instead), proof construction fails,
    /// or any chunk cannot be stored.
    pub async fn finalize_upload(
        &self,
        prepared: PreparedUpload,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
    ) -> Result<FileUploadResult> {
        self.finalize_upload_with_progress(prepared, tx_hash_map, None)
            .await
    }

    /// Phase 2 of external-signer upload (wave-batch) with progress events.
    ///
    /// Same as [`Client::finalize_upload`] but emits [`UploadEvent::ChunkStored`]
    /// on the provided channel as each chunk is successfully stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_upload`].
    pub async fn finalize_upload_with_progress(
        &self,
        prepared: PreparedUpload,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        let data_map_address = prepared.data_map_address;
        let already_stored_addresses = prepared.already_stored_addresses;
        let already_stored_count = already_stored_addresses.len();
        let total_chunks = prepared.total_chunks;
        match prepared.payment_info {
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent,
            } => {
                let paid_chunks = finalize_batch_payment(prepared_chunks, tx_hash_map)?;
                let wave_result = self
                    .store_paid_chunks_with_events(
                        paid_chunks,
                        progress.as_ref(),
                        already_stored_count,
                        total_chunks,
                    )
                    .await;
                if !wave_result.failed.is_empty() {
                    let failed_count = wave_result.failed.len();
                    let stored_count = already_stored_count + wave_result.stored.len();
                    let mut stored = already_stored_addresses;
                    stored.extend(wave_result.stored);
                    return Err(Error::PartialUpload {
                        stored,
                        stored_count,
                        failed: wave_result.failed,
                        failed_count,
                        total_chunks,
                        // Report the storage spend known from the payment intent
                        // the external signer was handed. Gas is paid by the
                        // signer out-of-band, so it stays unknown (0).
                        spend: Box::new(PartialUploadSpend {
                            storage_cost_atto: payment_intent.total_amount.to_string(),
                            gas_cost_wei: 0,
                        }),
                        reason: "finalize_upload: chunk storage failed after retries".into(),
                    });
                }
                let chunks_stored = already_stored_count + wave_result.stored.len();

                info!("External-signer upload finalized: {chunks_stored} chunks stored");

                let mut stats = WaveAggregateStats::default();
                stats.absorb(&wave_result);

                Ok(FileUploadResult {
                    data_map: prepared.data_map,
                    chunks_stored,
                    chunks_failed: 0,
                    total_chunks,
                    payment_mode_used: PaymentMode::Single,
                    // Storage spend is known from the payment intent; gas is
                    // paid by the external signer out-of-band (unknown here).
                    storage_cost_atto: payment_intent.total_amount.to_string(),
                    gas_cost_wei: 0,
                    data_map_address,
                    chunk_attempts_total: stats.chunk_attempts_total,
                    store_durations_ms: stats.store_durations_ms,
                    retries_histogram: stats.retries_histogram,
                })
            }
            ExternalPaymentInfo::Merkle { .. } => Err(Error::Payment(
                "Cannot finalize merkle upload with wave-batch tx hashes. \
                 Use finalize_upload_merkle() instead."
                    .to_string(),
            )),
        }
    }

    /// Per-batch leaf cap for external merkle preparation: the configured
    /// test override clamped to `3..=MAX_LEAVES` (see
    /// [`merkle_batch_sizes_with_cap`] for why 3 is the floor), or
    /// `MAX_LEAVES` (ADR-0003).
    fn merkle_external_batch_cap(&self) -> usize {
        self.config()
            .merkle_external_batch_cap
            .map_or(MAX_LEAVES, |cap| cap.clamp(3, MAX_LEAVES))
    }

    /// Phase 2 of external-signer upload (merkle): finalize with winner pool hash.
    ///
    /// The single-batch special case of
    /// [`Client::finalize_upload_merkle_multi`]: valid only for uploads that
    /// prepared as exactly one merkle sub-batch (any fresh upload below
    /// `MAX_LEAVES` chunks). Generates proofs and stores chunks on the
    /// network.
    ///
    /// # Errors
    ///
    /// Returns an error if the prepared upload used wave-batch payment (use
    /// [`Client::finalize_upload`] instead), was prepared as more than one
    /// sub-batch (use [`Client::finalize_upload_merkle_multi`]), or proof
    /// generation fails. Chunks still short of quorum after all retries
    /// surface as [`Error::PartialUpload`] carrying the stored and failed
    /// addresses — the same contract as [`Client::finalize_upload`].
    /// Re-preparing the same file skips chunks that are already stored.
    pub async fn finalize_upload_merkle(
        &self,
        prepared: PreparedUpload,
        winner_pool_hash: [u8; 32],
    ) -> Result<FileUploadResult> {
        self.finalize_upload_merkle_with_progress(prepared, winner_pool_hash, None)
            .await
    }

    /// Phase 2 of external-signer upload (merkle) with progress events.
    ///
    /// Same as [`Client::finalize_upload_merkle`] but emits [`UploadEvent::ChunkStored`]
    /// on the provided channel as each chunk is successfully stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_upload_merkle`].
    pub async fn finalize_upload_merkle_with_progress(
        &self,
        prepared: PreparedUpload,
        winner_pool_hash: [u8; 32],
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        if let ExternalPaymentInfo::Merkle {
            prepared_batches, ..
        } = &prepared.payment_info
        {
            let batches = prepared_batches.len();
            if batches != 1 {
                return Err(Error::Payment(format!(
                    "This upload was prepared as {batches} merkle sub-batches; \
                     pay each and call finalize_upload_merkle_multi() with one \
                     winner hash per batch."
                )));
            }
        }
        self.finalize_upload_merkle_multi_with_progress(
            prepared,
            vec![Some(winner_pool_hash)],
            progress,
        )
        .await
    }

    /// Phase 2 of external-signer upload (merkle): finalize with one winner
    /// pool hash per prepared sub-batch.
    ///
    /// `winner_pool_hashes` aligns with
    /// [`ExternalPaymentInfo::Merkle::prepared_batches`]: entry `i` is the
    /// `MerklePaymentMade` winner hash of batch `i`'s on-chain payment, or
    /// `None` if the signer never paid that batch (e.g. the user abandoned
    /// the flow midway). Paid batches make forward progress: their proofs
    /// are folded — mirroring the wallet path's multi-batch fold — and their
    /// chunks stored from the on-disk spill in a bounded fan-out; chunks of
    /// unpaid batches are reported through [`Error::PartialUpload`]
    /// (ADR-0003).
    ///
    /// # Errors
    ///
    /// Returns an error if the prepared upload used wave-batch payment, the
    /// hash count does not match the batch count, every entry is `None`, or
    /// proof generation fails. Chunks short of quorum after all retries —
    /// and all chunks of unpaid batches — surface as
    /// [`Error::PartialUpload`] carrying the stored and failed addresses.
    /// Re-preparing the same file skips chunks that are already stored.
    pub async fn finalize_upload_merkle_multi(
        &self,
        prepared: PreparedUpload,
        winner_pool_hashes: Vec<Option<[u8; 32]>>,
    ) -> Result<FileUploadResult> {
        self.finalize_upload_merkle_multi_with_progress(prepared, winner_pool_hashes, None)
            .await
    }

    /// Same as [`Client::finalize_upload_merkle_multi`] but emits
    /// [`UploadEvent::ChunkStored`] on the provided channel as each chunk is
    /// successfully stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_upload_merkle_multi`].
    pub async fn finalize_upload_merkle_multi_with_progress(
        &self,
        prepared: PreparedUpload,
        winner_pool_hashes: Vec<Option<[u8; 32]>>,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        let data_map_address = prepared.data_map_address;
        let already_stored_addresses = prepared.already_stored_addresses;
        let total_chunks = prepared.total_chunks;
        match prepared.payment_info {
            ExternalPaymentInfo::Merkle {
                prepared_batches,
                chunk_store,
                chunk_addresses,
            } => {
                let batch_result =
                    fold_external_merkle_payments(prepared_batches, winner_pool_hashes)?;

                let (chunks_stored, _storage_cost, _gas_cost, stats) = self
                    .upload_merkle_from_spill(
                        chunk_store.spill(),
                        &chunk_addresses,
                        &batch_result,
                        &already_stored_addresses,
                        progress.as_ref(),
                    )
                    .await?;

                info!("External-signer merkle upload finalized: {chunks_stored} chunks stored");

                Ok(FileUploadResult {
                    data_map: prepared.data_map,
                    chunks_stored,
                    chunks_failed: 0,
                    total_chunks,
                    payment_mode_used: PaymentMode::Merkle,
                    // The external signer pays on-chain out-of-band, so the
                    // spend is unknown to the library here.
                    storage_cost_atto: "0".into(),
                    gas_cost_wei: 0,
                    data_map_address,
                    chunk_attempts_total: stats.chunk_attempts_total,
                    store_durations_ms: stats.store_durations_ms,
                    retries_histogram: stats.retries_histogram,
                })
            }
            ExternalPaymentInfo::WaveBatch { .. } => Err(Error::Payment(
                "Cannot finalize wave-batch upload with merkle winner hashes. \
                 Use finalize_upload() instead."
                    .to_string(),
            )),
        }
    }

    /// Finalize an external-signer merkle upload, returning a resume handle if
    /// some chunks remain unstored after retries.
    ///
    /// Behaves like [`Client::finalize_upload_merkle_multi`], but instead of
    /// surfacing a quorum shortfall as [`Error::PartialUpload`] it returns
    /// [`FinalizeOutcome::Partial`], carrying a [`MerkleFinalizeResume`] (inside
    /// [`FinalizeResume::Merkle`]) that owns the on-disk chunk spill and the
    /// already-signed payment proofs. The caller can hand that handle to
    /// [`Client::finalize_resume`] to store only the still-unstored chunks
    /// against the **same** on-chain payment — no re-quoting, no second
    /// signature, no double payment (#140).
    ///
    /// Unlike the non-resumable method, **every sub-batch must be paid**
    /// (`winner_pool_hashes` all `Some`). A resume handle cannot acquire proofs
    /// for unpaid chunks, so a partially-paid finalize could never drain to
    /// [`FinalizeOutcome::Complete`]; partial payment is rejected up front. To
    /// finalize a partial payment, use [`Client::finalize_upload_merkle_multi`],
    /// which reports the unpaid chunks through [`Error::PartialUpload`].
    ///
    /// # Errors
    ///
    /// Returns an error if any sub-batch is unpaid, the winner-hash count does
    /// not match the prepared batches, the payment info is wave-batch rather
    /// than merkle, or payment finalization fails. Store failures are **not**
    /// errors here: a quorum shortfall — and a fatal store abort, which keeps
    /// its progress the same way — comes back as [`FinalizeOutcome::Partial`].
    pub async fn finalize_upload_merkle_multi_resumable(
        &self,
        prepared: PreparedUpload,
        winner_pool_hashes: Vec<Option<[u8; 32]>>,
    ) -> Result<FinalizeOutcome> {
        self.finalize_upload_merkle_multi_resumable_with_progress(
            prepared,
            winner_pool_hashes,
            None,
        )
        .await
    }

    /// Same as [`Client::finalize_upload_merkle_multi_resumable`] but emits
    /// [`UploadEvent::ChunkStored`] on the provided channel as each chunk is
    /// stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_upload_merkle_multi_resumable`].
    pub async fn finalize_upload_merkle_multi_resumable_with_progress(
        &self,
        prepared: PreparedUpload,
        winner_pool_hashes: Vec<Option<[u8; 32]>>,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FinalizeOutcome> {
        let data_map_address = prepared.data_map_address;
        let already_stored_addresses = prepared.already_stored_addresses;
        let total_chunks = prepared.total_chunks;
        let data_map = prepared.data_map;
        match prepared.payment_info {
            ExternalPaymentInfo::Merkle {
                prepared_batches,
                chunk_store,
                chunk_addresses,
            } => {
                require_fully_paid_for_resumable(&winner_pool_hashes)?;
                let batch_result =
                    fold_external_merkle_payments(prepared_batches, winner_pool_hashes)?;
                self.drive_merkle_finalize(
                    data_map,
                    data_map_address,
                    total_chunks,
                    chunk_store,
                    chunk_addresses,
                    batch_result,
                    already_stored_addresses,
                    progress.as_ref(),
                )
                .await
            }
            ExternalPaymentInfo::WaveBatch { .. } => Err(Error::Payment(
                "Cannot finalize wave-batch upload with merkle winner hashes. \
                 Use finalize_upload_resumable() instead."
                    .to_string(),
            )),
        }
    }

    /// Finalize an external-signer wave-batch upload, returning a resume handle
    /// if some chunks remain unstored after retries.
    ///
    /// Behaves like [`Client::finalize_upload`], but instead of surfacing a
    /// storage failure as [`Error::PartialUpload`] it returns
    /// [`FinalizeOutcome::Partial`], carrying a [`WaveFinalizeResume`] (inside
    /// [`FinalizeResume::Wave`]) that owns the already-paid chunks still needing
    /// storage. The caller can hand that handle to [`Client::finalize_resume`]
    /// to re-store only those chunks against the **same** on-chain payment — no
    /// re-quoting, no second signature, no double payment (#140).
    ///
    /// # Errors
    ///
    /// Returns an error if a `tx_hash` is missing for a quote, the payment info
    /// is merkle rather than wave-batch, or payment finalization fails. A plain
    /// storage shortfall is **not** an error — it comes back as
    /// [`FinalizeOutcome::Partial`].
    pub async fn finalize_upload_resumable(
        &self,
        prepared: PreparedUpload,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
    ) -> Result<FinalizeOutcome> {
        self.finalize_upload_resumable_with_progress(prepared, tx_hash_map, None)
            .await
    }

    /// Same as [`Client::finalize_upload_resumable`] but emits
    /// [`UploadEvent::ChunkStored`] on the provided channel as each chunk is
    /// stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_upload_resumable`].
    pub async fn finalize_upload_resumable_with_progress(
        &self,
        prepared: PreparedUpload,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FinalizeOutcome> {
        let data_map_address = prepared.data_map_address;
        let already_stored_count = prepared.already_stored_addresses.len();
        let total_chunks = prepared.total_chunks;
        let data_map = prepared.data_map;
        match prepared.payment_info {
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent,
            } => {
                let paid_chunks = finalize_batch_payment(prepared_chunks, tx_hash_map)?;
                let storage_cost_atto = payment_intent.total_amount.to_string();
                Ok(self
                    .drive_wave_finalize(
                        data_map,
                        data_map_address,
                        total_chunks,
                        already_stored_count,
                        paid_chunks,
                        storage_cost_atto,
                        progress.as_ref(),
                    )
                    .await)
            }
            ExternalPaymentInfo::Merkle { .. } => Err(Error::Payment(
                "Cannot finalize merkle upload with wave-batch tx hashes. \
                 Use finalize_upload_merkle_multi_resumable() instead."
                    .to_string(),
            )),
        }
    }

    /// Resume an external-signer finalize that returned
    /// [`FinalizeOutcome::Partial`], storing only the still-unstored chunks
    /// against the already-signed payment carried by the [`FinalizeResume`]
    /// handle.
    ///
    /// No re-quoting and no new signature: the handle owns the retained chunk
    /// bodies (wave path) or the spill + merkle proofs (merkle path). Every
    /// chunk in the handle has its payment material, so the upload always
    /// *can* complete once the network cooperates. Safe to call repeatedly —
    /// each call stores what it can and either completes the upload
    /// ([`FinalizeOutcome::Complete`]) or hands back the remainder, so a
    /// caller can loop until it drains or gives up (#140).
    ///
    /// **Bound that loop.** Store failures — including persistent ones, such
    /// as a chunk whose close group stays unreachable — surface as
    /// [`FinalizeOutcome::Partial`] on every call, never as `Err`, so an
    /// unbounded `while let Partial` loop will spin for as long as the
    /// failure persists. Cap the attempts (or apply backoff between them) and
    /// treat a handle that stops shrinking as stuck.
    ///
    /// # Errors
    ///
    /// Store failures are not errors — every store-side outcome, fatal aborts
    /// included, comes back as [`FinalizeOutcome::Partial`] with the payment
    /// material retained for retry. `Err` is reserved for failures outside
    /// the chunk store itself.
    pub async fn finalize_resume(&self, resume: FinalizeResume) -> Result<FinalizeOutcome> {
        self.finalize_resume_with_progress(resume, None).await
    }

    /// Same as [`Client::finalize_resume`] but emits [`UploadEvent::ChunkStored`]
    /// as each remaining chunk is stored.
    ///
    /// # Errors
    ///
    /// Same as [`Client::finalize_resume`].
    pub async fn finalize_resume_with_progress(
        &self,
        resume: FinalizeResume,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FinalizeOutcome> {
        match resume {
            FinalizeResume::Wave(w) => {
                let WaveFinalizeResume {
                    data_map,
                    data_map_address,
                    total_chunks,
                    stored_count,
                    failed_paid_chunks,
                    storage_cost_atto,
                } = *w;
                Ok(self
                    .drive_wave_finalize(
                        data_map,
                        data_map_address,
                        total_chunks,
                        stored_count,
                        failed_paid_chunks,
                        storage_cost_atto,
                        progress.as_ref(),
                    )
                    .await)
            }
            FinalizeResume::Merkle(m) => {
                let MerkleFinalizeResume {
                    data_map,
                    data_map_address,
                    total_chunks,
                    chunk_store,
                    unstored_addresses,
                    batch_result,
                    stored_addresses,
                } = *m;
                self.drive_merkle_finalize(
                    data_map,
                    data_map_address,
                    total_chunks,
                    chunk_store,
                    unstored_addresses,
                    batch_result,
                    stored_addresses,
                    progress.as_ref(),
                )
                .await
            }
        }
    }

    /// Drive one merkle store pass over `to_store` (reading bodies from the
    /// spill on demand and re-attaching proofs from `batch_result`), shared by
    /// the initial resumable finalize and [`Client::finalize_resume`].
    ///
    /// On a quorum shortfall — or a fatal store abort, which
    /// `upload_merkle_from_spill` folds into [`Error::PartialUpload`] with its
    /// progress preserved — it captures the retained spill, proofs, and the
    /// cumulative stored/unstored sets into a [`MerkleFinalizeResume`] and
    /// returns [`FinalizeOutcome::Partial`], so the same on-chain payment can
    /// be retried without re-signing. `Err` is reserved for failures outside
    /// the store fan-out (e.g. invalid payment material).
    #[allow(clippy::too_many_arguments)]
    async fn drive_merkle_finalize(
        &self,
        data_map: DataMap,
        data_map_address: Option<[u8; 32]>,
        total_chunks: usize,
        chunk_store: ExternalChunkStore,
        to_store: Vec<[u8; 32]>,
        batch_result: MerkleBatchPaymentResult,
        stored_addresses: Vec<[u8; 32]>,
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> Result<FinalizeOutcome> {
        let store_result = self
            .upload_merkle_from_spill(
                chunk_store.spill(),
                &to_store,
                &batch_result,
                &stored_addresses,
                progress,
            )
            .await;
        assemble_merkle_finalize_outcome(
            store_result,
            data_map,
            data_map_address,
            total_chunks,
            chunk_store,
            batch_result,
        )
    }

    /// Drive one wave-batch store pass over `paid_chunks`, shared by the initial
    /// resumable finalize and [`Client::finalize_resume`].
    ///
    /// Retains each paid chunk (cheaply — bodies are ref-counted `Bytes`) so a
    /// storage shortfall can hand the failed subset back in a
    /// [`WaveFinalizeResume`] ([`FinalizeOutcome::Partial`]) for re-store against
    /// the same payment, instead of the shortfall being dropped. The store never
    /// errors on a partial, so this is infallible.
    #[allow(clippy::too_many_arguments)]
    async fn drive_wave_finalize(
        &self,
        data_map: DataMap,
        data_map_address: Option<[u8; 32]>,
        total_chunks: usize,
        already_stored_count: usize,
        paid_chunks: Vec<PaidChunk>,
        storage_cost_atto: String,
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> FinalizeOutcome {
        // Retain address -> paid chunk so the failed subset can be re-stored on
        // resume; cloning is cheap since the chunk body is a ref-counted `Bytes`.
        let retained: HashMap<[u8; 32], PaidChunk> =
            paid_chunks.iter().map(|c| (c.address, c.clone())).collect();
        let wave_result = self
            .store_paid_chunks_with_events(
                paid_chunks,
                progress,
                already_stored_count,
                total_chunks,
            )
            .await;
        assemble_wave_finalize_outcome(
            wave_result,
            retained,
            data_map,
            data_map_address,
            total_chunks,
            already_stored_count,
            storage_cost_atto,
        )
    }

    /// Upload a file with a specific payment mode.
    ///
    /// Before encryption, checks that the temp directory has enough free
    /// disk space for the spilled chunks (~1.1× source file size).
    ///
    /// Encrypted chunks are spilled to a temp directory during encryption
    /// so that only their 32-byte addresses stay in memory. At upload time,
    /// chunks are read back one wave at a time (~64 × 4 MB ≈ 256 MB peak).
    ///
    /// # Errors
    ///
    /// Returns an error if there is insufficient disk space, the file cannot
    /// be read, encryption fails, or any chunk cannot be stored.
    #[allow(clippy::too_many_lines)]
    pub async fn file_upload_with_mode(
        &self,
        path: &Path,
        mode: PaymentMode,
    ) -> Result<FileUploadResult> {
        self.file_upload_with_progress(path, mode, None).await
    }

    /// Upload a file publicly, storing the serialized [`DataMap`] as part of
    /// the same upload payment batch.
    ///
    /// The returned [`FileUploadResult::data_map_address`] can be shared for
    /// public downloads via [`Client::data_map_fetch`].
    #[allow(clippy::too_many_lines)]
    pub async fn file_upload_public_with_mode(
        &self,
        path: &Path,
        mode: PaymentMode,
    ) -> Result<FileUploadResult> {
        self.file_upload_with_visibility_and_progress(path, mode, Visibility::Public, None)
            .await
    }

    /// Upload a file with progress events sent to the given channel.
    ///
    /// Same as [`Client::file_upload_with_mode`] but sends [`UploadEvent`]s to the
    /// provided channel for UI progress feedback.
    #[allow(clippy::too_many_lines)]
    pub async fn file_upload_with_progress(
        &self,
        path: &Path,
        mode: PaymentMode,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        self.file_upload_with_visibility_and_progress(path, mode, Visibility::Private, progress)
            .await
    }

    /// Public file upload with progress events.
    ///
    /// Same as [`Client::file_upload_public_with_mode`] but sends
    /// [`UploadEvent`]s to the provided channel for UI progress feedback.
    #[allow(clippy::too_many_lines)]
    pub async fn file_upload_public_with_progress(
        &self,
        path: &Path,
        mode: PaymentMode,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        self.file_upload_with_visibility_and_progress(path, mode, Visibility::Public, progress)
            .await
    }

    #[allow(clippy::too_many_lines)]
    async fn file_upload_with_visibility_and_progress(
        &self,
        path: &Path,
        mode: PaymentMode,
        visibility: Visibility,
        progress: Option<mpsc::Sender<UploadEvent>>,
    ) -> Result<FileUploadResult> {
        debug!(
            "Streaming file upload with mode {mode:?}, visibility {visibility:?}: {}",
            path.display()
        );

        // Pre-flight: verify enough temp disk space for the chunk spill.
        let file_size = std::fs::metadata(path)?.len();
        check_disk_space_for_spill(file_size)?;

        // Phase 1: Encrypt file and spill chunks to temp directory.
        // Only 32-byte addresses stay in memory — chunk data lives on disk.
        let (mut spill, data_map) = self.encrypt_file_to_spill(path, progress.as_ref()).await?;

        let data_map_address = match visibility {
            Visibility::Private => None,
            Visibility::Public => {
                let serialized = rmp_serde::to_vec(&data_map).map_err(|e| {
                    Error::Serialization(format!("Failed to serialize DataMap: {e}"))
                })?;
                let address = compute_address(&serialized);
                info!(
                    "Public upload: adding DataMap chunk ({} bytes) at address {} to payment batch",
                    serialized.len(),
                    hex::encode(address)
                );
                spill.push(&serialized)?;
                Some(address)
            }
        };

        let chunk_count = spill.len();
        info!(
            "Encrypted {} into {chunk_count} chunks (spilled to disk)",
            path.display()
        );
        if let Some(ref tx) = progress {
            let _ = tx
                .send(UploadEvent::Encrypted {
                    total_chunks: chunk_count,
                })
                .await;
        }

        // Phase 2: Decide payment mode and upload in waves from disk.
        //
        // For the merkle path, attempt to resume from a cached
        // receipt before paying again. The cache is keyed by the
        // CANONICAL source path so `./foo`, `/abs/foo`, and any
        // symlink alias all resolve to the same cache entry — a
        // crash-and-retry from a different cwd or via a different
        // alias still hits the receipt. Canonicalize may fail (the
        // file could have been moved between phase 1 and here); we
        // fall back to the display string in that case, which
        // preserves pre-fix behaviour rather than dropping cache
        // resume entirely.
        let file_path_key = std::fs::canonicalize(path)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| path.display().to_string());
        let (chunks_stored, actual_mode, storage_cost_atto, gas_cost_wei, stats) = if self
            .should_use_merkle(chunk_count, mode)
        {
            info!("Using merkle batch payment for {chunk_count} file chunks");

            let cached_merkle =
                crate::data::client::cached_merkle::try_load_for_file(&file_path_key)
                    .map(|(_cache_path, cached)| cached);

            let merkle_plan = match self
                .plan_merkle_upload(spill.chunk_entries()?, DATA_TYPE_CHUNK, progress.as_ref())
                .await
            {
                Ok(plan) => plan,
                Err(e) => {
                    if let Some(cached) = cached_merkle
                        .as_ref()
                        .filter(|cached| cached_merkle_covers_addresses(cached, &spill.addresses))
                    {
                        info!(
                            "Merkle preflight failed ({e}); \
                             resuming with cached merkle proofs"
                        );
                        let (stored, sc, gc, stats) = self
                            .upload_merkle_from_spill(
                                &spill,
                                &spill.addresses,
                                cached,
                                &[],
                                progress.as_ref(),
                            )
                            .await?;
                        crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                        return Ok(FileUploadResult {
                            data_map,
                            chunks_stored: stored,
                            chunks_failed: 0,
                            total_chunks: chunk_count,
                            payment_mode_used: PaymentMode::Merkle,
                            storage_cost_atto: sc,
                            gas_cost_wei: gc,
                            data_map_address,
                            chunk_attempts_total: stats.chunk_attempts_total,
                            store_durations_ms: stats.store_durations_ms,
                            retries_histogram: stats.retries_histogram,
                        });
                    }
                    match &e {
                        Error::InsufficientPeers(msg) if mode == PaymentMode::Auto => {
                            info!(
                                "Merkle preflight needs more peers ({msg}), \
                                 falling back to wave-batch"
                            );
                            let (stored, sc, gc, fb_stats) = self
                                .upload_waves_single(
                                    &spill,
                                    progress.as_ref(),
                                    Some(&file_path_key),
                                )
                                .await?;
                            crate::data::client::cached_single::try_delete_for_file(&file_path_key);
                            return Ok(FileUploadResult {
                                data_map,
                                chunks_stored: stored,
                                chunks_failed: 0,
                                total_chunks: chunk_count,
                                payment_mode_used: PaymentMode::Single,
                                storage_cost_atto: sc,
                                gas_cost_wei: gc,
                                data_map_address,
                                chunk_attempts_total: fb_stats.chunk_attempts_total,
                                store_durations_ms: fb_stats.store_durations_ms,
                                retries_histogram: fb_stats.retries_histogram,
                            });
                        }
                        _ => return Err(e),
                    }
                }
            };

            if merkle_plan.to_upload.is_empty() {
                info!("All {chunk_count} merkle chunks already stored; skipping payment");
                crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                crate::data::client::cached_single::try_delete_for_file(&file_path_key);
                (
                    chunk_count,
                    PaymentMode::Merkle,
                    "0".to_string(),
                    0,
                    WaveAggregateStats::default(),
                )
            } else if !self.should_use_merkle(merkle_plan.to_upload.len(), mode) {
                let remaining_chunks = merkle_plan.to_upload.len();
                if let Some(cached) = cached_merkle
                    .as_ref()
                    .filter(|cached| cached_merkle_covers_addresses(cached, &merkle_plan.to_upload))
                {
                    info!(
                        "{remaining_chunks} chunks remain below merkle threshold; \
                         reusing cached merkle proofs"
                    );
                    let (stored, sc, gc, stats) = self
                        .upload_merkle_from_spill(
                            &spill,
                            &merkle_plan.to_upload,
                            cached,
                            &merkle_plan.already_stored,
                            progress.as_ref(),
                        )
                        .await?;
                    crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                    (stored, PaymentMode::Merkle, sc, gc, stats)
                } else {
                    if cached_merkle.is_some() {
                        info!(
                            "{remaining_chunks} chunks remain below merkle threshold, \
                             and the cached merkle receipt does not cover them. \
                             Discarding cache and using single-node payment."
                        );
                        crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                    } else {
                        info!(
                            "{remaining_chunks} chunks need upload after merkle preflight; \
                             using single-node payment"
                        );
                    }
                    let (stored, sc, gc, stats) = self
                        .upload_spill_addresses_single(
                            &spill,
                            &merkle_plan.to_upload,
                            progress.as_ref(),
                            &merkle_plan.already_stored,
                            chunk_count,
                            Some(&file_path_key),
                        )
                        .await?;
                    crate::data::client::cached_single::try_delete_for_file(&file_path_key);
                    (stored, PaymentMode::Single, sc, gc, stats)
                }
            } else {
                let batch_result = if let Some(cached) = cached_merkle.as_ref() {
                    // Validate the cache against the chunks that still need
                    // storage. Extra proofs are harmless: a previous attempt
                    // may have paid for chunks that are now already stored.
                    if cached_merkle_covers_addresses(cached, &merkle_plan.to_upload) {
                        info!(
                            "Skipping merkle payment phase; resuming with \
                             cached proofs for {} remaining chunks",
                            merkle_plan.to_upload.len()
                        );
                        Ok(cached.clone())
                    } else {
                        info!(
                            "Cached merkle receipt does not cover the current \
                             remaining chunks (cached={}, remaining={}). \
                             Discarding cache and paying fresh.",
                            cached.proofs.len(),
                            merkle_plan.to_upload.len()
                        );
                        crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                        self.pay_for_merkle_batch(
                            &merkle_plan.to_upload,
                            DATA_TYPE_CHUNK,
                            merkle_plan.to_upload_avg_size(),
                        )
                        .await
                        .inspect(|result| {
                            crate::data::client::cached_merkle::try_save(&file_path_key, result);
                        })
                    }
                } else {
                    self.pay_for_merkle_batch(
                        &merkle_plan.to_upload,
                        DATA_TYPE_CHUNK,
                        merkle_plan.to_upload_avg_size(),
                    )
                    .await
                    .inspect(|result| {
                        // Save BEFORE the store phase so a crash
                        // mid-upload leaves a resumable receipt.
                        crate::data::client::cached_merkle::try_save(&file_path_key, result);
                    })
                };

                let batch_result = match batch_result {
                    Ok(result) => result,
                    Err(Error::InsufficientPeers(ref msg)) if mode == PaymentMode::Auto => {
                        info!("Merkle needs more peers ({msg}), falling back to wave-batch");
                        let (stored, sc, gc, fb_stats) = self
                            .upload_spill_addresses_single(
                                &spill,
                                &merkle_plan.to_upload,
                                progress.as_ref(),
                                &merkle_plan.already_stored,
                                chunk_count,
                                Some(&file_path_key),
                            )
                            .await?;
                        crate::data::client::cached_single::try_delete_for_file(&file_path_key);
                        return Ok(FileUploadResult {
                            data_map,
                            chunks_stored: stored,
                            chunks_failed: 0,
                            total_chunks: chunk_count,
                            payment_mode_used: PaymentMode::Single,
                            storage_cost_atto: sc,
                            gas_cost_wei: gc,
                            data_map_address,
                            chunk_attempts_total: fb_stats.chunk_attempts_total,
                            store_durations_ms: fb_stats.store_durations_ms,
                            retries_histogram: fb_stats.retries_histogram,
                        });
                    }
                    Err(e) => return Err(e),
                };

                let (stored, sc, gc, stats) = self
                    .upload_merkle_from_spill(
                        &spill,
                        &merkle_plan.to_upload,
                        &batch_result,
                        &merkle_plan.already_stored,
                        progress.as_ref(),
                    )
                    .await?;
                // Upload succeeded end-to-end; the cached receipt is
                // no longer needed.
                crate::data::client::cached_merkle::try_delete_for_file(&file_path_key);
                (stored, PaymentMode::Merkle, sc, gc, stats)
            }
        } else {
            let (stored, sc, gc, stats) = self
                .upload_waves_single(&spill, progress.as_ref(), Some(&file_path_key))
                .await?;
            // Full file success: drop any cached single-node receipt.
            crate::data::client::cached_single::try_delete_for_file(&file_path_key);
            (stored, PaymentMode::Single, sc, gc, stats)
        };

        info!(
            "File uploaded with {actual_mode:?}: {chunks_stored} chunks stored ({})",
            path.display()
        );

        Ok(FileUploadResult {
            data_map,
            chunks_stored,
            chunks_failed: 0,
            total_chunks: chunk_count,
            payment_mode_used: actual_mode,
            storage_cost_atto,
            gas_cost_wei,
            data_map_address,
            chunk_attempts_total: stats.chunk_attempts_total,
            store_durations_ms: stats.store_durations_ms,
            retries_histogram: stats.retries_histogram,
        })
    }

    /// Encrypt a file and spill chunks to a temp directory.
    ///
    /// Logs progress every 100 chunks so users get feedback during
    /// multi-GB encryptions.
    ///
    /// Returns the spill buffer (addresses on disk) and the `DataMap`.
    async fn encrypt_file_to_spill(
        &self,
        path: &Path,
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> Result<(ChunkSpill, DataMap)> {
        let (mut chunk_rx, datamap_rx, handle) = spawn_file_encryption(path.to_path_buf())?;

        let mut spill = ChunkSpill::new()?;
        while let Some(content) = chunk_rx.recv().await {
            spill.push(&content)?;
            let chunks_done = spill.len();
            if let Some(tx) = progress {
                if chunks_done.is_multiple_of(10) {
                    let _ = tx.send(UploadEvent::Encrypting { chunks_done }).await;
                }
            }
            if chunks_done % 100 == 0 {
                let mb = spill.total_bytes() / (1024 * 1024);
                info!(
                    "Encryption progress: {chunks_done} chunks spilled ({mb} MB) — {}",
                    path.display()
                );
            }
        }

        // Await encryption completion to catch errors before paying.
        handle
            .await
            .map_err(|e| Error::Encryption(format!("encryption task panicked: {e}")))?
            .map_err(|e| Error::Encryption(format!("encryption failed: {e}")))?;

        let data_map = datamap_rx
            .await
            .map_err(|_| Error::Encryption("no DataMap from encryption thread".to_string()))?;

        Ok((spill, data_map))
    }

    /// Upload chunks from a spill using wave-based per-chunk (single) payments.
    ///
    /// Reads one wave at a time from disk, prepares quotes, pays, and stores.
    /// Peak memory: ~`UPLOAD_WAVE_SIZE × MAX_CHUNK_SIZE` (~256 MB).
    ///
    /// Returns `(chunks_stored, storage_cost_atto, gas_cost_wei)`.
    async fn upload_waves_single(
        &self,
        spill: &ChunkSpill,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        resume_key: Option<&str>,
    ) -> Result<(usize, String, u128, WaveAggregateStats)> {
        self.upload_spill_addresses_single(
            spill,
            &spill.addresses,
            progress,
            &[],
            spill.len(),
            resume_key,
        )
        .await
    }

    async fn upload_spill_addresses_single(
        &self,
        spill: &ChunkSpill,
        addresses: &[[u8; 32]],
        progress: Option<&mpsc::Sender<UploadEvent>>,
        already_stored_addresses: &[[u8; 32]],
        total_chunks: usize,
        resume_key: Option<&str>,
    ) -> Result<(usize, String, u128, WaveAggregateStats)> {
        let mut total_stored = already_stored_addresses.len();
        let mut total_storage = Amount::ZERO;
        let mut total_gas: u128 = 0;
        let mut agg_stats = WaveAggregateStats::default();
        // A wave whose chunks fall short of quorum after retries must not abort
        // the file: its failures are accumulated here and surfaced as a single
        // `PartialUpload` only after every wave has been attempted, mirroring
        // `upload_merkle_from_spill`. Aborting on the first failed wave (the old `?`)
        // discarded all later waves' progress — already self-encrypted, spilled,
        // and in some cases already paid for — converting high per-chunk success
        // into 0% per-file success.
        // Seed with the addresses a preflight already confirmed stored (e.g.
        // the merkle-fallback path passes `merkle_plan.already_stored`), so a
        // returned `PartialUpload.stored` lists every stored chunk and
        // `stored_count == stored.len()` holds for programmatic callers.
        let mut stored_addresses: Vec<[u8; 32]> = already_stored_addresses.to_vec();
        let mut failed: Vec<([u8; 32], String)> = Vec::new();
        let waves: Vec<&[[u8; 32]]> = addresses.chunks(UPLOAD_WAVE_SIZE).collect();
        let wave_count = waves.len();

        // Unconditional breadcrumb: lets a clean run confirm the continue-on-
        // partial single-node path is in effect (the old path aborted the file
        // on the first failed wave instead of continuing across all waves).
        info!(
            "single-node upload: {} chunk(s) in {wave_count} wave(s) (continue-on-partial)",
            addresses.len()
        );

        for (wave_idx, wave_addrs) in waves.into_iter().enumerate() {
            let wave_num = wave_idx + 1;
            let wave_data: Vec<Bytes> = wave_addrs
                .iter()
                .map(|addr| spill.read_chunk(addr))
                .collect::<Result<Vec<_>>>()?;

            info!(
                "Wave {wave_num}/{wave_count}: quoting {} chunks — {total_stored}/{total_chunks} stored so far",
                wave_data.len()
            );
            if let Some(tx) = progress {
                let _ = tx
                    .send(UploadEvent::QuotingChunks {
                        wave: wave_num,
                        total_waves: wave_count,
                        chunks_in_wave: wave_data.len(),
                    })
                    .await;
            }
            // Fold this wave's result. A quorum shortfall (`PartialUpload`) is
            // recoverable and its parts are returned to be recorded here;
            // genuinely fatal errors propagate via `?` and abort the file, as in
            // `upload_merkle_from_spill`.
            let outcome = fold_single_wave(
                self.batch_upload_chunks_with_events(
                    wave_data,
                    progress,
                    total_stored,
                    total_chunks,
                    resume_key,
                )
                .await,
            )?;

            if !outcome.failed.is_empty() {
                warn!(
                    "Wave {wave_num}/{wave_count}: {} chunk(s) failed to store after retries; \
                     continuing with remaining waves",
                    outcome.failed.len()
                );
            }

            total_stored += outcome.stored.len();
            stored_addresses.extend(outcome.stored);
            failed.extend(outcome.failed);
            total_storage += outcome.storage_atto;
            total_gas = total_gas.saturating_add(outcome.gas_wei);
            // Merge per-wave stats (a quorum-short wave contributes none, since
            // `PartialUpload` carries no stats).
            agg_stats.chunk_attempts_total = agg_stats
                .chunk_attempts_total
                .saturating_add(outcome.stats.chunk_attempts_total);
            agg_stats
                .store_durations_ms
                .extend(outcome.stats.store_durations_ms);
            for (slot, count) in agg_stats
                .retries_histogram
                .iter_mut()
                .zip(outcome.stats.retries_histogram.iter())
            {
                *slot = slot.saturating_add(*count);
            }
        }

        // Any chunk still failed after every wave was attempted means the file
        // is not fully stored — surface it as `PartialUpload` (never silently
        // succeed with missing chunks), carrying the real on-chain spend.
        if !failed.is_empty() {
            let failed_count = failed.len();
            warn!(
                "single-node upload incomplete: {failed_count}/{total_chunks} chunks failed after retries"
            );
            return Err(Error::PartialUpload {
                stored: stored_addresses,
                stored_count: total_stored,
                failed,
                failed_count,
                total_chunks,
                spend: Box::new(PartialUploadSpend {
                    storage_cost_atto: total_storage.to_string(),
                    gas_cost_wei: total_gas,
                }),
                reason: format!("{failed_count} chunk(s) failed to store after retries"),
            });
        }

        Ok((
            total_stored,
            total_storage.to_string(),
            total_gas,
            agg_stats,
        ))
    }

    /// Upload chunks from a spill using pre-computed merkle proofs.
    ///
    /// Stores the whole file as a **single cap-bounded fan-out** — not in fixed
    /// waves. The store concurrency limiter is the only throttle: `store_one`
    /// reads each chunk's body from the on-disk spill on demand, so at most
    /// `store_cap` (≤ 64) bodies are ever resident, giving the same
    /// `~store_cap × MAX_CHUNK_SIZE` peak-memory bound the old 64-chunk waves
    /// gave — but with **no wave barrier**, so a slow straggler (e.g. a chunk
    /// whose close-group peers are stale relayed addresses that take minutes to
    /// revalidate) no longer stalls the rest of the file behind it.
    ///
    /// A chunk that is transiently short of quorum (`InsufficientPeers` /
    /// `CloseGroupShortfall` / `RemotePut`) does **not** abort the file, nor
    /// block the pass: the store pass is a **single attempt** (no in-pass
    /// backoff), and quorum-short chunks are collected into a deferred set. After
    /// the pass, [`merkle_deferred_retry`] retries that set in concurrent rounds
    /// ([`DEFERRED_ROUND_DELAYS_SECS`] delays), re-reading each body from the
    /// spill and reusing its proof. Non-quorum errors (e.g. a missing proof)
    /// stay fatal and abort immediately.
    ///
    /// Returns `(chunks_stored, storage_cost_atto, gas_cost_wei)` on success.
    /// Costs come from the `batch_result` which was populated during payment.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PartialUpload`] if any chunk is still short of quorum
    /// after the store pass and every deferred round (other chunks remain
    /// stored), or the underlying error for a non-quorum failure.
    async fn upload_merkle_from_spill(
        &self,
        spill: &ChunkSpill,
        addresses: &[[u8; 32]],
        batch_result: &MerkleBatchPaymentResult,
        already_stored_addresses: &[[u8; 32]],
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> Result<(usize, String, u128, WaveAggregateStats)> {
        let mut total_stored = already_stored_addresses.len();
        let total_chunks = total_stored + addresses.len();
        let mut stored_addresses: Vec<[u8; 32]> = already_stored_addresses.to_vec();
        let mut failed: Vec<([u8; 32], String)> = Vec::new();
        let mut agg_stats = WaveAggregateStats::default();

        // Chunks without a merkle proof were never paid for: a partial
        // `pay_for_merkle_multi_batch` result carries proofs only for the
        // sub-batches whose on-chain payment succeeded. Such a chunk cannot be
        // stored, so record it as failed (surfaced via `PartialUpload` once the
        // storable chunks have been attempted) rather than letting its
        // "missing proof" error abort the whole file and discard every other
        // chunk's progress.
        let (to_store, missing_proof) =
            partition_addresses_by_proof(addresses, &batch_result.proofs);
        if !missing_proof.is_empty() {
            warn!(
                "{} chunk(s) lack a merkle proof (partial payment); reporting them as failed",
                missing_proof.len()
            );
            for addr in &missing_proof {
                failed.push((
                    *addr,
                    format!("Missing merkle proof for chunk {}", hex::encode(addr)),
                ));
            }
        }

        let store_limiter = self.controller().store.clone();

        // Store one chunk to its (freshly re-collected) close group, reusing the
        // chunk's merkle proof. Reads the body from the on-disk spill on demand,
        // so the whole-file store runs as ONE cap-bounded fan-out with no per-wave
        // barrier: a slow straggler (e.g. a chunk whose close-group peers are
        // stale relayed addresses that take minutes to revalidate) no longer
        // holds back the rest of the file. Only the ≤cap in-flight stores hold a
        // body, so peak resident memory is `cap × MAX_CHUNK_SIZE`; the cap is
        // clamped to `MERKLE_STORE_MAX_IN_FLIGHT` (below) so it stays within the
        // ~256 MiB bound the fixed 64-chunk waves gave even if `adaptive.max.store`
        // is configured above 64.
        // Shared across every deferred round so a converged routing table yields
        // a fresh group. Only a quorum shortfall is recoverable; a missing proof
        // or a failed spill read stays fatal. Mirrors `merkle_upload_chunks`.
        let store_one = |addr: [u8; 32]| {
            let limiter = store_limiter.clone();
            let proof_bytes = batch_result.proofs.get(&addr).cloned();
            async move {
                let started = std::time::Instant::now();
                let proof = proof_bytes.ok_or_else(|| {
                    Error::Payment(format!(
                        "Missing merkle proof for chunk {}",
                        hex::encode(addr)
                    ))
                })?;
                let content = spill.read_chunk(&addr)?;
                let peers = self.put_target_peers(&addr).await?;
                observe_op(
                    &limiter,
                    || async move { self.chunk_put_to_close_group(content, proof, &peers).await },
                    classify_error,
                )
                .await
                .map(|_| started)
            }
        };

        info!(
            "Storing {} chunks (merkle) as a single cap-bounded pass — {total_stored}/{total_chunks} stored so far",
            to_store.len()
        );

        // Store the WHOLE file in one cap-bounded fan-out (`max_attempts = 1`, no
        // backoff): no wave barrier, so a slow straggler (dead-relay peers) can't
        // hold back the rest of the file. The store cap re-reads the limiter per
        // slot, so it maxes at 64 → ≤64 bodies resident (bodies read from spill on
        // demand by `store_one`), the same peak-memory bound the fixed 64-chunk
        // waves gave. Quorum-short chunks are collected and deferred to the
        // post-pass concurrent retry rather than parking slots behind a backoff.
        // `merkle_store_cap` clamps to `MERKLE_STORE_MAX_IN_FLIGHT` so a high
        // configured `adaptive.max.store` can't hold more than the wave-era
        // ~256 MB of spilled bodies resident (PR #137 review).
        let cap = || merkle_store_cap(store_limiter.current());
        let outcome = merkle_store_with_retry(
            to_store.clone(),
            cap,
            1,
            std::time::Duration::ZERO,
            progress,
            total_stored,
            total_chunks,
            &store_one,
        )
        .await?;

        // Record confirmed stores from the explicit set the store helper reports.
        // Using that set (rather than inferring "chunks minus failed") keeps
        // `stored_addresses` correct even when a fatal abort leaves some chunks
        // neither stored nor reported short of quorum.
        stored_addresses.extend(&outcome.stored_addresses);
        total_stored = outcome.stored;

        // Merge store stats (durations, attempts, per-round histogram).
        agg_stats.chunk_attempts_total = agg_stats
            .chunk_attempts_total
            .saturating_add(outcome.stats.chunk_attempts_total);
        agg_stats
            .store_durations_ms
            .extend(outcome.stats.store_durations_ms);
        for (slot, count) in agg_stats
            .retries_histogram
            .iter_mut()
            .zip(outcome.stats.retries_histogram.iter())
        {
            *slot = slot.saturating_add(*count);
        }

        if let Some(e) = outcome.fatal {
            // A non-quorum store error is fatal (missing proofs were filtered out
            // above, so this is a genuine network/store failure). Preserve every
            // chunk stored so far and report every not-stored chunk as failed, so
            // the `PartialUpload` counts are accurate.
            warn!("merkle store aborted: {e}");
            let mut known_failed = failed;
            known_failed.extend(outcome.failed_addresses);
            return Err(partial_upload_after_fatal(
                addresses,
                stored_addresses,
                total_stored,
                total_chunks,
                known_failed,
                PartialUploadSpend {
                    storage_cost_atto: batch_result.storage_cost_atto.clone(),
                    gas_cost_wei: batch_result.gas_cost_wei,
                },
                format!("merkle chunk store aborted: {e}"),
            ));
        }

        // Non-fatal: quorum-short chunks are deferred (not failed yet) for the
        // post-pass concurrent retry. A deferred chunk joins `stored_addresses`
        // only if/when a later round stores it.
        let deferred: Vec<([u8; 32], String)> = outcome.failed_addresses;

        // The store pass never blocked on backoff; now retry the deferred set in
        // concurrent rounds. Bodies are re-read from the spill by `store_one`
        // (peak RAM unchanged) and proofs re-attached. Chunks still short after
        // the final round become `failed`; a non-quorum error aborts as
        // `PartialUpload`.
        if !deferred.is_empty() {
            info!(
                "Deferring {} merkle chunk(s) short of quorum for concurrent retry after the store pass",
                deferred.len()
            );
            let dr = merkle_deferred_retry(
                deferred,
                &DEFERRED_ROUND_DELAYS_SECS,
                |n: usize| merkle_store_cap(store_limiter.current()).min(n.max(1)),
                progress,
                total_stored,
                total_chunks,
                &store_one,
            )
            .await?;

            stored_addresses.extend(dr.stored_addresses);
            total_stored = dr.stored;

            // Merge the deferred pass's stats — its histogram is already mapped
            // to the right per-round slots — into the file aggregate.
            agg_stats.chunk_attempts_total = agg_stats
                .chunk_attempts_total
                .saturating_add(dr.stats.chunk_attempts_total);
            agg_stats
                .store_durations_ms
                .extend(dr.stats.store_durations_ms);
            for (slot, count) in agg_stats
                .retries_histogram
                .iter_mut()
                .zip(dr.stats.retries_histogram.iter())
            {
                *slot = slot.saturating_add(*count);
            }

            if let Some(reason) = dr.fatal {
                // A non-quorum store error during a deferred round is fatal, the
                // same as in the wave path: preserve everything stored so far and
                // report every not-stored chunk as failed.
                warn!("merkle deferred retry aborted: {reason}");
                let mut known_failed = failed;
                known_failed.extend(dr.failed_addresses);
                return Err(partial_upload_after_fatal(
                    addresses,
                    stored_addresses,
                    total_stored,
                    total_chunks,
                    known_failed,
                    PartialUploadSpend {
                        storage_cost_atto: batch_result.storage_cost_atto.clone(),
                        gas_cost_wei: batch_result.gas_cost_wei,
                    },
                    format!("merkle chunk store aborted: {reason}"),
                ));
            }
            failed.extend(dr.failed_addresses);
        }

        // A file with any permanently-failed chunk is not fully stored — surface
        // it as `PartialUpload`, but only after the store pass and every deferred
        // retry round are exhausted (never silently succeed with missing chunks).
        if !failed.is_empty() {
            let failed_count = failed.len();
            let total_attempts = 1 + DEFERRED_ROUND_DELAYS_SECS.len();
            warn!(
                "merkle upload incomplete: {failed_count}/{total_chunks} chunks short of quorum after retries"
            );
            return Err(Error::PartialUpload {
                stored: stored_addresses,
                stored_count: total_stored,
                failed,
                failed_count,
                total_chunks,
                spend: Box::new(PartialUploadSpend {
                    storage_cost_atto: batch_result.storage_cost_atto.clone(),
                    gas_cost_wei: batch_result.gas_cost_wei,
                }),
                reason: format!(
                    "{failed_count} chunk(s) short of quorum after {total_attempts} attempts"
                ),
            });
        }

        Ok((
            total_stored,
            batch_result.storage_cost_atto.clone(),
            batch_result.gas_cost_wei,
            agg_stats,
        ))
    }

    /// Download and decrypt a file from the network, writing it to disk.
    ///
    /// Uses `streaming_decrypt` so that only one batch of chunks lives in
    /// memory at a time, avoiding OOM on large files. Chunks are fetched
    /// concurrently within each batch, then decrypted data is written to
    /// disk incrementally.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Panics
    ///
    /// Requires a multi-threaded Tokio runtime (`flavor = "multi_thread"`).
    /// Will panic if called from a `current_thread` runtime because
    /// `streaming_decrypt` takes a synchronous callback that must bridge
    /// back to async via `block_in_place`.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved, decryption fails,
    /// or the file cannot be written.
    pub async fn file_download(&self, data_map: &DataMap, output: &Path) -> Result<u64> {
        self.file_download_with_progress(data_map, output, None)
            .await
    }

    /// Download and decrypt a file, trying the requested number of
    /// closest peers for every chunk fetch.
    ///
    /// Returns the number of bytes written.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved, decryption fails,
    /// or the file cannot be written.
    pub async fn file_download_from_closest_peers(
        &self,
        data_map: &DataMap,
        output: &Path,
        peer_count: NonZeroUsize,
    ) -> Result<u64> {
        self.file_download_with_progress_from_closest_peers(data_map, output, None, peer_count)
            .await
    }

    /// Download and decrypt a file with progress events, trying the
    /// requested number of closest peers for every chunk fetch.
    ///
    /// Same as [`Client::file_download_from_closest_peers`] but sends
    /// [`DownloadEvent`]s for UI feedback.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved, decryption fails,
    /// or the file cannot be written.
    pub async fn file_download_with_progress_from_closest_peers(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: NonZeroUsize,
    ) -> Result<u64> {
        self.file_download_with_progress_using_peer_count(
            data_map,
            output,
            progress,
            peer_count.get(),
            None,
        )
        .await
    }

    /// Download a file with progress and optional per-attempt JSONL diagnostics.
    ///
    /// Passing `None` preserves the standard path without diagnostic records.
    pub async fn file_download_with_progress_and_diagnostics_from_closest_peers(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: NonZeroUsize,
        diagnostics: Option<DownloadDiagnosticsSender>,
    ) -> Result<u64> {
        self.file_download_with_progress_using_peer_count(
            data_map,
            output,
            progress,
            peer_count.get(),
            diagnostics,
        )
        .await
    }

    /// Download and decrypt a file with peer-health diagnostics.
    ///
    /// Each file chunk is fetched by querying every selected closest peer,
    /// not by returning after the first successful peer. The returned report
    /// records which peers had each chunk and which did not. DataMap
    /// resolution still uses the normal early-return fetch path; diagnostics
    /// are for file chunks only.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved, decryption fails,
    /// or the file cannot be written.
    pub async fn file_download_with_peer_report_from_closest_peers(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: NonZeroUsize,
    ) -> Result<FileDownloadWithPeerReport> {
        let chunk_reports = Arc::new(Mutex::new(Vec::new()));
        let bytes_written = self
            .file_download_with_progress_using_peer_count_and_reports(
                data_map,
                output,
                progress,
                peer_count.get(),
                Some(chunk_reports.clone()),
                None,
            )
            .await?;

        let chunk_reports = chunk_reports
            .lock()
            .map_err(|_| Error::Storage("file chunk peer report lock poisoned".to_string()))?
            .clone();
        let chunk_reports = file_chunk_reports_from_recorded_sweeps(chunk_reports);

        Ok(FileDownloadWithPeerReport {
            bytes_written,
            chunk_reports,
        })
    }

    async fn download_fetch_file_chunk(
        &self,
        idx: usize,
        hash: XorName,
        context: FileDownloadFetchContext,
        is_deferred_retry: bool,
        attempt: usize,
    ) -> std::result::Result<DownloadBatchEntry, self_encryption::Error> {
        let addr = hash.0;
        let addr_hex = hex::encode(addr);

        let chunk_content = if let Some(peer_reports) = context.peer_reports {
            match self
                .chunk_get_from_closest_peer_group(&addr, context.peer_count)
                .await
            {
                Ok(results) => {
                    let (content, sweep) = file_chunk_sweep_report_from_peer_results(
                        attempt,
                        is_deferred_retry,
                        &results,
                    );
                    peer_reports
                        .lock()
                        .map_err(|_| {
                            self_encryption::Error::Generic(
                                "file chunk peer report lock poisoned".to_string(),
                            )
                        })?
                        .push(RecordedFileChunkPeerSweep {
                            index: idx + 1,
                            address: addr,
                            sweep,
                        });
                    content
                }
                Err(e) => {
                    if is_deferred_retry {
                        info!(
                            "Deferred all-peer retry for {addr_hex} hit transient error: {e}; re-deferring"
                        );
                    } else {
                        info!("First-pass all-peer fetch error for {addr_hex}: {e}; deferring");
                    }
                    peer_reports
                        .lock()
                        .map_err(|_| {
                            self_encryption::Error::Generic(
                                "file chunk peer report lock poisoned".to_string(),
                            )
                        })?
                        .push(RecordedFileChunkPeerSweep {
                            index: idx + 1,
                            address: addr,
                            sweep: file_chunk_sweep_report_from_error(
                                attempt,
                                is_deferred_retry,
                                &e,
                            ),
                        });
                    None
                }
            }
        } else {
            // Normal path: early-return after the first peer that has the
            // chunk. When diagnostics are enabled we thread a per-chunk
            // diagnostics context through so each peer attempt in the sweep
            // is recorded; when disabled (`None`) this is a zero-cost pass.
            let diag = context
                .diagnostics
                .as_ref()
                .map(|sender| ChunkFetchDiagnostics::new(sender, attempt, idx + 1, addr));
            match self
                .chunk_get_observed_from_closest_peers(&addr, context.peer_count, diag.as_ref())
                .await
            {
                Ok(Some(chunk)) => Some(chunk.content),
                Ok(None) => None,
                Err(e) => {
                    if is_deferred_retry {
                        info!(
                            "Deferred retry for {addr_hex} hit transient error: {e}; re-deferring"
                        );
                    } else {
                        info!("First-pass fetch error for {addr_hex}: {e}; deferring");
                    }
                    None
                }
            }
        };

        let Some(content) = chunk_content else {
            return Ok((idx, Err(hash)));
        };

        let fetched = context
            .fetched_ref
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if is_deferred_retry {
            info!(
                "Downloaded {fetched}/{} (deferred retry)",
                context.total_chunks
            );
        } else {
            let total_chunks = context.total_chunks;
            info!("Downloaded {fetched}/{total_chunks}");
        }
        if let Some(ref tx) = context.progress_ref {
            let _ = tx.try_send(DownloadEvent::ChunksFetched {
                fetched,
                total: context.total_chunks,
            });
        }

        Ok((idx, Ok(content)))
    }

    /// Shared download core: resolve the DataMap, then fetch + streaming-decrypt
    /// the file one batch at a time, handing each decrypted plaintext segment
    /// (in order) to `on_chunk`. Constant memory — only one decrypt batch is
    /// resident at a time. Returns the total plaintext bytes produced.
    ///
    /// `on_chunk` is async so a sink can apply backpressure (e.g. a bounded
    /// channel). Driving the decrypt iterator runs the batched chunk fetch via
    /// `block_in_place`, so this requires a multi-threaded Tokio runtime.
    ///
    /// Every chunk fetch tries `peer_count` closest peers.
    ///
    /// Progress reporting (via `progress`):
    /// 1. Resolves hierarchical DataMaps to the root level first (reports as
    ///    `ChunksFetched` with `total: 0` during resolution)
    /// 2. Once the root DataMap is known, sends `total_chunks` with accurate count
    /// 3. Fetches data chunks with accurate `fetched/total` progress
    async fn download_decrypted_chunks<F, Fut>(
        &self,
        data_map: &DataMap,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: usize,
        peer_reports: Option<Arc<Mutex<Vec<RecordedFileChunkPeerSweep>>>>,
        diagnostics: Option<DownloadDiagnosticsSender>,
        mut on_chunk: F,
    ) -> Result<u64>
    where
        F: FnMut(Bytes) -> Fut,
        Fut: std::future::Future<Output = Result<()>>,
    {
        let handle = Handle::current();

        // Phase 1: Resolve hierarchical DataMap to root level.
        // This fetches child DataMap chunks (typically 3) to discover the real chunk count.
        let root_map = if data_map.is_child() {
            let dm_chunks = data_map.len();
            if let Some(ref tx) = progress {
                let _ = tx.try_send(DownloadEvent::ResolvingDataMap {
                    total_map_chunks: dm_chunks,
                });
            }

            let resolve_progress = progress.clone();
            let resolve_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));

            let resolved = tokio::task::block_in_place(|| {
                let counter_ref = resolve_counter.clone();
                let progress_ref = resolve_progress.clone();
                let fetch_limiter = self.controller().fetch.clone();
                let fetch = |batch: &[(usize, XorName)]| {
                    let batch_owned: Vec<(usize, XorName)> = batch.to_vec();
                    let counter = counter_ref.clone();
                    let prog = progress_ref.clone();
                    let limiter = fetch_limiter.clone();
                    handle.block_on(async {
                        // Use rebucketed_unordered so the in-flight cap
                        // is re-read from the limiter as each slot frees.
                        // `buffer_unordered` snapshots the cap once at
                        // pipeline build, which means observe_op
                        // signals from inside chunk_get cannot reduce
                        // concurrency on the current batch — exactly
                        // the case where load-shedding is needed.
                        let mut results = rebucketed_unordered(
                            &limiter,
                            batch_owned,
                            |(idx, hash): (usize, XorName)| {
                                let counter = counter.clone();
                                let prog = prog.clone();
                                async move {
                                    let addr = hash.0;
                                    // chunk_get_observed feeds the
                                    // adaptive fetch limiter once per
                                    // call via chunk_get_outcome
                                    // (Ok(None) -> Timeout is the
                                    // load-shedding signal for
                                    // sustained close-group exhaustion).
                                    let chunk = self
                                        .chunk_get_observed_from_closest_peers(
                                            &addr, peer_count, None,
                                        )
                                        .await
                                        .map_err(|e| {
                                            self_encryption::Error::Generic(format!(
                                                "DataMap resolution failed: {e}"
                                            ))
                                        })?
                                        .ok_or_else(|| {
                                            self_encryption::Error::Generic(format!(
                                                "DataMap chunk not found: {}",
                                                hex::encode(addr)
                                            ))
                                        })?;
                                    let fetched = counter
                                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                        + 1;
                                    if let Some(ref tx) = prog {
                                        let _ =
                                            tx.try_send(DownloadEvent::MapChunkFetched { fetched });
                                    }
                                    Ok::<_, self_encryption::Error>((idx, chunk.content))
                                }
                            },
                        )
                        .await?;
                        // CRITICAL: self_encryption::get_root_data_map_parallel
                        // pairs the returned Vec POSITIONALLY with the input
                        // hashes via .zip() and discards our idx field.
                        // rebucketed_unordered preserves first-completion
                        // order, so sort by idx to restore input order
                        // before returning.
                        results.sort_by_key(|(idx, _)| *idx);
                        Ok(results)
                    })
                };
                get_root_data_map_parallel(data_map.clone(), &fetch)
            })
            .map_err(|e| Error::Encryption(format!("DataMap resolution failed: {e}")))?;

            info!(
                "Resolved hierarchical DataMap: {} data chunks",
                resolved.len()
            );
            resolved
        } else {
            data_map.clone()
        };

        // Phase 2: Now we know the real chunk count.
        let total_chunks = root_map.len();
        if let Some(ref tx) = progress {
            let _ = tx.try_send(DownloadEvent::DataMapResolved { total_chunks });
        }

        // Phase 3: Fetch and decrypt data chunks with accurate progress.
        let fetched_counter = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let fetched_for_closure = fetched_counter.clone();
        let progress_for_closure = progress.clone();
        let peer_reports_for_closure = peer_reports.clone();
        let diagnostics_for_closure = diagnostics.clone();

        let fetch_limiter_outer = self.controller().fetch.clone();
        let usable_memory = usable_memory_bytes();
        let configured_batch_floor = stream_decrypt_batch_size();
        let fetch_cap = fetch_limiter_outer.current();
        let decrypt_batch_size = adaptive_stream_decrypt_batch_size(
            total_chunks,
            fetch_cap,
            configured_batch_floor,
            usable_memory,
        );
        info!(
            total_chunks,
            fetch_cap,
            configured_batch_floor,
            ?usable_memory,
            decrypt_batch_size,
            "Selected adaptive stream decrypt batch size"
        );

        let stream = streaming_decrypt_with_batch_size(
            &root_map,
            |batch: &[(usize, XorName)]| {
                let batch_owned: Vec<(usize, XorName)> = batch.to_vec();
                let fetch_context = FileDownloadFetchContext {
                    total_chunks,
                    peer_count,
                    fetched_ref: fetched_for_closure.clone(),
                    progress_ref: progress_for_closure.clone(),
                    peer_reports: peer_reports_for_closure.clone(),
                    diagnostics: diagnostics_for_closure.clone(),
                };
                let fetch_limiter = fetch_limiter_outer.clone();

                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        // First pass: try every chunk in the batch. Both normal
                        // and diagnostic modes preserve the closest-peer
                        // early-return path; diagnostics only records the peers
                        // actually attempted before a chunk is found.
                        // Any missing chunk or transient fetch error is encoded
                        // as Err(hash), so one noisy chunk does not abort the
                        // whole batch before the deferred retry rounds run.
                        let first_fetch_context = fetch_context.clone();
                        let raw: Vec<DownloadBatchEntry> = rebucketed_unordered(
                            &fetch_limiter,
                            batch_owned,
                            |(idx, hash): (usize, XorName)| {
                                let fetch_context = first_fetch_context.clone();
                                async move {
                                    self.download_fetch_file_chunk(
                                        idx,
                                        hash,
                                        fetch_context,
                                        false,
                                        FIRST_DIAGNOSTIC_FETCH_ATTEMPT,
                                    )
                                    .await
                                }
                            },
                        )
                        .await?;

                        // Partition: things we already have vs the
                        // deferred set we need to retry.
                        let mut results: Vec<(usize, bytes::Bytes)> = Vec::new();
                        let mut deferred: Vec<(usize, XorName)> = Vec::new();
                        for (idx, inner) in raw {
                            match inner {
                                Ok(bytes) => results.push((idx, bytes)),
                                Err(hash) => deferred.push((idx, hash)),
                            }
                        }

                        // Deferred retry pass: retry the deferred chunks
                        // in CONCURRENT rounds (reusing the fetch
                        // limiter's cap), not serially. The first round
                        // fires immediately — most deferrals on a
                        // healthy-but-lossy link are peer-side noise
                        // that clears in well under a second, and
                        // serializing them behind mandatory multi-second
                        // sleeps was the single biggest throughput sink
                        // on such links (a batch deferring ~20 chunks
                        // burned minutes of near-zero throughput even
                        // though every chunk succeeded on its first
                        // retry). Only chunks that survive a round get a
                        // longer back-off before the next, so genuine
                        // saturation still gets time to settle.
                        if !deferred.is_empty() {
                            // Round delays in seconds. Round 0 is
                            // immediate; later rounds back off to ride
                            // out sustained saturation.
                            const DEFERRED_ROUND_DELAYS_SECS: [u64; 3] = [0, 15, 45];
                            info!(
                                "Deferring {} chunk(s) for concurrent retry after batch settles",
                                deferred.len()
                            );
                            let mut remaining = deferred;
                            for (round, &delay_secs) in
                                DEFERRED_ROUND_DELAYS_SECS.iter().enumerate()
                            {
                                if remaining.is_empty() {
                                    break;
                                }
                                if delay_secs > 0 {
                                    tokio::time::sleep(std::time::Duration::from_secs(delay_secs))
                                        .await;
                                }
                                info!(
                                    "Deferred retry round {}/{}: {} chunk(s)",
                                    round + 1,
                                    DEFERRED_ROUND_DELAYS_SECS.len(),
                                    remaining.len(),
                                );
                                let round_input = std::mem::take(&mut remaining);
                                let retry_fetch_context = fetch_context.clone();
                                let round_results: Vec<DownloadBatchEntry> = rebucketed_unordered(
                                    &fetch_limiter,
                                    round_input,
                                    |(idx, hash): (usize, XorName)| {
                                        let fetch_context = retry_fetch_context.clone();
                                        async move {
                                            self.download_fetch_file_chunk(
                                                idx,
                                                hash,
                                                fetch_context,
                                                true,
                                                round + DEFERRED_RETRY_ATTEMPT_OFFSET,
                                            )
                                            .await
                                        }
                                    },
                                )
                                .await?;
                                for (idx, inner) in round_results {
                                    match inner {
                                        Ok(bytes) => results.push((idx, bytes)),
                                        Err(hash) => remaining.push((idx, hash)),
                                    }
                                }
                            }
                            if let Some((_, hash)) = remaining.first() {
                                return Err(self_encryption::Error::Generic(format!(
                                    "Chunk not found after {} deferred retry rounds: {}",
                                    DEFERRED_ROUND_DELAYS_SECS.len(),
                                    hex::encode(hash.0),
                                )));
                            }
                        }

                        // streaming_decrypt itself sort_by_keys before
                        // zipping, but the same closure is also passed
                        // through get_root_data_map_parallel internally
                        // (see self_encryption::stream_decrypt.rs::new), and
                        // THAT path zips positionally without sorting. Sort
                        // here so both consumers see input order.
                        results.sort_by_key(|(idx, _)| *idx);
                        Ok(results)
                    })
                })
            },
            decrypt_batch_size,
        )
        .map_err(|e| Error::Encryption(format!("streaming decrypt failed: {e}")))?;

        // Drive the iterator (each `next()` runs the batched fetch via
        // block_in_place) and hand each decrypted segment to the sink in
        // order. Awaiting the sink between items yields back to the runtime so
        // a bounded sink can apply backpressure.
        let mut bytes_total = 0u64;
        for chunk_result in stream {
            let chunk: Bytes =
                chunk_result.map_err(|e| Error::Encryption(format!("decryption failed: {e}")))?;
            bytes_total += chunk.len() as u64;
            on_chunk(chunk).await?;
        }
        Ok(bytes_total)
    }

    /// Download and decrypt a file to disk, with optional progress events.
    ///
    /// Same as [`Client::file_download`] but sends [`DownloadEvent`]s for UI
    /// feedback. Streams to a temp file (one decrypt batch resident at a time)
    /// and renames atomically on success. A `TempDownload` guard removes the
    /// staging file on any error path, including a panic.
    pub async fn file_download_with_progress(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
    ) -> Result<u64> {
        self.file_download_with_progress_using_peer_count(
            data_map,
            output,
            progress,
            self.config().close_group_size,
            None,
        )
        .await
    }

    /// Download and decrypt a file to disk with progress events, trying
    /// `peer_count` closest peers for every chunk fetch.
    ///
    /// Streams to a temp file (one decrypt batch resident at a time) and
    /// renames atomically on success.
    async fn file_download_with_progress_using_peer_count(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: usize,
        diagnostics: Option<DownloadDiagnosticsSender>,
    ) -> Result<u64> {
        self.file_download_with_progress_using_peer_count_and_reports(
            data_map,
            output,
            progress,
            peer_count,
            None,
            diagnostics,
        )
        .await
    }

    async fn file_download_with_progress_using_peer_count_and_reports(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
        peer_count: usize,
        peer_reports: Option<Arc<Mutex<Vec<RecordedFileChunkPeerSweep>>>>,
        diagnostics: Option<DownloadDiagnosticsSender>,
    ) -> Result<u64> {
        debug!("Downloading file to {}", output.display());

        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let unique: u64 = rand::random();
        let tmp_path = parent.join(format!(".ant_download_{}_{unique}.tmp", std::process::id()));

        // Guard removes the staging file on any early return OR a panic unwind
        // out of the `block_in_place` decrypt loop; defused only by a
        // successful commit(). Centralizes what used to be three duplicated
        // cleanup arms.
        let tmp = TempDownload::new(tmp_path);
        let mut file = std::fs::File::create(tmp.path())?;

        let bytes_written = self
            .download_decrypted_chunks(
                data_map,
                progress,
                peer_count,
                peer_reports,
                diagnostics,
                |bytes| {
                    let r = file.write_all(&bytes).map_err(Error::from);
                    std::future::ready(r)
                },
            )
            .await?;
        file.flush()?;
        drop(file); // close the handle before rename (Windows won't rename an open file)

        tmp.commit(output)?;
        info!(
            "File downloaded: {bytes_written} bytes written to {}",
            output.display()
        );
        Ok(bytes_written)
    }

    /// Download and decrypt a file, streaming the plaintext to `sink` instead
    /// of writing to disk.
    ///
    /// Constant memory (one decrypt batch resident at a time); the caller
    /// receives bytes progressively as each batch decrypts, suitable for
    /// forwarding to an HTTP chunked body or a gRPC response stream. The
    /// bounded `sink` applies backpressure. If the receiver is dropped (e.g.
    /// the client disconnected) the download stops early and returns
    /// [`Error::Cancelled`].
    ///
    /// The channel item type is `Result<Bytes, Error>`, so the caller sets up:
    ///
    /// ```ignore
    /// let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, Error>>(8);
    /// ```
    ///
    /// Typically the caller `tokio::spawn`s this and converts the matching
    /// `Receiver` into its response stream. Requires a multi-threaded Tokio
    /// runtime (the decrypt iterator uses `block_in_place`).
    pub async fn file_download_to_sender(
        &self,
        data_map: &DataMap,
        sink: mpsc::Sender<std::result::Result<Bytes, Error>>,
        progress: Option<mpsc::Sender<DownloadEvent>>,
    ) -> Result<u64> {
        let peer_count = self.config().close_group_size;
        self.download_decrypted_chunks(data_map, progress, peer_count, None, None, |bytes| {
            let sink = sink.clone();
            async move {
                sink.send(Ok(bytes))
                    .await
                    .map_err(|_| Error::Cancelled("download stream receiver dropped".into()))
            }
        })
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Throwaway payment result — the assembler only moves it into the resume
    /// handle, never inspects it.
    fn dummy_batch_result() -> MerkleBatchPaymentResult {
        MerkleBatchPaymentResult {
            proofs: HashMap::new(),
            chunk_count: 0,
            storage_cost_atto: "0".into(),
            gas_cost_wei: 0,
            merkle_payment_timestamp: 0,
        }
    }

    fn empty_chunk_store() -> ExternalChunkStore {
        ExternalChunkStore::from_spill(ChunkSpill::new().unwrap())
    }

    /// A minimal already-paid chunk — the wave assembler only moves it and reads
    /// its `address`, so the body/proof/targets can be trivial.
    fn paid_chunk(address: [u8; 32]) -> PaidChunk {
        PaidChunk {
            content: Bytes::from_static(b"x"),
            address,
            quoted_peers: Vec::new(),
            proof_bytes: Vec::new(),
        }
    }

    #[test]
    fn assemble_complete_on_full_store() {
        let outcome = assemble_merkle_finalize_outcome(
            Ok((3, "0".into(), 0, WaveAggregateStats::default())),
            DataMap::new(vec![]),
            Some([9u8; 32]),
            3,
            empty_chunk_store(),
            dummy_batch_result(),
        )
        .expect("a fully-stored pass is not an error");
        match outcome {
            FinalizeOutcome::Complete(result) => {
                assert_eq!(result.chunks_stored, 3);
                assert_eq!(result.chunks_failed, 0);
                assert_eq!(result.total_chunks, 3);
                assert_eq!(result.data_map_address, Some([9u8; 32]));
                assert!(matches!(result.payment_mode_used, PaymentMode::Merkle));
            }
            FinalizeOutcome::Partial { .. } => panic!("expected Complete"),
        }
    }

    #[test]
    fn assemble_partial_retains_resume_for_unstored() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        let c = [3u8; 32];
        // One chunk stored, two still short of quorum after retries.
        let store_result = Err(Error::PartialUpload {
            stored: vec![a],
            stored_count: 1,
            failed: vec![(b, "quorum".into()), (c, "quorum".into())],
            failed_count: 2,
            total_chunks: 3,
            spend: Box::new(PartialUploadSpend {
                storage_cost_atto: "777".into(),
                gas_cost_wei: 0,
            }),
            reason: "merkle chunk store aborted".into(),
        });
        let outcome = assemble_merkle_finalize_outcome(
            store_result,
            DataMap::new(vec![]),
            Some([9u8; 32]),
            3,
            empty_chunk_store(),
            dummy_batch_result(),
        )
        .expect("a quorum shortfall is Ok(Partial), never Err");
        match outcome {
            FinalizeOutcome::Partial { result, resume } => {
                // Snapshot reports real progress + spend from the payment.
                assert_eq!(result.chunks_stored, 1);
                assert_eq!(result.chunks_failed, 2);
                assert_eq!(result.total_chunks, 3);
                assert_eq!(result.storage_cost_atto, "777");
                let FinalizeResume::Merkle(m) = resume else {
                    panic!("expected a merkle resume handle");
                };
                // Resume targets exactly the unstored chunks, carries the stored
                // set forward as already-stored, and preserves public + total.
                assert_eq!(m.unstored_addresses, vec![b, c]);
                assert_eq!(m.stored_addresses, vec![a]);
                assert_eq!(m.total_chunks, 3);
                assert_eq!(m.data_map_address, Some([9u8; 32]));
            }
            FinalizeOutcome::Complete(_) => panic!("expected Partial"),
        }
    }

    #[test]
    fn resumable_guard_rejects_partial_payment() {
        // Regression for the PR #172 review: a Some/None mix must not reach the
        // resumable path — its resume handle could never acquire proofs for the
        // unpaid chunks, so repeated finalize_resume calls would return Partial
        // forever instead of draining to Complete.
        let err = require_fully_paid_for_resumable(&[Some([1u8; 32]), None, Some([2u8; 32])])
            .expect_err("a mix of paid and unpaid sub-batches must be rejected");
        match err {
            Error::Payment(msg) => {
                assert!(msg.contains("1/3"), "counts unpaid batches: {msg}");
                assert!(
                    msg.contains("finalize_upload_merkle_multi()"),
                    "points at the non-resumable path: {msg}"
                );
            }
            other => panic!("expected Error::Payment, got {other:?}"),
        }
    }

    #[test]
    fn resumable_guard_accepts_fully_paid() {
        require_fully_paid_for_resumable(&[Some([1u8; 32]), Some([2u8; 32])])
            .expect("fully-paid winner hashes pass the guard");
        require_fully_paid_for_resumable(&[]).expect(
            "an empty set has no unpaid batch — fold_external_merkle_payments \
             rejects it as nothing-to-finalize",
        );
    }

    #[test]
    fn merkle_resume_handle_drains_to_complete() {
        // Regression for the PR #172 review: drive the resume-handoff contract
        // through two passes and prove the handle drains. Pass 1 stores one of
        // three chunks; the Partial handle carries the unstored set plus the
        // original payment. Pass 2 re-drives exactly that handle's material and
        // stores the rest, reaching Complete with whole-file counts.
        let a = [1u8; 32];
        let b = [2u8; 32];
        let c = [3u8; 32];
        let first_pass = Err(Error::PartialUpload {
            stored: vec![a],
            stored_count: 1,
            failed: vec![(b, "quorum".into()), (c, "quorum".into())],
            failed_count: 2,
            total_chunks: 3,
            spend: Box::new(PartialUploadSpend {
                storage_cost_atto: "777".into(),
                gas_cost_wei: 0,
            }),
            reason: "quorum shortfall".into(),
        });
        let outcome = assemble_merkle_finalize_outcome(
            first_pass,
            DataMap::new(vec![]),
            Some([9u8; 32]),
            3,
            empty_chunk_store(),
            dummy_batch_result(),
        )
        .expect("a quorum shortfall is Ok(Partial), never Err");
        let FinalizeOutcome::Partial { resume, .. } = outcome else {
            panic!("expected Partial after a shortfall pass");
        };
        let FinalizeResume::Merkle(m) = resume else {
            panic!("expected a merkle resume handle");
        };
        assert_eq!(m.unstored_addresses, vec![b, c]);

        // Second pass: finalize_resume feeds the handle's own fields back into
        // the drive; simulate its store pass succeeding for the remainder.
        let second_pass = Ok((3, "0".into(), 0, WaveAggregateStats::default()));
        let outcome = assemble_merkle_finalize_outcome(
            second_pass,
            m.data_map,
            m.data_map_address,
            m.total_chunks,
            m.chunk_store,
            m.batch_result,
        )
        .expect("a fully-stored resume pass is not an error");
        match outcome {
            FinalizeOutcome::Complete(result) => {
                assert_eq!(result.chunks_stored, 3);
                assert_eq!(result.chunks_failed, 0);
                assert_eq!(result.total_chunks, 3);
                assert_eq!(result.data_map_address, Some([9u8; 32]));
            }
            FinalizeOutcome::Partial { .. } => panic!("expected Complete after the drain pass"),
        }
    }

    #[test]
    fn assemble_propagates_fatal_error() {
        // A non-recoverable error is not folded into a resumable outcome.
        let outcome = assemble_merkle_finalize_outcome(
            Err(Error::Payment("on-chain call reverted".into())),
            DataMap::new(vec![]),
            None,
            3,
            empty_chunk_store(),
            dummy_batch_result(),
        );
        assert!(matches!(outcome, Err(Error::Payment(_))));
    }

    #[test]
    fn assemble_wave_complete_when_all_stored() {
        let a = [1u8; 32];
        let wave_result = WaveResult {
            stored: vec![a],
            failed: Vec::new(),
            chunk_attempts_total: 1,
            store_durations_ms: vec![5],
            retries_per_chunk: vec![0],
        };
        let mut retained = HashMap::new();
        retained.insert(a, paid_chunk(a));
        let outcome = assemble_wave_finalize_outcome(
            wave_result,
            retained,
            DataMap::new(vec![]),
            Some([9u8; 32]),
            1,
            0,
            "500".into(),
        );
        match outcome {
            FinalizeOutcome::Complete(result) => {
                assert_eq!(result.chunks_stored, 1);
                assert_eq!(result.chunks_failed, 0);
                assert_eq!(result.storage_cost_atto, "500");
                assert!(matches!(result.payment_mode_used, PaymentMode::Single));
            }
            FinalizeOutcome::Partial { .. } => panic!("expected Complete"),
        }
    }

    #[test]
    fn assemble_wave_partial_retains_failed_paid_chunks() {
        let a = [1u8; 32]; // stored
        let b = [2u8; 32]; // failed
        let c = [3u8; 32]; // failed
        let wave_result = WaveResult {
            stored: vec![a],
            failed: vec![(b, "quorum".into()), (c, "quorum".into())],
            chunk_attempts_total: 3,
            store_durations_ms: vec![5],
            retries_per_chunk: vec![0],
        };
        // All three were paid; only the two failures should be retained.
        let mut retained = HashMap::new();
        for addr in [a, b, c] {
            retained.insert(addr, paid_chunk(addr));
        }
        let outcome = assemble_wave_finalize_outcome(
            wave_result,
            retained,
            DataMap::new(vec![]),
            Some([9u8; 32]),
            3,
            0,
            "500".into(),
        );
        match outcome {
            FinalizeOutcome::Partial { result, resume } => {
                assert_eq!(result.chunks_stored, 1);
                assert_eq!(result.chunks_failed, 2);
                assert_eq!(result.storage_cost_atto, "500");
                let FinalizeResume::Wave(w) = resume else {
                    panic!("expected a wave resume handle");
                };
                // Exactly the two failed chunks are kept for re-store — no re-pay.
                let mut got: Vec<[u8; 32]> =
                    w.failed_paid_chunks.iter().map(|pc| pc.address).collect();
                got.sort();
                assert_eq!(got, vec![b, c]);
                assert_eq!(w.stored_count, 1);
                assert_eq!(w.total_chunks, 3);
            }
            FinalizeOutcome::Complete(_) => panic!("expected Partial"),
        }
    }

    #[test]
    fn merkle_store_cap_clamps_to_memory_bound() {
        // Below the ceiling: pass the adaptive cap through unchanged.
        assert_eq!(merkle_store_cap(8), 8);
        assert_eq!(merkle_store_cap(64), 64);
        // A configured `adaptive.max.store` above the ceiling must be clamped so
        // the whole-file fan-out can't pin more than ~256 MB of bodies (PR #137).
        assert_eq!(merkle_store_cap(512), MERKLE_STORE_MAX_IN_FLIGHT);
        assert_eq!(merkle_store_cap(usize::MAX), MERKLE_STORE_MAX_IN_FLIGHT);
        // Never zero — always make progress.
        assert_eq!(merkle_store_cap(0), 1);
    }

    #[test]
    fn distributed_sample_indices_spreads_across_large_file() {
        // cap 5 over 100 chunks: first and last included, evenly spread.
        assert_eq!(distributed_sample_indices(100, 5), vec![0, 24, 49, 74, 99]);
    }

    #[test]
    fn distributed_sample_indices_covers_whole_small_file() {
        // total <= cap returns every index, preserving the exact
        // "whole file sampled" detection in estimate_upload_cost.
        assert_eq!(distributed_sample_indices(3, 5), vec![0, 1, 2]);
        assert_eq!(distributed_sample_indices(5, 5), vec![0, 1, 2, 3, 4]);
    }

    /// The estimator bills the padded tree, and the leaf total it bills comes
    /// from the batches the payment path really builds — a `[255, 2]` split of
    /// 257 chunks, not a `[256, 1]` one that could never be paid.
    #[test]
    fn estimator_leaf_total_is_the_padded_payment_partition() {
        for chunks in [2u64, 64, 65, 100, 129, 255, 256, 257, 300, 512, 513, 769] {
            let from_partition: u64 = merkle_batch_sizes(chunks as usize)
                .into_iter()
                .map(|size| size.next_power_of_two() as u64)
                .sum();
            assert_eq!(
                merkle_billable_leaves(chunks),
                from_partition,
                "{chunks} chunks must be billed for the partition the payment path pays"
            );
        }
    }

    #[test]
    fn distributed_sample_indices_is_in_range_and_increasing() {
        assert!(distributed_sample_indices(0, 5).is_empty());
        assert_eq!(distributed_sample_indices(1, 5), vec![0]);
        for total in 1..200usize {
            let idx = distributed_sample_indices(total, 5);
            assert_eq!(*idx.first().unwrap(), 0);
            assert_eq!(*idx.last().unwrap(), total - 1);
            assert!(idx.iter().all(|&i| i < total));
            assert!(idx.windows(2).all(|w| w[0] < w[1]));
        }
    }

    #[test]
    fn disk_space_check_passes_for_small_file() {
        // A 1 KB file should always pass the disk space check
        check_disk_space_for_spill(1024).unwrap();
    }

    #[test]
    fn disk_space_check_fails_for_absurd_size() {
        // Requesting space for a 1 exabyte file should fail on any real system
        let result = check_disk_space_for_spill(u64::MAX / 2);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, Error::InsufficientDiskSpace(_)),
            "expected InsufficientDiskSpace, got: {err}"
        );
    }

    /// External multi-batch payment fold: winner-hash validation and
    /// paid/unpaid mixes (ADR-0003).
    mod external_merkle_fold {
        use super::*;
        use crate::data::client::merkle::test_support::{
            make_prepared_merkle_batch, winner_hash_for,
        };

        #[test]
        fn hash_count_mismatch_is_rejected() {
            let batches = vec![make_prepared_merkle_batch(2), make_prepared_merkle_batch(3)];
            let err = fold_external_merkle_payments(batches, vec![None]).unwrap_err();
            assert!(
                err.to_string().contains("winner pool hash entries"),
                "unexpected error: {err}"
            );
        }

        #[test]
        fn all_unpaid_is_rejected() {
            let batches = vec![make_prepared_merkle_batch(2)];
            let err = fold_external_merkle_payments(batches, vec![None]).unwrap_err();
            assert!(
                err.to_string().contains("No merkle sub-batch was paid"),
                "unexpected error: {err}"
            );
        }

        /// A k-of-N payment makes forward progress: the paid batch's proofs
        /// fold in, the unpaid batch contributes none — so the store phase
        /// reports its chunks via `PartialUpload` instead of aborting.
        #[test]
        fn paid_batches_fold_and_unpaid_contribute_no_proofs() {
            let paid = make_prepared_merkle_batch(2);
            let unpaid = make_prepared_merkle_batch(3);
            let winner = winner_hash_for(&paid);
            let merged =
                fold_external_merkle_payments(vec![paid, unpaid], vec![Some(winner), None])
                    .unwrap();
            assert_eq!(merged.proofs.len(), 2, "proofs cover only the paid batch");
            assert_eq!(merged.chunk_count, 2);
        }
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_tracks_fetch_headroom() {
        let batch_size = adaptive_stream_decrypt_batch_size(1_000, 64, 10, Some(u64::MAX));

        assert_eq!(batch_size, 64 * DOWNLOAD_STREAM_BATCH_FETCH_MULTIPLIER);
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_caps_to_total_chunks() {
        let batch_size = adaptive_stream_decrypt_batch_size(12, 64, 10, Some(u64::MAX));

        assert_eq!(batch_size, 12);
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_honours_configured_floor() {
        let batch_size = adaptive_stream_decrypt_batch_size(1_000, 1, 32, None);

        assert_eq!(batch_size, 32);
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_does_not_expand_without_memory_reading() {
        let batch_size = adaptive_stream_decrypt_batch_size(1_000, 64, 10, None);

        assert_eq!(batch_size, 10);
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_caps_to_memory_budget() {
        let estimated_bytes_per_chunk = (self_encryption::MAX_CHUNK_SIZE as u64)
            .saturating_mul(DOWNLOAD_STREAM_BATCH_BYTES_PER_CHUNK_MULTIPLIER)
            .max(1);
        let usable_memory = estimated_bytes_per_chunk
            .saturating_mul(16)
            .saturating_mul(DOWNLOAD_STREAM_BATCH_MEMORY_BUDGET_DIVISOR);
        let batch_size = adaptive_stream_decrypt_batch_size(1_000, 256, 10, Some(usable_memory));

        assert_eq!(batch_size, 16);
    }

    #[test]
    fn adaptive_stream_decrypt_batch_size_keeps_one_chunk_when_memory_is_tight() {
        let batch_size = adaptive_stream_decrypt_batch_size(1_000, 64, 10, Some(1));

        assert_eq!(batch_size, 1);
    }

    #[test]
    fn cached_merkle_covers_only_when_all_addresses_have_proofs() {
        let covered = compute_address(&Bytes::from_static(b"covered"));
        let extra = compute_address(&Bytes::from_static(b"extra"));
        let missing = compute_address(&Bytes::from_static(b"missing"));
        let cached = MerkleBatchPaymentResult {
            proofs: HashMap::from([(covered, vec![1]), (extra, vec![2])]),
            chunk_count: 2,
            storage_cost_atto: "0".to_string(),
            gas_cost_wei: 0,
            merkle_payment_timestamp: 0,
        };

        assert!(cached_merkle_covers_addresses(&cached, &[covered]));
        assert!(cached_merkle_covers_addresses(&cached, &[covered, extra]));
        assert!(!cached_merkle_covers_addresses(
            &cached,
            &[covered, missing]
        ));
    }

    /// A partial merkle payment leaves some addresses without a proof. Those
    /// must be split out so `upload_merkle_from_spill` reports them as failed
    /// (`PartialUpload`) instead of aborting the whole file — preserving the
    /// addresses' original order in each group.
    #[test]
    fn partition_addresses_by_proof_splits_paid_and_unpaid() {
        let paid_a = [1u8; 32];
        let unpaid_b = [2u8; 32];
        let paid_c = [3u8; 32];
        let unpaid_d = [4u8; 32];
        let proofs: HashMap<[u8; 32], Vec<u8>> =
            HashMap::from([(paid_a, vec![0xaa]), (paid_c, vec![0xcc])]);

        let (to_store, missing) =
            partition_addresses_by_proof(&[paid_a, unpaid_b, paid_c, unpaid_d], &proofs);

        assert_eq!(to_store, vec![paid_a, paid_c]);
        assert_eq!(missing, vec![unpaid_b, unpaid_d]);
    }

    /// A wave that returns `Ok` contributes its stored chunks, parsed cost, and
    /// stats; nothing is recorded as failed.
    #[test]
    fn fold_single_wave_keeps_ok_wave() {
        let stored = vec![[1u8; 32], [2u8; 32]];
        let stats = WaveAggregateStats {
            chunk_attempts_total: 7,
            ..Default::default()
        };

        let outcome = fold_single_wave(Ok((stored.clone(), "100".to_string(), 9, stats))).unwrap();

        assert_eq!(outcome.stored, stored);
        assert!(outcome.failed.is_empty());
        assert_eq!(outcome.storage_atto.to_string(), "100");
        assert_eq!(outcome.gas_wei, 9);
        assert_eq!(outcome.stats.chunk_attempts_total, 7);
    }

    /// The core V2-461 semantic: a wave short of quorum (`PartialUpload`) is
    /// recoverable — its stored chunks, failed chunks, and on-chain spend are
    /// folded so the caller can continue to the next wave rather than aborting
    /// the whole file.
    #[test]
    fn fold_single_wave_folds_partial_upload() {
        let stored = vec![[3u8; 32]];
        let failed = vec![([4u8; 32], "short of quorum".to_string())];
        let err = Error::PartialUpload {
            stored: stored.clone(),
            stored_count: 1,
            failed: failed.clone(),
            failed_count: 1,
            total_chunks: 2,
            spend: Box::new(PartialUploadSpend {
                storage_cost_atto: "250".to_string(),
                gas_cost_wei: 11,
            }),
            reason: "wave store failed after retries".to_string(),
        };

        let outcome = fold_single_wave(Err(err)).unwrap();

        assert_eq!(outcome.stored, stored);
        assert_eq!(outcome.failed, failed);
        assert_eq!(outcome.storage_atto.to_string(), "250");
        assert_eq!(outcome.gas_wei, 11);
        // `PartialUpload` carries no stats, so the failed wave contributes none.
        assert_eq!(outcome.stats.chunk_attempts_total, 0);
    }

    /// A non-`PartialUpload` error (wallet/payment-infrastructure failure) is
    /// fatal and must abort the file, not be folded into the failed set.
    #[test]
    fn fold_single_wave_propagates_fatal_error() {
        let result = fold_single_wave(Err(Error::Payment("wallet unavailable".to_string())));

        assert!(
            matches!(result, Err(Error::Payment(_))),
            "fatal payment error must propagate, got: {result:?}"
        );
    }

    #[test]
    fn partition_addresses_by_proof_handles_all_or_nothing() {
        let a = [5u8; 32];
        let b = [6u8; 32];

        // No proofs at all → every address is missing.
        let empty: HashMap<[u8; 32], Vec<u8>> = HashMap::new();
        let (to_store, missing) = partition_addresses_by_proof(&[a, b], &empty);
        assert!(to_store.is_empty());
        assert_eq!(missing, vec![a, b]);

        // All proofs present → nothing missing.
        let full: HashMap<[u8; 32], Vec<u8>> = HashMap::from([(a, vec![1]), (b, vec![2])]);
        let (to_store, missing) = partition_addresses_by_proof(&[a, b], &full);
        assert_eq!(to_store, vec![a, b]);
        assert!(missing.is_empty());
    }

    #[test]
    fn chunk_spill_round_trip() {
        let mut spill = ChunkSpill::new().unwrap();
        let data1 = vec![0xAA; 1024];
        let data2 = vec![0xBB; 2048];

        spill.push(&data1).unwrap();
        spill.push(&data2).unwrap();

        assert_eq!(spill.len(), 2);
        assert_eq!(spill.total_bytes(), 1024 + 2048);
        let chunk_entries = spill.chunk_entries().unwrap();
        let entry_total: u64 = chunk_entries.iter().map(|(_, size)| *size).sum();
        assert_eq!(entry_total, 1024 + 2048);

        // Read back and verify
        let chunk1 = spill.read_chunk(spill.addresses.first().unwrap()).unwrap();
        assert_eq!(&chunk1[..], &data1[..]);

        let chunk2 = spill.read_chunk(spill.addresses.get(1).unwrap()).unwrap();
        assert_eq!(&chunk2[..], &data2[..]);

        // Verify waves with 1-chunk wave size
        let waves: Vec<_> = spill.addresses.chunks(1).collect();
        assert_eq!(waves.len(), 2);
    }

    #[test]
    fn chunk_spill_cleanup_on_drop() {
        let dir;
        {
            let spill = ChunkSpill::new().unwrap();
            dir = spill.dir.clone();
            assert!(dir.exists());
        }
        // After drop, the directory should be cleaned up
        assert!(!dir.exists(), "spill dir should be removed on drop");
    }

    #[test]
    fn chunk_spill_deduplicates_identical_content() {
        let mut spill = ChunkSpill::new().unwrap();
        let data = vec![0xCC; 512];

        spill.push(&data).unwrap();
        spill.push(&data).unwrap(); // same content, should be skipped
        spill.push(&data).unwrap(); // again

        assert_eq!(spill.len(), 1, "duplicate chunks should be deduplicated");
        assert_eq!(
            spill.total_bytes(),
            512,
            "total_bytes should count unique only"
        );

        // Different content should still be added
        let data2 = vec![0xDD; 256];
        spill.push(&data2).unwrap();
        assert_eq!(spill.len(), 2);
        assert_eq!(spill.total_bytes(), 512 + 256);
    }
}

/// Compile-time assertions that Client file method futures are Send.
#[cfg(test)]
mod send_assertions {
    use super::*;

    fn _assert_send<T: Send>(_: &T) {}

    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn _file_upload_is_send(client: &Client) {
        let fut = client.file_upload(Path::new("/dev/null"));
        _assert_send(&fut);
    }

    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn _file_upload_with_mode_is_send(client: &Client) {
        let fut = client.file_upload_with_mode(Path::new("/dev/null"), PaymentMode::Auto);
        _assert_send(&fut);
    }

    #[allow(
        dead_code,
        unreachable_code,
        unused_variables,
        clippy::diverging_sub_expression
    )]
    async fn _file_download_is_send(client: &Client) {
        let dm: DataMap = todo!();
        let fut = client.file_download(&dm, Path::new("/dev/null"));
        _assert_send(&fut);
    }
}
