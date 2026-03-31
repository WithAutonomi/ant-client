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
use crate::data::client::merkle::{MerkleBatchPaymentResult, PaymentMode};
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_evm::QuoteHash;
use ant_node::ant_protocol::DATA_TYPE_CHUNK;
use ant_node::client::compute_address;
use bytes::Bytes;
use evmlib::common::TxHash;
use futures::stream::{self, StreamExt};
use self_encryption::{stream_encrypt, streaming_decrypt, DataMap};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};
use xor_name::XorName;

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
struct ChunkSpill {
    /// Temp directory holding spilled chunk files (named by hex address).
    dir: PathBuf,
    /// Ordered list of chunk addresses (preserves encryption order).
    addresses: Vec<[u8; 32]>,
    /// Running total of chunk byte sizes (for average-size calculation).
    total_bytes: u64,
}

impl ChunkSpill {
    /// Create a new spill directory under the system temp dir.
    fn new() -> Result<Self> {
        let unique: u64 = rand::random();
        let dir = std::env::temp_dir().join(format!(".ant_spill_{}_{unique}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            addresses: Vec::new(),
            total_bytes: 0,
        })
    }

    /// Write one encrypted chunk to disk and record its address.
    fn push(&mut self, content: &[u8]) -> Result<()> {
        let address = compute_address(content);
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

/// Check that the temp directory has enough free space for the spilled chunks.
///
/// `file_size` is the source file's byte count. We require
/// `file_size + 10%` free space in the temp dir to account for
/// self-encryption overhead.
fn check_disk_space_for_spill(file_size: u64) -> Result<()> {
    let tmp = std::env::temp_dir();
    let available = fs2::available_space(&tmp).map_err(|e| {
        Error::Io(std::io::Error::new(
            e.kind(),
            format!("failed to query disk space on {}: {e}", tmp.display()),
        ))
    })?;

    // Use integer arithmetic to avoid f64 precision loss on large file sizes.
    let headroom = file_size / DISK_SPACE_HEADROOM_PERCENT;
    let required = file_size.saturating_add(headroom);

    if available < required {
        let avail_mb = available / (1024 * 1024);
        let req_mb = required / (1024 * 1024);
        return Err(Error::InsufficientDiskSpace(format!(
            "need ~{req_mb} MB in temp dir ({}) but only {avail_mb} MB available",
            tmp.display()
        )));
    }

    debug!(
        "Disk space check passed: {available} bytes available, {required} bytes required (temp: {})",
        tmp.display()
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
}

/// Prepared upload ready for external payment.
///
/// Contains everything needed to construct the on-chain payment transaction
/// externally (e.g. via WalletConnect in a desktop app) and then finalize
/// the upload without a Rust-side wallet.
///
/// Note: This struct stays in Rust memory — only `payment_intent` is sent
/// to the frontend. `PreparedChunk` contains non-serializable network types,
/// so the full struct cannot derive `Serialize`.
#[derive(Debug)]
pub struct PreparedUpload {
    /// The data map for later retrieval.
    pub data_map: DataMap,
    /// Chunks ready for payment.
    pub prepared_chunks: Vec<PreparedChunk>,
    /// Payment intent for external signing.
    pub payment_intent: PaymentIntent,
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

        let (spill, data_map) = self.encrypt_file_to_spill(path).await?;

        info!(
            "Encrypted {} into {} chunks for external signing (spilled to disk)",
            path.display(),
            spill.len()
        );

        // Read each chunk from disk and collect quotes. Note: all PreparedChunks
        // accumulate in memory because the external-signer protocol requires them
        // for finalize_upload. This is NOT memory-bounded for large files.
        let mut prepared_chunks = Vec::with_capacity(spill.len());
        for (i, addr) in spill.addresses.iter().enumerate() {
            let content = spill.read_chunk(addr)?;
            if let Some(prepared) = self.prepare_chunk_payment(content).await? {
                prepared_chunks.push(prepared);
            }
            if (i + 1) % 100 == 0 {
                info!(
                    "Prepared {}/{} chunks for external signing",
                    i + 1,
                    spill.len()
                );
            }
        }

        let payment_intent = PaymentIntent::from_prepared_chunks(&prepared_chunks);

        info!(
            "File prepared for external signing: {} chunks, total {} atto ({})",
            prepared_chunks.len(),
            payment_intent.total_amount,
            path.display()
        );

        Ok(PreparedUpload {
            data_map,
            prepared_chunks,
            payment_intent,
        })
    }

    /// Phase 2 of external-signer upload: finalize with externally-signed tx hashes.
    ///
    /// Takes a [`PreparedUpload`] from [`Client::file_prepare_upload`] and a map
    /// of `quote_hash -> tx_hash` provided by the external signer after on-chain
    /// payment. Builds payment proofs and stores chunks on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if proof construction fails or any chunk cannot be stored.
    pub async fn finalize_upload(
        &self,
        prepared: PreparedUpload,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
    ) -> Result<FileUploadResult> {
        let paid_chunks = finalize_batch_payment(prepared.prepared_chunks, tx_hash_map)?;
        let chunks_stored = self.store_paid_chunks(paid_chunks).await?.len();

        info!("External-signer upload finalized: {chunks_stored} chunks stored");

        Ok(FileUploadResult {
            data_map: prepared.data_map,
            chunks_stored,
            payment_mode_used: PaymentMode::Single,
        })
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
        debug!(
            "Streaming file upload with mode {mode:?}: {}",
            path.display()
        );

        // Pre-flight: verify enough temp disk space for the chunk spill.
        let file_size = std::fs::metadata(path)?.len();
        check_disk_space_for_spill(file_size)?;

        // Phase 1: Encrypt file and spill chunks to temp directory.
        // Only 32-byte addresses stay in memory — chunk data lives on disk.
        let (spill, data_map) = self.encrypt_file_to_spill(path).await?;

        let chunk_count = spill.len();
        info!(
            "Encrypted {} into {chunk_count} chunks (spilled to disk)",
            path.display()
        );

        // Phase 2: Decide payment mode and upload in waves from disk.
        //
        // Note on merkle proof memory: MerkleBatchPaymentResult.proofs holds all
        // proofs simultaneously (~86 KB each due to ML-DSA-65 signatures in the
        // candidate pool). For a 100 GB file (~25k chunks), this is ~2 GB. This
        // is acceptable because merkle payments save significant gas costs — the
        // gas savings far outweigh the proof memory overhead.
        let (chunks_stored, actual_mode) = if self.should_use_merkle(chunk_count, mode) {
            // Merkle batch payment path — needs all addresses upfront for tree.
            info!("Using merkle batch payment for {chunk_count} file chunks");

            let batch_result = match self
                .pay_for_merkle_batch(&spill.addresses, DATA_TYPE_CHUNK, spill.avg_chunk_size())
                .await
            {
                Ok(result) => result,
                Err(Error::InsufficientPeers(ref msg)) if mode == PaymentMode::Auto => {
                    info!("Merkle needs more peers ({msg}), falling back to wave-batch");
                    let stored = self.upload_waves_single(&spill).await?;
                    return Ok(FileUploadResult {
                        data_map,
                        chunks_stored: stored,
                        payment_mode_used: PaymentMode::Single,
                    });
                }
                Err(e) => return Err(e),
            };

            let stored = self.upload_waves_merkle(&spill, &batch_result).await?;
            (stored, PaymentMode::Merkle)
        } else {
            // Wave-based per-chunk payment path.
            let stored = self.upload_waves_single(&spill).await?;
            (stored, PaymentMode::Single)
        };

        info!(
            "File uploaded with {actual_mode:?}: {chunks_stored} chunks stored ({})",
            path.display()
        );

        Ok(FileUploadResult {
            data_map,
            chunks_stored,
            payment_mode_used: actual_mode,
        })
    }

    /// Encrypt a file and spill chunks to a temp directory.
    ///
    /// Logs progress every 100 chunks so users get feedback during
    /// multi-GB encryptions.
    ///
    /// Returns the spill buffer (addresses on disk) and the `DataMap`.
    async fn encrypt_file_to_spill(&self, path: &Path) -> Result<(ChunkSpill, DataMap)> {
        let (mut chunk_rx, datamap_rx, handle) = spawn_file_encryption(path.to_path_buf())?;

        let mut spill = ChunkSpill::new()?;
        while let Some(content) = chunk_rx.recv().await {
            spill.push(&content)?;
            if spill.len() % 100 == 0 {
                let mb = spill.total_bytes() / (1024 * 1024);
                info!(
                    "Encryption progress: {} chunks spilled ({mb} MB) — {}",
                    spill.len(),
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
    async fn upload_waves_single(&self, spill: &ChunkSpill) -> Result<usize> {
        let mut total_stored = 0usize;
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
                "Wave {wave_num}/{wave_count}: uploading {} chunks (single payment) — {total_stored}/{total_chunks} stored so far",
                wave_data.len()
            );
            let addresses = self.batch_upload_chunks(wave_data).await?;
            total_stored += addresses.len();
        }

        Ok(total_stored)
    }

    /// Upload chunks from a spill using pre-computed merkle proofs.
    ///
    /// Reads one wave at a time from disk, pairs each chunk with its proof,
    /// and uploads concurrently. Peak memory: ~`UPLOAD_WAVE_SIZE × MAX_CHUNK_SIZE`.
    async fn upload_waves_merkle(
        &self,
        spill: &ChunkSpill,
        batch_result: &MerkleBatchPaymentResult,
    ) -> Result<usize> {
        let mut total_stored = 0usize;
        let total_chunks = spill.len();
        let waves: Vec<&[[u8; 32]]> = spill.waves().collect();
        let wave_count = waves.len();

        for (wave_idx, wave_addrs) in waves.into_iter().enumerate() {
            let wave_num = wave_idx + 1;
            let wave = spill.read_wave(wave_addrs)?;

            info!(
                "Wave {wave_num}/{wave_count}: uploading {} chunks (merkle) — {total_stored}/{total_chunks} stored so far",
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
            .buffer_unordered(self.config().chunk_concurrency);

            while let Some(result) = upload_stream.next().await {
                result?;
                total_stored += 1;
            }
        }

        Ok(total_stored)
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
    #[allow(clippy::unused_async)] // Async for API consistency; blocking handled via block_in_place
    pub async fn file_download(&self, data_map: &DataMap, output: &Path) -> Result<u64> {
        debug!("Downloading file to {}", output.display());

        let handle = Handle::current();

        let stream = streaming_decrypt(data_map, |batch: &[(usize, XorName)]| {
            let batch_owned: Vec<(usize, XorName)> = batch.to_vec();

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
            Ok(bytes_written) => {
                std::fs::rename(&tmp_path, output)?;
                info!(
                    "File downloaded: {bytes_written} bytes written to {}",
                    output.display()
                );
                Ok(bytes_written)
            }
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
