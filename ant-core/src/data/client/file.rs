// Copyright 2026 Saorsa Labs Limited
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared upload types and finalization. Filesystem streaming is native-only.

#[cfg(feature = "native")]
mod native;
use crate::data::client::batch::{
    finalize_batch_payment, PaymentIntent, PreparedChunk, WaveAggregateStats,
};
use crate::data::client::merkle::{finalize_merkle_batch, PaymentMode, PreparedMerkleBatch};
use crate::data::client::Client;
use crate::data::error::{Error, PartialUploadSpend, Result};
use ant_protocol::evm::{QuoteHash, TxHash};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::XorName as ChunkAddress;
use bytes::Bytes;
use self_encryption::DataMap;
use std::collections::HashMap;
use tokio::sync::mpsc;
use tracing::info;

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
    /// Merkle: single on-chain call with depth, pool commitments, timestamp.
    Merkle {
        /// The prepared merkle batch (public fields sent to frontend, private fields stay in Rust).
        prepared_batch: PreparedMerkleBatch,
        /// Raw chunk contents that still need upload after the preflight check.
        chunk_contents: Vec<Bytes>,
        /// Chunk addresses that still need upload after the preflight check.
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

impl Client {
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
        let data_map_address = prepared.data_map_address;
        let already_stored_count = prepared.already_stored_addresses.len();
        let total_chunks = prepared.total_chunks;
        match prepared.payment_info {
            ExternalPaymentInfo::Merkle {
                prepared_batch,
                chunk_contents,
                chunk_addresses,
            } => {
                let batch_result = finalize_merkle_batch(prepared_batch, winner_pool_hash)?;
                let outcome = self
                    .merkle_upload_chunks(
                        chunk_contents,
                        chunk_addresses,
                        &batch_result,
                        progress.as_ref(),
                        already_stored_count,
                        total_chunks,
                    )
                    .await?;

                info!(
                    "External-signer merkle upload finalized: {} chunks stored, {} failed",
                    outcome.stored, outcome.failed
                );

                Ok(FileUploadResult {
                    data_map: prepared.data_map,
                    chunks_stored: outcome.stored,
                    chunks_failed: outcome.failed,
                    total_chunks,
                    payment_mode_used: PaymentMode::Merkle,
                    storage_cost_atto: "0".into(),
                    gas_cost_wei: 0,
                    data_map_address,
                    chunk_attempts_total: outcome.stats.chunk_attempts_total,
                    store_durations_ms: outcome.stats.store_durations_ms,
                    retries_histogram: outcome.stats.retries_histogram,
                })
            }
            ExternalPaymentInfo::WaveBatch { .. } => Err(Error::Payment(
                "Cannot finalize wave-batch upload with merkle winner hash. \
                 Use finalize_upload() instead."
                    .to_string(),
            )),
        }
    }
}
