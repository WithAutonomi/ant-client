//! File operations using streaming self-encryption.
//!
//! Upload files directly from disk without loading them entirely into memory.
//! Uses `stream_encrypt` to process files in 8KB chunks, encrypting and
//! uploading each piece as it's produced.
//! For in-memory data uploads, see the `data` module.

use crate::data::client::merkle::PaymentMode;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use bytes::Bytes;
use futures::stream;
use futures::StreamExt;
use saorsa_node::ant_protocol::DATA_TYPE_CHUNK;
use saorsa_node::client::compute_address;
use self_encryption::{stream_encrypt, streaming_decrypt, DataMap};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::runtime::Handle;
use tracing::{debug, info, warn};

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
                    if let Ok(mut guard) = read_error_clone.lock() {
                        *guard = Some(e);
                    }
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
            if let Ok(guard) = read_error.lock() {
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
        if let Ok(guard) = read_error.lock() {
            if let Some(ref e) = *guard {
                return Err(Error::Io(std::io::Error::new(e.kind(), e.to_string())));
            }
        }

        let datamap = stream
            .into_datamap()
            .ok_or_else(|| Error::Encryption("no DataMap after encryption".to_string()))?;
        let _ = datamap_tx.send(datamap);
        Ok(())
    });

    Ok((chunk_rx, datamap_rx, handle))
}

impl Client {
    /// Upload a file to the network using streaming self-encryption.
    ///
    /// The file is read in 8KB chunks, encrypted via `stream_encrypt`,
    /// and each encrypted chunk is uploaded as it's produced. The file
    /// is never fully loaded into memory.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, encryption fails,
    /// or any chunk cannot be stored.
    pub async fn file_upload(&self, path: &Path) -> Result<FileUploadResult> {
        debug!("Streaming file upload: {}", path.display());

        let (mut chunk_rx, datamap_rx, handle) = spawn_file_encryption(path.to_path_buf())?;

        // Collect chunks from encryption channel, then upload concurrently.
        let mut chunk_contents = Vec::new();
        while let Some(content) = chunk_rx.recv().await {
            chunk_contents.push(content);
        }

        let mut chunks_stored = 0usize;
        let mut upload_stream = futures::stream::iter(chunk_contents)
            .map(|content| self.chunk_put(content))
            .buffer_unordered(4);

        while let Some(result) = futures::StreamExt::next(&mut upload_stream).await {
            result?;
            chunks_stored += 1;
        }

        handle
            .await
            .map_err(|e| Error::Encryption(format!("encryption task panicked: {e}")))?
            .map_err(|e| Error::Encryption(format!("encryption failed: {e}")))?;

        let data_map = datamap_rx
            .await
            .map_err(|_| Error::Encryption("no DataMap from encryption thread".to_string()))?;

        info!(
            "File uploaded: {chunks_stored} chunks stored ({})",
            path.display()
        );

        Ok(FileUploadResult {
            data_map,
            chunks_stored,
            payment_mode_used: PaymentMode::Single,
        })
    }

    /// Upload a file with a specific payment mode.
    ///
    /// Uses buffer-then-pay strategy: encrypts file, collects all chunks,
    /// then pays via merkle batch or per-chunk depending on mode and count.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, encryption fails,
    /// or any chunk cannot be stored.
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

        let (mut chunk_rx, datamap_rx, handle) = spawn_file_encryption(path.to_path_buf())?;

        // Collect all chunks first (buffer-then-pay)
        let mut chunk_contents = Vec::new();
        while let Some(content) = chunk_rx.recv().await {
            chunk_contents.push(content);
        }

        let chunk_count = chunk_contents.len();

