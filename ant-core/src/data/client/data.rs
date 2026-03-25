//! In-memory data operations using self-encryption.
//!
//! Upload and download raw byte data. Content is encrypted via
//! convergent encryption and stored as content-addressed chunks.
//! Use this when you already have data in memory (e.g., `Bytes`).
//! For file-based streaming uploads that avoid loading the entire
//! file into memory, see the `file` module.

use crate::data::client::merkle::PaymentMode;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_node::ant_protocol::DATA_TYPE_CHUNK;
use ant_node::client::compute_address;
use bytes::Bytes;
use futures::stream::{self, StreamExt, TryStreamExt};
use self_encryption::{decrypt, encrypt, DataMap, EncryptedChunk};
use tracing::{debug, info, warn};

/// Result of an in-memory data upload: the `DataMap` needed to retrieve the data.
#[derive(Debug, Clone)]
pub struct DataUploadResult {
    /// The data map containing chunk metadata for reconstruction.
    pub data_map: DataMap,
    /// Number of chunks stored on the network.
    pub chunks_stored: usize,
    /// Which payment mode was actually used (not just requested).
    pub payment_mode_used: PaymentMode,
}

impl Client {
    /// Upload in-memory data to the network using self-encryption.
    ///
    /// The content is encrypted and split into chunks, each stored
    /// as a content-addressed chunk on the network. Returns a `DataMap`
    /// that can be used to retrieve and decrypt the data.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails or any chunk cannot be stored.
    pub async fn data_upload(&self, content: Bytes) -> Result<DataUploadResult> {
        let content_len = content.len();
        debug!("Encrypting data ({content_len} bytes)");

        let (data_map, encrypted_chunks) = encrypt(content)
            .map_err(|e| Error::Encryption(format!("Failed to encrypt data: {e}")))?;

        info!("Data encrypted into {} chunks", encrypted_chunks.len());

        let chunk_contents: Vec<Bytes> = encrypted_chunks
            .into_iter()
            .map(|chunk| chunk.content)
            .collect();

        // Fail-fast: cancel remaining uploads on first error to avoid
        // wasting gas on chunks that can't form a complete DataMap.
        let mut chunks_stored = 0usize;
        let mut upload_stream = stream::iter(chunk_contents)
            .map(|content| self.chunk_put(content))
            .buffer_unordered(4);

        while let Some(result) = upload_stream.next().await {
            result?;
            chunks_stored += 1;
        }

        info!("Data uploaded: {chunks_stored} chunks stored ({content_len} bytes original)");

        Ok(DataUploadResult {
            data_map,
            chunks_stored,
            payment_mode_used: PaymentMode::Single,
        })
    }

