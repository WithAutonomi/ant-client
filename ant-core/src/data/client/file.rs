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

use crate::data::client::batch::{finalize_batch_payment, PaymentIntent, PreparedChunk};
use crate::data::client::merkle::{
    finalize_merkle_batch, should_use_merkle, MerkleBatchPaymentResult, PaymentMode,
    PreparedMerkleBatch,
};
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{QuoteHash, TxHash};
use ant_protocol::{compute_address, DATA_TYPE_CHUNK};
use bytes::Bytes;
use fs2::FileExt;
use futures::stream::{self, StreamExt};
use self_encryption::{get_root_data_map_parallel, stream_encrypt, streaming_decrypt, DataMap};
use std::collections::{HashMap, HashSet};
use std::io::Write;
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
    /// A wave has completed.
    WaveComplete {
        wave: usize,
        total_waves: usize,
        stored_so_far: usize,
        total: usize,
    },
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

/// Number of chunks per upload wave (matches batch.rs PAYMENT_WAVE_SIZE).
const UPLOAD_WAVE_SIZE: usize = 64;

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
/// Maximum age (in seconds) for orphaned spill directories.
/// Dirs older than this are cleaned up if they have no active lockfile.
const SPILL_MAX_AGE_SECS: u64 = 24 * 60 * 60; // 24 hours

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
            total_bytes: 0,
        })
    }

    /// Clean up stale spill directories. Best-effort, errors are logged.
    ///
    /// Only removes directories that:
    /// 1. Start with `SPILL_DIR_PREFIX` (ignores unrelated files)
    /// 2. Are actual directories (not symlinks -- prevents symlink attacks)
    /// 3. Have a timestamp older than `SPILL_MAX_AGE_SECS`
    /// 4. Do NOT have an active lockfile (prevents deleting in-progress uploads)
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

            if now.saturating_sub(timestamp) <= SPILL_MAX_AGE_SECS {
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
        self.total_bytes += content.len() as u64;
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

    /// Average chunk size in bytes (for quoting metrics).
    fn avg_chunk_size(&self) -> u64 {
        if self.addresses.is_empty() {
            return 0;
        }
        self.total_bytes / self.addresses.len() as u64
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

    /// Iterate over address slices in wave-sized groups.
    fn waves(&self) -> std::slice::Chunks<'_, [u8; 32]> {
        self.addresses.chunks(UPLOAD_WAVE_SIZE)
    }

    /// Read a wave of chunks from disk.
    fn read_wave(&self, wave_addrs: &[[u8; 32]]) -> Result<Vec<(Bytes, [u8; 32])>> {
        let mut out = Vec::with_capacity(wave_addrs.len());
        for addr in wave_addrs {
            let content = self.read_chunk(addr)?;
            out.push((content, *addr));
        }
        Ok(out)
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

/// Result of a file upload: the `DataMap` needed to retrieve the file.
#[derive(Debug, Clone)]
pub struct FileUploadResult {
    /// The data map containing chunk metadata for reconstruction.
    pub data_map: DataMap,
    /// Number of chunks stored on the network.
    pub chunks_stored: usize,
    /// Which payment mode was actually used (not just requested).
    pub payment_mode_used: PaymentMode,
    /// Total storage cost paid in token units (atto). "0" if all chunks already existed.
    pub storage_cost_atto: String,
    /// Total gas cost in wei. 0 if no on-chain transactions were made.
    pub gas_cost_wei: u128,
}

/// Payment information for external signing — either wave-batch or merkle.
#[derive(Debug)]
pub enum ExternalPaymentInfo {
    /// Wave-batch: individual (quote_hash, rewards_address, amount) tuples.
    WaveBatch {
        /// Chunks ready for payment (needed for finalize).
        prepared_chunks: Vec<PreparedChunk>,
        /// Payment intent for external signing.
        payment_intent: PaymentIntent,
    },
    /// Merkle: single on-chain call with depth, pool commitments, timestamp.
    Merkle {
        /// The prepared merkle batch (public fields sent to frontend, private fields stay in Rust).
        prepared_batch: PreparedMerkleBatch,
        /// Raw chunk contents (needed for upload after payment).
        chunk_contents: Vec<Bytes>,
        /// Chunk addresses in order (needed for upload after payment).
        chunk_addresses: Vec<[u8; 32]>,
    },
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
#[derive(Debug)]
pub struct PreparedUpload {
    /// The data map for later retrieval.
    pub data_map: DataMap,
    /// Payment information — either wave-batch or merkle depending on chunk count.
    pub payment_info: ExternalPaymentInfo,
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

    /// Phase 1 of external-signer upload: encrypt file and prepare chunks.
    ///
    /// Requires an EVM network (for contract price queries) but NOT a wallet.
    /// Returns a [`PreparedUpload`] containing the data map, prepared chunks,
    /// and a [`PaymentIntent`] that the external signer uses to construct
    /// and submit the on-chain payment transaction.
    ///
    /// **Memory note:** Encryption uses disk spilling for bounded memory, but
    /// the returned [`PreparedUpload`] holds all chunk content in memory (each
    /// [`PreparedChunk`] contains a `Bytes` with the full chunk data). This is
    /// inherent to the two-phase external-signer protocol — the chunks must
    /// stay in memory until [`Client::finalize_upload`] stores them. For very
    /// large files, prefer [`Client::file_upload`] which streams directly.
    ///
    /// # Errors
    ///
    /// Returns an error if there is insufficient disk space, the file cannot
    /// be read, encryption fails, or quote collection fails.
    pub async fn file_prepare_upload(&self, path: &Path) -> Result<PreparedUpload> {
        debug!(
            "Preparing file upload for external signing: {}",
            path.display()
        );

        let file_size = std::fs::metadata(path)?.len();
        check_disk_space_for_spill(file_size)?;

        let (spill, data_map) = self.encrypt_file_to_spill(path, None).await?;

        info!(
            "Encrypted {} into {} chunks for external signing (spilled to disk)",
            path.display(),
            spill.len()
        );

        // Read each chunk from disk and collect quotes concurrently.
        // Note: all PreparedChunks accumulate in memory because the external-signer
        // protocol requires them for finalize_upload. NOT memory-bounded for large files.
        let chunk_data: Vec<Bytes> = spill
            .addresses
            .iter()
            .map(|addr| spill.read_chunk(addr))
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let chunk_count = chunk_data.len();

        let payment_info = if should_use_merkle(chunk_count, PaymentMode::Auto) {
            // Merkle path: build tree, collect candidate pools, return for external payment.
            info!("Using merkle batch preparation for {chunk_count} file chunks");

            let addresses: Vec<[u8; 32]> = chunk_data.iter().map(|c| compute_address(c)).collect();

            let avg_size =
                chunk_data.iter().map(bytes::Bytes::len).sum::<usize>() / chunk_count.max(1);
            let avg_size_u64 = u64::try_from(avg_size).unwrap_or(0);

            let prepared_batch = self
                .prepare_merkle_batch_external(&addresses, DATA_TYPE_CHUNK, avg_size_u64)
                .await?;

            info!(
                "File prepared for external merkle signing: {} chunks, depth={} ({})",
                chunk_count,
                prepared_batch.depth,
                path.display()
            );

            ExternalPaymentInfo::Merkle {
                prepared_batch,
                chunk_contents: chunk_data,
                chunk_addresses: addresses,
            }
        } else {
            // Wave-batch path: collect quotes per chunk concurrently.
            let quote_concurrency = self.config().quote_concurrency;
            let results: Vec<Result<Option<PreparedChunk>>> = stream::iter(chunk_data)
                .map(|content| async move { self.prepare_chunk_payment(content).await })
                .buffer_unordered(quote_concurrency)
                .collect()
                .await;

            let mut prepared_chunks = Vec::with_capacity(spill.len());
            for result in results {
                if let Some(prepared) = result? {
                    prepared_chunks.push(prepared);
                }
            }

            let payment_intent = PaymentIntent::from_prepared_chunks(&prepared_chunks);

            info!(
                "File prepared for external signing: {} chunks, total {} atto ({})",
                prepared_chunks.len(),
                payment_intent.total_amount,
                path.display()
            );

            ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent,
            }
        };

        Ok(PreparedUpload {
            data_map,
            payment_info,
        })
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
        match prepared.payment_info {
            ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent: _,
            } => {
                let paid_chunks = finalize_batch_payment(prepared_chunks, tx_hash_map)?;
                let wave_result = self.store_paid_chunks(paid_chunks).await;
                if !wave_result.failed.is_empty() {
                    let failed_count = wave_result.failed.len();
                    return Err(Error::PartialUpload {
                        stored: wave_result.stored.clone(),
                        stored_count: wave_result.stored.len(),
                        failed: wave_result.failed,
                        failed_count,
                        reason: "finalize_upload: chunk storage failed after retries".into(),
                    });
                }
                let chunks_stored = wave_result.stored.len();

                info!("External-signer upload finalized: {chunks_stored} chunks stored");

                Ok(FileUploadResult {
                    data_map: prepared.data_map,
                    chunks_stored,
                    payment_mode_used: PaymentMode::Single,
                    storage_cost_atto: "0".into(),
                    gas_cost_wei: 0,
                })
            }
            ExternalPaymentInfo::Merkle { .. } => Err(Error::Payment(
                "Cannot finalize merkle upload with wave-batch tx hashes. \
                 Use finalize_upload_merkle() instead."
                    .to_string(),
            )),
        }
    }

    /// Phase 2 of external-signer upload (merkle): finalize with winner pool hash.
    ///
    /// Takes a [`PreparedUpload`] that used merkle payment and the `winner_pool_hash`
    /// returned by the on-chain merkle payment transaction. Generates proofs and
    /// stores chunks on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if the prepared upload used wave-batch payment (use
    /// [`Client::finalize_upload`] instead), proof generation fails,
    /// or any chunk cannot be stored.
    pub async fn finalize_upload_merkle(
        &self,
        prepared: PreparedUpload,
        winner_pool_hash: [u8; 32],
    ) -> Result<FileUploadResult> {
        match prepared.payment_info {
            ExternalPaymentInfo::Merkle {
                prepared_batch,
                chunk_contents,
                chunk_addresses,
            } => {
                let batch_result = finalize_merkle_batch(prepared_batch, winner_pool_hash)?;
                let chunks_stored = self
                    .merkle_upload_chunks(chunk_contents, chunk_addresses, &batch_result)
                    .await?;

                info!("External-signer merkle upload finalized: {chunks_stored} chunks stored");

                Ok(FileUploadResult {
                    data_map: prepared.data_map,
                    chunks_stored,
                    payment_mode_used: PaymentMode::Merkle,
                    storage_cost_atto: "0".into(),
                    gas_cost_wei: 0,
                })
            }
            ExternalPaymentInfo::WaveBatch { .. } => Err(Error::Payment(
                "Cannot finalize wave-batch upload with merkle winner hash. \
                 Use finalize_upload() instead."
                    .to_string(),
            )),
        }
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
        debug!(
            "Streaming file upload with mode {mode:?}: {}",
            path.display()
        );

        // Pre-flight: verify enough temp disk space for the chunk spill.
        let file_size = std::fs::metadata(path)?.len();
        check_disk_space_for_spill(file_size)?;

        // Phase 1: Encrypt file and spill chunks to temp directory.
        // Only 32-byte addresses stay in memory — chunk data lives on disk.
        let (spill, data_map) = self.encrypt_file_to_spill(path, progress.as_ref()).await?;

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
        let (chunks_stored, actual_mode, storage_cost_atto, gas_cost_wei) =
            if self.should_use_merkle(chunk_count, mode) {
                info!("Using merkle batch payment for {chunk_count} file chunks");

                let batch_result = match self
                    .pay_for_merkle_batch(&spill.addresses, DATA_TYPE_CHUNK, spill.avg_chunk_size())
                    .await
                {
                    Ok(result) => result,
                    Err(Error::InsufficientPeers(ref msg)) if mode == PaymentMode::Auto => {
                        info!("Merkle needs more peers ({msg}), falling back to wave-batch");
                        let (stored, sc, gc) =
                            self.upload_waves_single(&spill, progress.as_ref()).await?;
                        return Ok(FileUploadResult {
                            data_map,
                            chunks_stored: stored,
                            payment_mode_used: PaymentMode::Single,
                            storage_cost_atto: sc,
                            gas_cost_wei: gc,
                        });
                    }
                    Err(e) => return Err(e),
                };

                let (stored, sc, gc) = self
                    .upload_waves_merkle(&spill, &batch_result, progress.as_ref())
                    .await?;
                (stored, PaymentMode::Merkle, sc, gc)
            } else {
                let (stored, sc, gc) = self.upload_waves_single(&spill, progress.as_ref()).await?;
                (stored, PaymentMode::Single, sc, gc)
            };

        info!(
            "File uploaded with {actual_mode:?}: {chunks_stored} chunks stored ({})",
            path.display()
        );

        Ok(FileUploadResult {
            data_map,
            chunks_stored,
            payment_mode_used: actual_mode,
            storage_cost_atto,
            gas_cost_wei,
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
    ) -> Result<(usize, String, u128)> {
        let mut total_stored = 0usize;
        let mut total_storage = ant_protocol::evm::Amount::ZERO;
        let mut total_gas: u128 = 0;
        let total_chunks = spill.len();
        let waves: Vec<&[[u8; 32]]> = spill.waves().collect();
        let wave_count = waves.len();

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
            let (addresses, wave_storage, wave_gas) = self
                .batch_upload_chunks_with_events(wave_data, progress, total_stored, total_chunks)
                .await?;
            total_stored += addresses.len();
            if let Ok(cost) = wave_storage.parse::<ant_protocol::evm::Amount>() {
                total_storage += cost;
            }
            total_gas = total_gas.saturating_add(wave_gas);
            if let Some(tx) = progress {
                let _ = tx
                    .send(UploadEvent::WaveComplete {
                        wave: wave_num,
                        total_waves: wave_count,
                        stored_so_far: total_stored,
                        total: total_chunks,
                    })
                    .await;
            }
        }

        Ok((total_stored, total_storage.to_string(), total_gas))
    }

    /// Upload chunks from a spill using pre-computed merkle proofs.
    ///
    /// Reads one wave at a time from disk, pairs each chunk with its proof,
    /// and uploads concurrently. Peak memory: ~`UPLOAD_WAVE_SIZE × MAX_CHUNK_SIZE`.
    ///
    /// Returns `(chunks_stored, storage_cost_atto, gas_cost_wei)`.
    /// Costs come from the `batch_result` which was populated during payment.
    async fn upload_waves_merkle(
        &self,
        spill: &ChunkSpill,
        batch_result: &MerkleBatchPaymentResult,
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> Result<(usize, String, u128)> {
        let mut total_stored = 0usize;
        let total_chunks = spill.len();
        let waves: Vec<&[[u8; 32]]> = spill.waves().collect();
        let wave_count = waves.len();

        for (wave_idx, wave_addrs) in waves.into_iter().enumerate() {
            let wave_num = wave_idx + 1;
            let wave = spill.read_wave(wave_addrs)?;

            info!(
                "Wave {wave_num}/{wave_count}: storing {} chunks (merkle) — {total_stored}/{total_chunks} stored so far",
                wave.len()
            );

            let mut upload_stream = stream::iter(wave.into_iter().map(|(content, addr)| {
                let proof_bytes = batch_result.proofs.get(&addr).cloned();
                async move {
                    let proof = proof_bytes.ok_or_else(|| {
                        Error::Payment(format!(
                            "Missing merkle proof for chunk {}",
                            hex::encode(addr)
                        ))
                    })?;
                    let peers = self.close_group_peers(&addr).await?;
                    self.chunk_put_to_close_group(content, proof, &peers).await
                }
            }))
            .buffer_unordered(self.config().store_concurrency);

            while let Some(result) = upload_stream.next().await {
                result?;
                total_stored += 1;
                info!("Stored {total_stored}/{total_chunks}");
                if let Some(tx) = progress {
                    let _ = tx
                        .send(UploadEvent::ChunkStored {
                            stored: total_stored,
                            total: total_chunks,
                        })
                        .await;
                }
            }

            if let Some(tx) = progress {
                let _ = tx
                    .send(UploadEvent::WaveComplete {
                        wave: wave_num,
                        total_waves: wave_count,
                        stored_so_far: total_stored,
                        total: total_chunks,
                    })
                    .await;
            }
        }

        Ok((
            total_stored,
            batch_result.storage_cost_atto.clone(),
            batch_result.gas_cost_wei,
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
    #[allow(clippy::unused_async)]
    pub async fn file_download(&self, data_map: &DataMap, output: &Path) -> Result<u64> {
        self.file_download_with_progress(data_map, output, None)
            .await
    }

    /// Download and decrypt a file with progress events.
    ///
    /// Same as [`Client::file_download`] but sends [`DownloadEvent`]s for UI feedback.
    ///
    /// Progress reporting:
    /// 1. Resolves hierarchical DataMaps to the root level first (reports as
    ///    `ChunksFetched` with `total: 0` during resolution)
    /// 2. Once the root DataMap is known, sends `total_chunks` with accurate count
    /// 3. Fetches data chunks with accurate `fetched/total` progress
    #[allow(clippy::unused_async)]
    pub async fn file_download_with_progress(
        &self,
        data_map: &DataMap,
        output: &Path,
        progress: Option<mpsc::Sender<DownloadEvent>>,
    ) -> Result<u64> {
        debug!("Downloading file to {}", output.display());

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
                let fetch = |batch: &[(usize, XorName)]| {
                    let batch_owned: Vec<(usize, XorName)> = batch.to_vec();
                    let counter = counter_ref.clone();
                    let prog = progress_ref.clone();
                    handle.block_on(async {
                        let mut futs = futures::stream::FuturesUnordered::new();
                        for (idx, hash) in batch_owned {
                            let addr = hash.0;
                            futs.push(async move {
                                let result = self.chunk_get(&addr).await;
                                (idx, hash, result)
                            });
                        }
                        let mut results = Vec::with_capacity(futs.len());
                        while let Some((idx, hash, result)) =
                            futures::StreamExt::next(&mut futs).await
                        {
                            let chunk = result
                                .map_err(|e| {
                                    self_encryption::Error::Generic(format!(
                                        "DataMap resolution failed: {e}"
                                    ))
                                })?
                                .ok_or_else(|| {
                                    self_encryption::Error::Generic(format!(
                                        "DataMap chunk not found: {}",
                                        hex::encode(hash.0)
                                    ))
                                })?;
                            results.push((idx, chunk.content));
                            let fetched =
                                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                            if let Some(ref tx) = prog {
                                let _ = tx.try_send(DownloadEvent::MapChunkFetched { fetched });
                            }
                        }
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

        let stream = streaming_decrypt(&root_map, |batch: &[(usize, XorName)]| {
            let batch_owned: Vec<(usize, XorName)> = batch.to_vec();
            let fetched_ref = fetched_for_closure.clone();
            let progress_ref = progress_for_closure.clone();

            tokio::task::block_in_place(|| {
                handle.block_on(async {
                    let mut futs = futures::stream::FuturesUnordered::new();
                    for (idx, hash) in batch_owned {
                        let addr = hash.0;
                        futs.push(async move {
                            let result = self.chunk_get(&addr).await;
                            (idx, hash, result)
                        });
                    }

                    let mut results = Vec::with_capacity(futs.len());
                    while let Some((idx, hash, result)) = futures::StreamExt::next(&mut futs).await
                    {
                        let addr_hex = hex::encode(hash.0);
                        let chunk = result
                            .map_err(|e| {
                                self_encryption::Error::Generic(format!(
                                    "Network fetch failed for {addr_hex}: {e}"
                                ))
                            })?
                            .ok_or_else(|| {
                                self_encryption::Error::Generic(format!(
                                    "Chunk not found: {addr_hex}"
                                ))
                            })?;
                        results.push((idx, chunk.content));
                        let fetched =
                            fetched_ref.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                        info!("Downloaded {fetched}/{total_chunks}");
                        if let Some(ref tx) = progress_ref {
                            let _ = tx.try_send(DownloadEvent::ChunksFetched {
                                fetched,
                                total: total_chunks,
                            });
                        }
                    }
                    Ok(results)
                })
            })
        })
        .map_err(|e| Error::Encryption(format!("streaming decrypt failed: {e}")))?;

        // Write decrypted chunks to a temp file, then rename atomically.
        let parent = output.parent().unwrap_or_else(|| Path::new("."));
        let unique: u64 = rand::random();
        let tmp_path = parent.join(format!(".ant_download_{}_{unique}.tmp", std::process::id()));

        let write_result = (|| -> Result<u64> {
            let mut file = std::fs::File::create(&tmp_path)?;
            let mut bytes_written = 0u64;
            for chunk_result in stream {
                let chunk_bytes = chunk_result
                    .map_err(|e| Error::Encryption(format!("decryption failed: {e}")))?;
                file.write_all(&chunk_bytes)?;
                bytes_written += chunk_bytes.len() as u64;
            }
            file.flush()?;
            Ok(bytes_written)
        })();

        match write_result {
            Ok(bytes_written) => match std::fs::rename(&tmp_path, output) {
                Ok(()) => {
                    info!(
                        "File downloaded: {bytes_written} bytes written to {}",
                        output.display()
                    );
                    Ok(bytes_written)
                }
                Err(rename_err) => {
                    if let Err(cleanup_err) = std::fs::remove_file(&tmp_path) {
                        warn!(
                            "Failed to remove temp download file {}: {cleanup_err}",
                            tmp_path.display()
                        );
                    }
                    Err(rename_err.into())
                }
            },
            Err(e) => {
                if let Err(cleanup_err) = std::fs::remove_file(&tmp_path) {
                    warn!(
                        "Failed to remove temp download file {}: {cleanup_err}",
                        tmp_path.display()
                    );
                }
                Err(e)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

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

    #[test]
    fn chunk_spill_round_trip() {
        let mut spill = ChunkSpill::new().unwrap();
        let data1 = vec![0xAA; 1024];
        let data2 = vec![0xBB; 2048];

        spill.push(&data1).unwrap();
        spill.push(&data2).unwrap();

        assert_eq!(spill.len(), 2);
        assert_eq!(spill.total_bytes(), 1024 + 2048);
        assert_eq!(spill.avg_chunk_size(), (1024 + 2048) / 2);

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
