//! In-memory data operations using self-encryption.
//!
//! Upload and download raw byte data. Content is encrypted via
//! convergent encryption and stored as content-addressed chunks.
//! Use this when you already have data in memory (e.g., `Bytes`).
//! For file-based streaming uploads that avoid loading the entire
//! file into memory, see the `file` module.

use crate::data::client::adaptive::{observe_op, rebucketed_ordered};
use crate::data::client::batch::{PaymentIntent, PreparedChunk};
use crate::data::client::classify_error;
use crate::data::client::file::{ExternalPaymentInfo, PreparedUpload};
use crate::data::client::merkle::PaymentMode;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::{compute_address, DATA_TYPE_CHUNK};
use bytes::Bytes;
use futures::stream::StreamExt;
use self_encryption::{decrypt, encrypt, DataMap, EncryptedChunk};
use tracing::{debug, info};

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

        let (addresses, _storage_cost, _gas_cost) =
            self.batch_upload_chunks(chunk_contents).await?;
        let chunks_stored = addresses.len();

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
                    info!("Merkle needs more peers ({msg}), falling back to wave-batch");
                    let (addresses, _sc, _gc) = self.batch_upload_chunks(chunk_contents).await?;
                    return Ok(DataUploadResult {
                        data_map,
                        chunks_stored: addresses.len(),
                        payment_mode_used: PaymentMode::Single,
                    });
                }
                Err(e) => return Err(e),
            };

            let chunks_stored = self
                .merkle_upload_chunks(chunk_contents, addresses, &batch_result)
                .await?;

            info!("Data uploaded via merkle: {chunks_stored} chunks stored ({content_len} bytes)");
            Ok(DataUploadResult {
                data_map,
                chunks_stored,
                payment_mode_used: PaymentMode::Merkle,
            })
        } else {
            // Wave-based batch payment path (single EVM tx per wave).
            let (addresses, _sc, _gc) = self.batch_upload_chunks(chunk_contents).await?;

            info!(
                "Data uploaded: {} chunks stored ({content_len} bytes original)",
                addresses.len()
            );
            Ok(DataUploadResult {
                data_map,
                chunks_stored: addresses.len(),
                payment_mode_used: PaymentMode::Single,
            })
        }
    }

    /// Phase 1 of external-signer data upload: encrypt and collect quotes.
    ///
    /// Encrypts in-memory data via self-encryption, then collects storage
    /// quotes for each chunk without making any on-chain payment. Returns
    /// a [`PreparedUpload`] containing the data map and a [`PaymentIntent`]
    /// with the payment details for external signing.
    ///
    /// After the caller signs and submits the payment transaction, call
    /// [`Client::finalize_upload`] with the tx hashes to complete storage.
    ///
    /// # Errors
    ///
    /// Returns an error if encryption fails or quote collection fails.
    pub async fn data_prepare_upload(&self, content: Bytes) -> Result<PreparedUpload> {
        let content_len = content.len();
        debug!("Preparing data upload for external signing ({content_len} bytes)");

        let (data_map, encrypted_chunks) = encrypt(content)
            .map_err(|e| Error::Encryption(format!("Failed to encrypt data: {e}")))?;

        let chunk_count = encrypted_chunks.len();
        info!("Data encrypted into {chunk_count} chunks");

        let chunk_contents: Vec<Bytes> = encrypted_chunks
            .into_iter()
            .map(|chunk| chunk.content)
            .collect();

        let quote_limiter = self.controller().quote.clone();
        let quote_concurrency = quote_limiter.current().min(chunk_count.max(1));
        let results: Vec<Result<Option<PreparedChunk>>> = futures::stream::iter(chunk_contents)
            .map(|content| {
                let limiter = quote_limiter.clone();
                async move {
                    observe_op(
                        &limiter,
                        || async move { self.prepare_chunk_payment(content).await },
                        classify_error,
                    )
                    .await
                }
            })
            .buffer_unordered(quote_concurrency)
            .collect()
            .await;

        let mut prepared_chunks = Vec::with_capacity(results.len());
        for result in results {
            if let Some(prepared) = result? {
                prepared_chunks.push(prepared);
            }
        }

        let payment_intent = PaymentIntent::from_prepared_chunks(&prepared_chunks);

        info!(
            "Data prepared for external signing: {} chunks, total {} atto ({content_len} bytes)",
            prepared_chunks.len(),
            payment_intent.total_amount,
        );

        Ok(PreparedUpload {
            data_map,
            payment_info: ExternalPaymentInfo::WaveBatch {
                prepared_chunks,
                payment_intent,
            },
            data_map_address: None,
        })
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
    /// and reassembles the original content. Fetches chunks concurrently;
    /// the fan-out is sized by the adaptive controller's `fetch` channel
    /// and ramps up under healthy conditions.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk cannot be retrieved or decryption fails.
    pub async fn data_download(&self, data_map: &DataMap) -> Result<Bytes> {
        let chunk_infos = data_map.infos();
        debug!("Downloading data ({} chunks)", chunk_infos.len());

        // Extract owned addresses to avoid HRTB lifetime issue with
        // stream::iter over references combined with async closures.
        let addresses: Vec<[u8; 32]> = chunk_infos.iter().map(|info| info.dst_hash.0).collect();

        // Build ONE peer pool from a single DHT lookup, then reuse it
        // for every chunk's GET. On networks where find_closest_peers
        // dominates per-chunk wall-clock, this turns a download from
        // O(N * lookup_time) into O(lookup_time + N * RPC_time).
        let pool = self.build_peer_pool_for(&addresses).await?;
        debug!(
            "Built peer pool of {} for {} chunks",
            pool.len(),
            addresses.len()
        );
        let pool_ref = &pool;

        // Rolling rebucketing: re-reads the controller's fetch cap as
        // each slot frees, so a long download (e.g. 10 GB = ~2500
        // chunks) sees adaptive growth/decay mid-flight without batch
        // fences. Output is index-sorted so self_encryption decrypt
        // sees DataMap-ordered chunks.
        let fetch_limiter = self.controller().fetch.clone();
        let encrypted_chunks: Vec<EncryptedChunk> = rebucketed_ordered(
            &fetch_limiter,
            addresses.into_iter().enumerate(),
            |(idx, address)| {
                let limiter = fetch_limiter.clone();
                async move {
                    let chunk = observe_op(
                        &limiter,
                        || async move { self.chunk_get_with_pool(&address, pool_ref).await },
                        classify_error,
                    )
                    .await?
                    .ok_or_else(|| {
                        Error::InvalidData(format!(
                            "Missing chunk {} required for data reconstruction",
                            hex::encode(address)
                        ))
                    })?;
                    Ok::<_, Error>((
                        idx,
                        EncryptedChunk {
                            content: chunk.content,
                        },
                    ))
                }
            },
        )
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

/// Compile-time assertions that Client method futures are Send.
///
/// These methods are called from axum handlers and tokio::spawn contexts
/// that require Send + 'static. The async closures inside stream
/// combinators must not capture references with concrete lifetimes
/// (HRTB issue). If any of these checks fail, the stream closures
/// need restructuring to use owned values instead of references.
#[cfg(test)]
mod send_assertions {
    use super::*;

    fn _assert_send<T: Send>(_: &T) {}

    #[allow(
        dead_code,
        unreachable_code,
        unused_variables,
        clippy::diverging_sub_expression
    )]
    async fn _data_download_is_send(client: &Client) {
        let dm: DataMap = todo!();
        let fut = client.data_download(&dm);
        _assert_send(&fut);
    }

    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn _data_upload_is_send(client: &Client) {
        let fut = client.data_upload(Bytes::new());
        _assert_send(&fut);
    }

    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn _data_upload_with_mode_is_send(client: &Client) {
        let fut = client.data_upload_with_mode(Bytes::new(), PaymentMode::Auto);
        _assert_send(&fut);
    }

    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn _data_prepare_upload_is_send(client: &Client) {
        let fut = client.data_prepare_upload(Bytes::new());
        _assert_send(&fut);
    }
}
