// Copyright 2026 Saorsa Labs Limited
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable file metadata and events. Filesystem staging and finalization are native-only.

#[cfg(feature = "native")]
mod native;
use crate::data::client::merkle::PaymentMode;
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::XorName as ChunkAddress;
#[cfg(feature = "native")]
pub use native::{
    ExternalChunkStore, ExternalPaymentInfo, FinalizeOutcome, FinalizeResume, MerkleFinalizeResume,
    PreparedUpload, WaveFinalizeResume,
};
use self_encryption::DataMap;

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
/// retrieve the `DataMap` (via [`crate::data::Client::data_map_fetch`]) and then the file.
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
/// [`crate::data::Client::estimate_upload_cost`].
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
    /// retrievable from the network (via [`crate::data::Client::data_map_fetch`])**, not
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