        let (chunks_stored, actual_mode) = if self.should_use_merkle(chunk_count, mode) {
            // Merkle batch payment path
            info!("Using merkle batch payment for {chunk_count} file chunks");

            let addresses: Vec<[u8; 32]> =
                chunk_contents.iter().map(|c| compute_address(c)).collect();

            let avg_size =
                chunk_contents.iter().map(bytes::Bytes::len).sum::<usize>() / chunk_count.max(1);
            let avg_size_u64 = u64::try_from(avg_size).unwrap_or(0);

            let batch_result = match self
                .pay_for_merkle_batch(&addresses, DATA_TYPE_CHUNK, avg_size_u64)
                .await
            {
                Ok(result) => result,
                Err(Error::InsufficientPeers(ref msg)) if mode == PaymentMode::Auto => {
                    warn!("Merkle needs more peers ({msg}), falling back to per-chunk");
                    let mut s_stored = 0usize;
                    let mut s = stream::iter(chunk_contents)
                        .map(|c| self.chunk_put(c))
                        .buffer_unordered(4);
                    while let Some(result) = futures::StreamExt::next(&mut s).await {
                        result?;
                        s_stored += 1;
                    }

                    // Need datamap from the encryption handle
                    handle
                        .await
                        .map_err(|e| Error::Encryption(format!("encryption task panicked: {e}")))?
                        .map_err(|e| Error::Encryption(format!("encryption failed: {e}")))?;
                    let data_map = datamap_rx.await.map_err(|_| {
                        Error::Encryption("no DataMap from encryption thread".to_string())
                    })?;

                    return Ok(FileUploadResult {
                        data_map,
                        chunks_stored: s_stored,
                        payment_mode_used: PaymentMode::Single,
                    });
                }
                Err(e) => return Err(e),
            };

            let mut stored = 0usize;
            let mut upload_stream =
                stream::iter(chunk_contents.into_iter().zip(addresses.iter()).map(
                    |(content, addr)| {
                        let proof_bytes = batch_result.proofs.get(addr).cloned();
                        async move {
                            let proof = proof_bytes.ok_or_else(|| {
                                Error::Payment(format!(
                                    "Missing merkle proof for chunk {}",
                                    hex::encode(addr)
                                ))
                            })?;
                            let peers = self.close_group_peers(addr).await?;
                            let target = peers.first().ok_or_else(|| {
                                Error::InsufficientPeers("no peers for chunk".to_string())
                            })?;
                            self.chunk_put_with_proof(content, proof, target).await
                        }
                    },
                ))
                .buffer_unordered(4);

            while let Some(result) = futures::StreamExt::next(&mut upload_stream).await {
                result?;
                stored += 1;
            }
            (stored, PaymentMode::Merkle)
        } else {
            // Standard per-chunk payment path
            let mut stored = 0usize;
            let mut upload_stream = stream::iter(chunk_contents)
                .map(|content| self.chunk_put(content))
                .buffer_unordered(4);

            while let Some(result) = futures::StreamExt::next(&mut upload_stream).await {
                result?;
                stored += 1;
            }
            (stored, PaymentMode::Single)
        };

        handle
            .await
            .map_err(|e| Error::Encryption(format!("encryption task panicked: {e}")))?
            .map_err(|e| Error::Encryption(format!("encryption failed: {e}")))?;

        let data_map = datamap_rx
            .await
            .map_err(|_| Error::Encryption("no DataMap from encryption thread".to_string()))?;

        info!(
            "File uploaded with mode {mode:?}: {chunks_stored} chunks stored ({})",
            path.display()
        );

        Ok(FileUploadResult {
            data_map,
            chunks_stored,
            payment_mode_used: actual_mode,
        })
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
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved, decryption fails,
    /// or the file cannot be written.
    #[allow(clippy::unused_async)] // Async for API consistency; blocking handled via block_in_place
    pub async fn file_download(&self, data_map: &DataMap, output: &Path) -> Result<u64> {
        debug!("Downloading file to {}", output.display());

        let handle = Handle::current();

        let stream = streaming_decrypt(data_map, |batch: &[(usize, xor_name::XorName)]| {
            let batch_owned: Vec<(usize, xor_name::XorName)> = batch.to_vec();

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
        let tmp_path = parent.join(format!(
            ".saorsa_download_{}_{unique}.tmp",
            std::process::id()
        ));

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
                let _ = std::fs::remove_file(&tmp_path);
                Err(e)
            }
        }
    }
}