    /// Upload in-memory data with a specific payment mode.
    ///
    /// When `mode` is `Auto` and the chunk count >= threshold, or when `mode`
    /// is `Merkle`, this buffers all chunks and pays via a single merkle
    /// batch transaction. Otherwise falls back to per-chunk payment.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails or any chunk cannot be stored.
    pub async fn data_upload_with_mode(
        &self,
        content: Bytes,
        mode: PaymentMode,
    ) -> Result<DataUploadResult> {
        let content_len = content.len();
        debug!("Encrypting data ({content_len} bytes) with mode {mode:?}");

        let (data_map, encrypted_chunks) = encrypt(content)
            .map_err(|e| Error::Encryption(format!("Failed to encrypt data: {e}")))?;

        let chunk_count = encrypted_chunks.len();
        info!("Data encrypted into {chunk_count} chunks");

        let chunk_contents: Vec<Bytes> = encrypted_chunks
            .into_iter()
            .map(|chunk| chunk.content)
            .collect();

        if self.should_use_merkle(chunk_count, mode) {
            // Merkle batch payment path
            info!("Using merkle batch payment for {chunk_count} chunks");

            let addresses: Vec<[u8; 32]> =
                chunk_contents.iter().map(|c| compute_address(c)).collect();

            // Compute average chunk size for quoting
            let avg_size =
                chunk_contents.iter().map(bytes::Bytes::len).sum::<usize>() / chunk_count.max(1);
            let avg_size_u64 = u64::try_from(avg_size).unwrap_or(0);

            // Try merkle batch; in Auto mode, fall back to per-chunk on network issues
            let batch_result = match self
                .pay_for_merkle_batch(&addresses, DATA_TYPE_CHUNK, avg_size_u64)
                .await
            {
                Ok(result) => result,
                Err(Error::InsufficientPeers(ref msg)) if mode == PaymentMode::Auto => {
                    warn!("Merkle needs more peers ({msg}), falling back to per-chunk");
                    let mut chunks_stored = 0usize;
                    let mut s = stream::iter(chunk_contents)
                        .map(|c| self.chunk_put(c))
                        .buffer_unordered(4);
                    while let Some(result) = s.next().await {
                        result?;
                        chunks_stored += 1;
                    }
                    return Ok(DataUploadResult {
                        data_map,
                        chunks_stored,
                        payment_mode_used: PaymentMode::Single,
                    });
                }
                Err(e) => return Err(e),
            };

            // Upload each chunk with its merkle proof
            let mut chunks_stored = 0usize;
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
                            self.chunk_put_to_close_group(content, proof, &peers).await
                        }
                    },
                ))
                .buffer_unordered(4);

            while let Some(result) = upload_stream.next().await {
                result?;
                chunks_stored += 1;
            }

            info!("Data uploaded via merkle: {chunks_stored} chunks stored ({content_len} bytes)");
            Ok(DataUploadResult {
                data_map,
                chunks_stored,
                payment_mode_used: PaymentMode::Merkle,
            })
        } else {
            // Standard per-chunk payment path
            let mut chunks_stored = 0usize;
            let mut upload_stream = stream::iter(chunk_contents)
                .map(|content| self.chunk_put(content))
                .buffer_unordered(4);

            while let Some(result) = upload_stream.next().await {
                result?;
                chunks_stored += 1;
            }

            info!("Data uploaded: {chunks_stored} chunks stored ({content_len} bytes original)");
            Ok(DataUploadResult {
                data_map,
                chunks_stored,
                payment_mode_used: PaymentMode::Single,
            })
        }
    }

    /// Store a `DataMap` on the network as a public chunk.
    ///
    /// The serialized `DataMap` is stored as a regular content-addressed chunk.
    /// Anyone who knows the returned address can retrieve and use the `DataMap`
    /// to download the original data.
    ///
    /// # Errors
    ///
    /// Returns an error if serialization or the chunk store fails.
    pub async fn data_map_store(&self, data_map: &DataMap) -> Result<[u8; 32]> {
        let serialized = rmp_serde::to_vec(data_map)
            .map_err(|e| Error::Serialization(format!("Failed to serialize DataMap: {e}")))?;

        info!(
            "Storing DataMap as public chunk ({} bytes serialized)",
            serialized.len()
        );

        self.chunk_put(Bytes::from(serialized)).await
    }

    /// Fetch a `DataMap` from the network by its chunk address.
    ///
    /// Retrieves the chunk at `address` and deserializes it as a `DataMap`.
    ///
    /// # Errors
    ///
    /// Returns an error if the chunk is not found or deserialization fails.
    pub async fn data_map_fetch(&self, address: &[u8; 32]) -> Result<DataMap> {
        let chunk = self.chunk_get(address).await?.ok_or_else(|| {
            Error::InvalidData(format!(
                "DataMap chunk not found at {}",
                hex::encode(address)
            ))
        })?;

        rmp_serde::from_slice(&chunk.content)
            .map_err(|e| Error::Serialization(format!("Failed to deserialize DataMap: {e}")))
    }

    /// Download and decrypt data from the network using its `DataMap`.
    ///
    /// Retrieves all chunks referenced by the data map, then decrypts
    /// and reassembles the original content. Uses `buffered(4)` to
    /// fetch chunks concurrently while preserving order.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved or decryption fails.
    pub async fn data_download(&self, data_map: &DataMap) -> Result<Bytes> {
        let chunk_infos = data_map.infos();
        debug!("Downloading data ({} chunks)", chunk_infos.len());

        let encrypted_chunks: Vec<EncryptedChunk> = stream::iter(chunk_infos.iter())
            .map(|info| {
                let address = info.dst_hash.0;
                async move {
                    let chunk = self.chunk_get(&address).await?.ok_or_else(|| {
                        Error::InvalidData(format!(
                            "Missing chunk {} required for data reconstruction",
                            hex::encode(address)
                        ))
                    })?;
                    Ok::<_, Error>(EncryptedChunk {
                        content: chunk.content,
                    })
                }
            })
            .buffered(4)
            .try_collect()
            .await?;

        debug!(
            "All {} chunks retrieved, decrypting",
            encrypted_chunks.len()
        );

        let content = decrypt(data_map, &encrypted_chunks)
            .map_err(|e| Error::Encryption(format!("Failed to decrypt data: {e}")))?;

        info!("Data downloaded and decrypted ({} bytes)", content.len());

        Ok(content)
    }
}
