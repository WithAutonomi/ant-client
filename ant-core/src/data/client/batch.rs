//! Batch chunk upload with wave-based pipelined EVM payments.
//!
//! Groups chunks into waves of 64 and pays for each
//! wave in a single EVM transaction. Stores from wave N are pipelined
//! with quote collection for wave N+1 via `tokio::join!`.

use crate::data::client::adaptive::observe_op;
use crate::data::client::classify_error;
use crate::data::client::file::UploadEvent;
use crate::data::client::payment::{
    paid_quote_payment_from_store_quotes_with_target_and_views, peer_id_to_encoded,
};
use crate::data::client::Client;
use crate::data::error::{Error, PartialUploadSpend, Result};
use ant_protocol::evm::{
    Amount, EncodedPeerId, PayForQuotesError, PaymentQuote, ProofOfPayment, QuoteHash,
    RewardsAddress, TxHash,
};
use ant_protocol::payment::{
    deserialize_proof, serialize_single_node_proof, PaymentProof, QuotePaymentInfo,
};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{compute_address, XorName, DATA_TYPE_CHUNK};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Number of chunks per payment wave.
const PAYMENT_WAVE_SIZE: usize = 64;

/// Soft ceiling on the combined body size of chunks stored concurrently in a
/// single wave. Caps store concurrency for large chunks so the send path's
/// per-peer body buffers can't pin multiple GB at once (see V2-461). At ~4 MB
/// chunks this permits ~16 concurrent stores; small chunks hit the chunk-count
/// / adaptive limits instead and are unaffected.
const STORE_INFLIGHT_BYTE_BUDGET: usize = 64 * 1024 * 1024;

/// Payment entries for a prepared chunk.
#[derive(Debug, Clone)]
pub struct PreparedChunkPayment {
    /// Quote payment entries that must be paid before storing.
    pub quotes: Vec<QuotePaymentInfo>,
}

/// Chunk quoted but not yet paid. Produced by [`Client::prepare_chunk_payment`].
#[derive(Debug)]
pub struct PreparedChunk {
    /// The chunk content bytes.
    pub content: Bytes,
    /// Content address (BLAKE3 hash).
    pub address: XorName,
    /// Closest peers from quote collection — PUT targets for close-group replication.
    pub quoted_peers: Vec<(PeerId, Vec<MultiAddr>)>,
    /// Payment entries selected for the proof, not yet paid on-chain.
    pub payment: PreparedChunkPayment,
    /// Peer quotes for building `ProofOfPayment`.
    pub peer_quotes: Vec<(EncodedPeerId, PaymentQuote)>,
}

/// Chunk paid but not yet stored. Produced by [`Client::batch_pay`].
#[derive(Debug, Clone)]
pub struct PaidChunk {
    /// The chunk content bytes.
    pub content: Bytes,
    /// Content address (BLAKE3 hash).
    pub address: XorName,
    /// Closest peers from quote collection — PUT targets for close-group replication.
    pub quoted_peers: Vec<(PeerId, Vec<MultiAddr>)>,
    /// Serialized [`PaymentProof`] bytes.
    pub proof_bytes: Vec<u8>,
}

/// Result of storing a wave of paid chunks, with retry tracking.
#[derive(Debug)]
pub struct WaveResult {
    /// Successfully stored chunk addresses.
    pub stored: Vec<XorName>,
    /// Chunks that failed to store after all retries.
    pub failed: Vec<(XorName, String)>,
    /// Sum of store-RPC attempts across all chunks in this wave (>= stored.len() + failed.len()).
    pub chunk_attempts_total: usize,
    /// Per-chunk wall-clock (ms) from first attempt to successful store. Only populated for stored chunks.
    pub store_durations_ms: Vec<u64>,
    /// Histogram of which retry-round each stored chunk succeeded on (index 0 = first attempt).
    pub retries_per_chunk: Vec<u32>,
}

/// Aggregated retry / wall-clock stats across one or more [`WaveResult`]s.
///
/// Used by [`Client::batch_upload_chunks_with_events`] (which may store
/// multiple waves per call) and surfaced upward into `FileUploadResult` so
/// downstream tooling can record per-upload retry pressure and per-chunk
/// store wall-clock without needing log parsing.
#[derive(Debug, Default, Clone)]
pub struct WaveAggregateStats {
    /// Sum of store-RPC attempts across all waves (>= chunks_stored).
    pub chunk_attempts_total: usize,
    /// Per-chunk wall-clock (ms) from first attempt to successful store,
    /// concatenated across waves.
    pub store_durations_ms: Vec<u64>,
    /// Count of stored chunks that succeeded on each retry round
    /// (index 0 = first attempt, 1 = first retry, etc.). Indices match
    /// the retry rounds emitted by `Client::store_paid_chunks_with_events`
    /// which caps at `MAX_RETRIES = 3`, so an array of 4 suffices.
    pub retries_histogram: [usize; 4],
}

impl WaveAggregateStats {
    /// Fold one [`WaveResult`]'s stats into the running aggregate.
    pub fn absorb(&mut self, wave: &WaveResult) {
        self.chunk_attempts_total = self
            .chunk_attempts_total
            .saturating_add(wave.chunk_attempts_total);
        self.store_durations_ms.extend(&wave.store_durations_ms);
        for &r in &wave.retries_per_chunk {
            let idx = (r as usize).min(self.retries_histogram.len() - 1);
            self.retries_histogram[idx] = self.retries_histogram[idx].saturating_add(1);
        }
    }
}

/// Compute a percentile from an unsorted slice of `u64` values.
///
/// `p` is in `[0.0, 1.0]`. Returns 0 for an empty slice. Uses nearest-rank;
/// callers don't need numerical precision here — these are coarse log/metric
/// summaries.
fn percentile(values: &[u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    let p = p.clamp(0.0, 1.0);
    // Nearest-rank: ceil(p * n) - 1, clamped to [0, n-1].
    let n = sorted.len();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let rank = ((p * n as f64).ceil() as usize)
        .saturating_sub(1)
        .min(n - 1);
    sorted[rank]
}

/// Payment data for external signing.
///
/// Contains the information needed to construct and submit the on-chain
/// payment transaction without requiring a local wallet or private key.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaymentIntent {
    /// Individual payment entries: (quote_hash, rewards_address, amount).
    pub payments: Vec<(QuoteHash, RewardsAddress, Amount)>,
    /// Total amount across all payments.
    pub total_amount: Amount,
}

impl PaymentIntent {
    /// Build from a set of prepared chunks.
    ///
    /// Collects all non-zero payment entries and computes the total.
    pub fn from_prepared_chunks(prepared: &[PreparedChunk]) -> Self {
        let mut payments = Vec::new();
        let mut total = Amount::ZERO;
        for chunk in prepared {
            for info in &chunk.payment.quotes {
                if !info.amount.is_zero() {
                    payments.push((info.quote_hash, info.rewards_address, info.amount));
                    total += info.amount;
                }
            }
        }
        Self {
            payments,
            total_amount: total,
        }
    }
}

/// Build [`PaidChunk`]s from prepared chunks and externally-provided transaction hashes.
///
/// Shared by [`Client::batch_pay`] (wallet flow) and [`finalize_batch_payment`] (external signer).
///
/// Returns an error if any non-zero-amount quote hash is missing from `tx_hash_map`,
/// since chunks uploaded without valid proofs would be rejected by the network.
fn build_paid_chunks(
    prepared: Vec<PreparedChunk>,
    tx_hash_map: &HashMap<QuoteHash, TxHash>,
) -> Result<Vec<PaidChunk>> {
    let mut paid_chunks = Vec::with_capacity(prepared.len());
    for chunk in prepared {
        let mut tx_hashes = Vec::new();
        for info in &chunk.payment.quotes {
            if !info.amount.is_zero() {
                let tx_hash = tx_hash_map.get(&info.quote_hash).copied().ok_or_else(|| {
                    Error::Payment(format!(
                        "Missing tx hash for quote {} — external signer did not return a receipt for this payment",
                        hex::encode(info.quote_hash)
                    ))
                })?;
                tx_hashes.push(tx_hash);
            }
        }

        let proof = PaymentProof {
            proof_of_payment: ProofOfPayment {
                peer_quotes: chunk.peer_quotes,
            },
            tx_hashes,
        };

        let proof_bytes = serialize_single_node_proof(&proof)
            .map_err(|e| Error::Serialization(format!("Failed to serialize payment proof: {e}")))?;

        paid_chunks.push(PaidChunk {
            content: chunk.content,
            address: chunk.address,
            quoted_peers: chunk.quoted_peers,
            proof_bytes,
        });
    }
    Ok(paid_chunks)
}

/// Finalize a batch payment using externally-provided transaction hashes.
///
/// Takes prepared chunks and a map of `quote_hash -> tx_hash` from the
/// external signer. Builds per-chunk `PaymentProof` bytes without needing a wallet.
pub fn finalize_batch_payment(
    prepared: Vec<PreparedChunk>,
    tx_hash_map: &HashMap<QuoteHash, TxHash>,
) -> Result<Vec<PaidChunk>> {
    build_paid_chunks(prepared, tx_hash_map)
}

impl Client {
    /// Prepare a single chunk for batch payment.
    ///
    /// Collects quotes and uses node-reported prices without making any
    /// on-chain transaction. Returns `Ok(None)` if the chunk is already
    /// stored on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if quote collection or payment construction fails.
    pub async fn prepare_chunk_payment(&self, content: Bytes) -> Result<Option<PreparedChunk>> {
        let address = compute_address(&content);
        let data_size = u64::try_from(content.len())
            .map_err(|e| Error::InvalidData(format!("content size too large: {e}")))?;

        let quote_plan = match self
            .get_store_quote_plan(&address, data_size, DATA_TYPE_CHUNK)
            .await
        {
            Ok(plan) => plan,
            Err(Error::AlreadyStored) => {
                debug!("Chunk {} already stored, skipping", hex::encode(address));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let quotes_with_peers = quote_plan.quotes;

        // Capture all quoted peers for close-group replication.
        let quoted_peers = quote_plan.put_peers;

        let (paid_peer_id, paid_quote, paid_quote_info) =
            paid_quote_payment_from_store_quotes_with_target_and_views(
                &quotes_with_peers,
                quote_plan.paid_quote_acceptance_target,
                &quote_plan.witness_views,
            )?;
        let peer_quotes = vec![(peer_id_to_encoded(&paid_peer_id)?, paid_quote)];
        let payment = PreparedChunkPayment {
            quotes: vec![paid_quote_info],
        };

        Ok(Some(PreparedChunk {
            content,
            address,
            quoted_peers,
            payment,
            peer_quotes,
        }))
    }

    /// Pay for multiple chunks in a single EVM transaction.
    ///
    /// Flattens all quote payments from the prepared chunks into one
    /// `wallet.pay_for_quotes()` call, then maps transaction hashes
    /// back to per-chunk [`PaymentProof`] bytes.
    ///
    /// # Errors
    ///
    /// Returns an error if the wallet is not configured or the on-chain
    /// payment fails.
    /// Returns `(paid_chunks, storage_cost_atto, gas_cost_wei)`.
    pub async fn batch_pay(
        &self,
        prepared: Vec<PreparedChunk>,
    ) -> Result<(Vec<PaidChunk>, String, u128)> {
        if prepared.is_empty() {
            return Ok((Vec::new(), "0".to_string(), 0));
        }

        let wallet = self.require_wallet()?;

        // Compute total storage cost from the prepared chunks before paying.
        let intent = PaymentIntent::from_prepared_chunks(&prepared);
        let storage_cost_atto = intent.total_amount.to_string();

        // Flatten all paid quote entries from all chunks into a single batch.
        let total_payments: usize = intent.payments.len();
        let mut all_payments = Vec::with_capacity(total_payments);
        for chunk in &prepared {
            for info in &chunk.payment.quotes {
                if !info.amount.is_zero() {
                    all_payments.push((info.quote_hash, info.rewards_address, info.amount));
                }
            }
        }

        debug!(
            "Batch payment for {} chunks ({} paid quote entries)",
            prepared.len(),
            all_payments.len()
        );

        let (tx_hash_map, gas_info) =
            wallet
                .pay_for_quotes(all_payments)
                .await
                .map_err(|PayForQuotesError(err, _)| {
                    Error::Payment(format!("Batch payment failed: {err}"))
                })?;

        info!(
            "Batch payment succeeded: {} transactions",
            tx_hash_map.len()
        );

        let tx_hash_map: HashMap<QuoteHash, TxHash> = tx_hash_map.into_iter().collect();
        let paid_chunks = build_paid_chunks(prepared, &tx_hash_map)?;
        Ok((paid_chunks, storage_cost_atto, gas_info.gas_cost_wei))
    }

    /// Upload chunks in waves with pipelined EVM payments.
    ///
    /// Processes chunks in waves of `PAYMENT_WAVE_SIZE` (64). Within each wave:
    /// 1. **Prepare**: collect quotes for all chunks concurrently
    /// 2. **Pay**: single EVM transaction for the whole wave
    /// 3. **Store**: concurrent chunk replication to close group
    ///
    /// Stores from wave N overlap with quote collection for wave N+1
    /// via `tokio::join!`.
    ///
    /// # Errors
    ///
    /// Returns an error if any payment or store operation fails.
    /// Returns `(addresses, total_storage_cost_atto, total_gas_cost_wei)`.
    pub async fn batch_upload_chunks(
        &self,
        chunks: Vec<Bytes>,
    ) -> Result<(Vec<XorName>, String, u128)> {
        let (addresses, storage, gas, _stats) = self
            .batch_upload_chunks_with_events(chunks, None, 0, 0, None)
            .await?;
        Ok((addresses, storage, gas))
    }

    /// Same as [`Client::batch_upload_chunks`] but sends [`UploadEvent::ChunkStored`]
    /// events as each chunk is stored, enabling per-chunk progress bars.
    ///
    /// `stored_offset` is the number of chunks already stored in previous waves
    /// (so events report cumulative progress). `file_total` is the total chunk
    /// count across ALL waves (for the `total` field in events).
    ///
    /// When `resume_key` is `Some`, per-wave payment proofs are persisted
    /// to `<data_dir>/payments/single/<ts>_<hash(resume_key)>` via
    /// `crate::data::client::cached_single` so that a partial-upload
    /// failure can be resumed on the next attempt without paying twice.
    /// The caller is responsible for deleting the cache entry on full
    /// success (typically `upload_with_options` in `file.rs`).
    pub async fn batch_upload_chunks_with_events(
        &self,
        chunks: Vec<Bytes>,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        stored_offset: usize,
        file_total: usize,
        resume_key: Option<&str>,
    ) -> Result<(Vec<XorName>, String, u128, WaveAggregateStats)> {
        if chunks.is_empty() {
            return Ok((
                Vec::new(),
                "0".to_string(),
                0,
                WaveAggregateStats::default(),
            ));
        }

        let total_chunks = chunks.len();
        let quote_cap = self.controller().quote.current();
        let store_cap = self.controller().store.current();
        debug!(
            "Batch uploading {total_chunks} chunks in waves of {PAYMENT_WAVE_SIZE} \
             (current adaptive caps — quote: {quote_cap}, store: {store_cap})"
        );

        // Load any previously-cached single-node receipt for this
        // upload. Each chunk whose address is in the cache will skip
        // the quote + pay phases and have its `PaidChunk` constructed
        // directly from the cached proof + fresh quoted peers. The
        // caller is responsible for deleting the cache on full
        // success; we only read here, never write the load result back.
        //
        // Before trusting any cached proof, decode it locally and drop
        // any whose quote.timestamp is past the storer's per-quote age
        // budget (`QUOTE_MAX_AGE_SECS`, mirrored here as
        // `CACHED_PROOF_EXPIRY_SECS`). The previous design trusted a
        // substring match on remote error text, which a Byzantine
        // storer could spoof to force double-payment. Local pre-flight
        // is decision-pure: we never hand a doomed proof to a storer,
        // and the cache is updated under our own lock with no remote
        // text involved.
        // Load only the cached PROOFS (for reuse). The cost this function
        // returns is a per-call DELTA — what was freshly paid in THIS call —
        // not the cache's cumulative. The single-node wave driver
        // (`upload_spill_addresses_single`) calls this once per wave and SUMS
        // the per-call costs, so seeding the return with the cumulative cache
        // (which grows as each wave appends to it) double-counts:
        // A + (A+B) + (A+B+C) instead of A+B+C.
        let cached_proofs: HashMap<XorName, Vec<u8>> = match resume_key {
            Some(key) => match crate::data::client::cached_single::try_load_for_file(key) {
                Some((_, receipt)) => prune_locally_expired_proofs(key, receipt.proofs),
                None => HashMap::new(),
            },
            None => HashMap::new(),
        };

        let mut all_addresses = Vec::with_capacity(total_chunks);
        let mut seen_addresses: HashSet<XorName> = HashSet::new();

        // Accumulate only THIS call's freshly-paid cost (per-call delta; see
        // the proof-load comment above for why this must not include the cache).
        let mut total_storage = Amount::ZERO;
        let mut total_gas: u128 = 0;
        let mut agg_stats = WaveAggregateStats::default();

        // Deduplicate chunks by content address.
        let mut unique_chunks = Vec::with_capacity(total_chunks);
        for chunk in chunks {
            let address = compute_address(&chunk);
            if seen_addresses.insert(address) {
                unique_chunks.push(chunk);
            } else {
                debug!("Skipping duplicate chunk {}", hex::encode(address));
                all_addresses.push(address);
                if let Some(tx) = progress {
                    let _ = tx.try_send(UploadEvent::ChunkStored {
                        stored: stored_offset + all_addresses.len(),
                        total: file_total,
                    });
                }
            }
        }

        // Split into waves.
        let waves: Vec<Vec<Bytes>> = unique_chunks
            .chunks(PAYMENT_WAVE_SIZE)
            .map(<[Bytes]>::to_vec)
            .collect();
        let wave_count = waves.len();

        debug!(
            "{total_chunks} chunks -> {} unique -> {wave_count} waves",
            seen_addresses.len()
        );

        let mut pending_store: Option<Vec<PaidChunk>> = None;
        let mut total_quoted: usize = 0;

        for (wave_idx, wave_chunks) in waves.into_iter().enumerate() {
            let wave_num = wave_idx + 1;
            let wave_size = wave_chunks.len();

            // Pipeline: store previous wave while preparing this one.
            let (prepare_result, store_result) = match pending_store.take() {
                Some(paid_chunks) => {
                    let store_offset = stored_offset + all_addresses.len();
                    let quoted_offset = stored_offset + total_quoted;
                    let (prep, stored) = tokio::join!(
                        self.prepare_wave(wave_chunks, progress, quoted_offset, file_total),
                        self.store_paid_chunks_with_events(
                            paid_chunks,
                            progress,
                            store_offset,
                            file_total
                        )
                    );
                    (prep, Some(stored))
                }
                None => {
                    let quoted_offset = stored_offset + total_quoted;
                    let result = self
                        .prepare_wave(wave_chunks, progress, quoted_offset, file_total)
                        .await;
                    (result, None)
                }
            };
            total_quoted += wave_size;

            // Track partial progress from previous wave.
            if let Some(wave_result) = store_result {
                all_addresses.extend(&wave_result.stored);
                agg_stats.absorb(&wave_result);
                if !wave_result.failed.is_empty() {
                    let failed_count = wave_result.failed.len();
                    warn!("{failed_count} chunks failed to store after retries");
                    return Err(Error::PartialUpload {
                        stored: all_addresses.clone(),
                        stored_count: stored_offset + all_addresses.len(),
                        failed: wave_result.failed,
                        failed_count,
                        total_chunks: file_total,
                        spend: Box::new(PartialUploadSpend {
                            storage_cost_atto: total_storage.to_string(),
                            gas_cost_wei: total_gas,
                        }),
                        reason: "wave store failed after retries".into(),
                    });
                }
            }

            let (prepared_chunks, already_stored) = prepare_result?;
            all_addresses.extend(&already_stored);
            if let Some(tx) = progress {
                for _ in &already_stored {
                    let _ = tx.try_send(UploadEvent::ChunkStored {
                        stored: stored_offset + all_addresses.len(),
                        total: file_total,
                    });
                }
            }

            if prepared_chunks.is_empty() {
                info!("Wave {wave_num}/{wave_count}: all chunks already stored");
                continue;
            }

            // Split prepared chunks into "already paid in a previous
            // attempt" (cached) and "needs payment" (fresh). Cached
            // chunks build a `PaidChunk` from the cached proof + the
            // freshly-quoted peers, bypassing the EVM transaction.
            let mut needs_pay: Vec<PreparedChunk> = Vec::with_capacity(prepared_chunks.len());
            let mut cached_paid: Vec<PaidChunk> = Vec::new();
            let mut stale_cached: Vec<([u8; 32], Vec<u8>)> = Vec::new();
            for prep in prepared_chunks {
                if let Some(proof_bytes) = cached_proofs.get(&prep.address).cloned() {
                    if cached_proof_matches_current_quote_floor(&proof_bytes, &prep) {
                        cached_paid.push(PaidChunk {
                            content: prep.content,
                            address: prep.address,
                            quoted_peers: prep.quoted_peers,
                            proof_bytes,
                        });
                    } else {
                        stale_cached.push((prep.address, proof_bytes));
                        needs_pay.push(prep);
                    }
                } else {
                    needs_pay.push(prep);
                }
            }
            if let Some(key) = resume_key {
                if !stale_cached.is_empty() {
                    info!(
                        "Wave {wave_num}/{wave_count}: discarding {} cached payment proofs \
                         below current quote floor",
                        stale_cached.len()
                    );
                    crate::data::client::cached_single::try_drop_proofs_for_file(
                        key,
                        &stale_cached,
                    );
                }
            }
            if !cached_paid.is_empty() {
                info!(
                    "Wave {wave_num}/{wave_count}: reusing {} cached payment proofs",
                    cached_paid.len()
                );
            }

            let (mut paid_chunks, wave_storage, wave_gas) = if needs_pay.is_empty() {
                (Vec::new(), "0".to_string(), 0u128)
            } else {
                info!(
                    "Wave {wave_num}/{wave_count}: paying for {} chunks",
                    needs_pay.len()
                );
                self.batch_pay(needs_pay).await?
            };
            if let Ok(cost) = wave_storage.parse::<Amount>() {
                total_storage += cost;
            }
            total_gas = total_gas.saturating_add(wave_gas);

            // Persist the freshly-paid wave's proofs so a later
            // failure can resume without re-paying.
            if let Some(key) = resume_key {
                if !paid_chunks.is_empty() {
                    let new_proofs: HashMap<[u8; 32], Vec<u8>> = paid_chunks
                        .iter()
                        .map(|pc| (pc.address, pc.proof_bytes.clone()))
                        .collect();
                    crate::data::client::cached_single::try_append_wave(
                        key,
                        new_proofs,
                        &wave_storage,
                        wave_gas,
                    );
                }
            }

            paid_chunks.extend(cached_paid);
            pending_store = Some(paid_chunks);
        }

        // Store the last wave.
        if let Some(paid_chunks) = pending_store {
            let store_offset = stored_offset + all_addresses.len();
            let wave_result = self
                .store_paid_chunks_with_events(paid_chunks, progress, store_offset, file_total)
                .await;
            all_addresses.extend(&wave_result.stored);
            agg_stats.absorb(&wave_result);
            if !wave_result.failed.is_empty() {
                let failed_count = wave_result.failed.len();
                warn!("{failed_count} chunks failed to store after retries (final wave)");
                return Err(Error::PartialUpload {
                    stored: all_addresses.clone(),
                    stored_count: stored_offset + all_addresses.len(),
                    failed: wave_result.failed,
                    failed_count,
                    total_chunks: file_total,
                    spend: Box::new(PartialUploadSpend {
                        storage_cost_atto: total_storage.to_string(),
                        gas_cost_wei: total_gas,
                    }),
                    reason: "final wave store failed after retries".into(),
                });
            }
        }

        debug!("Batch upload complete: {} addresses", all_addresses.len());
        Ok((
            all_addresses,
            total_storage.to_string(),
            total_gas,
            agg_stats,
        ))
    }

    /// Prepare a wave of chunks by collecting quotes concurrently.
    ///
    /// Fires [`UploadEvent::ChunkQuoted`] as each chunk's quote completes.
    /// Returns `(prepared_chunks, already_stored_addresses)`.
    async fn prepare_wave(
        &self,
        chunks: Vec<Bytes>,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        quoted_offset: usize,
        file_total: usize,
    ) -> Result<(Vec<PreparedChunk>, Vec<XorName>)> {
        let chunk_count = chunks.len();
        let chunks_with_addr: Vec<(Bytes, XorName)> = chunks
            .into_iter()
            .map(|c| {
                let addr = compute_address(&c);
                (c, addr)
            })
            .collect();

        let quote_limiter = self.controller().quote.clone();
        // Batch-aware fan-out: clamp to chunk_count so we never
        // pay for fan-out slots we cannot fill on a partial wave.
        // See PERF-RESULTS.md — measured ~30% slowdown when
        // cap > batch size on quoting workloads (live mainnet).
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

        let mut prepared = Vec::with_capacity(chunk_count);
        let mut already_stored = Vec::new();
        let mut quoted_count = 0usize;

        while let Some((address, result)) = quote_stream.next().await {
            let chunk_already_stored = result.as_ref().is_ok_and(|r| r.is_none());
            match result? {
                Some(chunk) => prepared.push(chunk),
                None => already_stored.push(address),
            }
            quoted_count += 1;
            let progress_num = quoted_offset + quoted_count;
            if file_total > 0 {
                if chunk_already_stored {
                    info!("Verified {progress_num}/{file_total} (already stored)");
                } else {
                    info!("Quoted {progress_num}/{file_total}");
                }
            }
            if let Some(tx) = progress {
                let _ = tx.try_send(UploadEvent::ChunkQuoted {
                    quoted: progress_num,
                    total: file_total,
                });
            }
        }

        Ok((prepared, already_stored))
    }

    /// Store a batch of paid chunks concurrently to their close groups.
    ///
    /// Retries failed chunks up to 3 times with exponential backoff (500ms, 1s, 2s).
    /// Returns a [`WaveResult`] with both successes and failures so callers can
    /// track partial progress instead of losing information about stored chunks.
    ///
    /// When `progress` is `Some`, sends [`UploadEvent::ChunkStored`] as each
    /// chunk is successfully stored. `stored_before` is the count of chunks
    /// already stored in previous waves so the event reports an accurate
    /// cumulative total; `total_chunks` is the total across all waves. Pass
    /// `None`/0/0 when progress reporting is not needed.
    pub(crate) async fn store_paid_chunks_with_events(
        &self,
        paid_chunks: Vec<PaidChunk>,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        stored_before: usize,
        total_chunks: usize,
    ) -> WaveResult {
        const MAX_RETRIES: u32 = 3;
        const BASE_DELAY_MS: u64 = 500;

        let mut stored = Vec::new();
        let mut to_retry = paid_chunks;

        // Per-chunk first-seen timestamps, keyed by chunk address.
        // Inserted on first sight; never overwritten so wall-clock spans
        // first attempt → eventual success across all retry rounds.
        let mut first_seen: HashMap<XorName, Instant> = HashMap::with_capacity(to_retry.len());
        for chunk in &to_retry {
            first_seen.entry(chunk.address).or_insert_with(Instant::now);
        }

        // Bound concurrency by IN-FLIGHT BYTES, not just chunk count. Each
        // concurrently-stored chunk is held in memory while it is sent to its
        // close group, and the send path re-serializes the body once per peer,
        // so a wave of large (~4 MB) chunks at full store concurrency can pin
        // multiple GB and OOM a small host. Cap how many chunks store at once
        // so their combined body size stays under the budget; small chunks are
        // unaffected (the byte bound exceeds the chunk-count bound). The budget
        // is deliberately conservative for the current per-peer send
        // amplification and can be raised once that is reduced upstream.
        let max_chunk_bytes = to_retry.iter().map(|c| c.content.len()).max().unwrap_or(0);
        // `checked_div` yields `None` only when `max_chunk_bytes == 0` (an
        // empty/zero-length wave), in which case there is no byte limit.
        let byte_bound = STORE_INFLIGHT_BYTE_BUDGET
            .checked_div(max_chunk_bytes)
            .map_or(usize::MAX, |n| n.max(1));

        let mut chunk_attempts_total: usize = 0;
        let mut store_durations_ms: Vec<u64> = Vec::new();
        let mut retries_per_chunk: Vec<u32> = Vec::new();

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(BASE_DELAY_MS * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
                info!(
                    "Retry attempt {attempt}/{MAX_RETRIES} for {} chunks",
                    to_retry.len()
                );
            }

            // Each chunk in this round counts as one store-RPC attempt.
            chunk_attempts_total = chunk_attempts_total.saturating_add(to_retry.len());

            let store_limiter = self.controller().store.clone();
            let store_concurrency = store_limiter
                .current()
                .min(to_retry.len().max(1))
                .min(byte_bound);
            let mut upload_stream = stream::iter(to_retry)
                .map(|chunk| {
                    let chunk_clone = chunk.clone();
                    let limiter = store_limiter.clone();
                    async move {
                        let result = observe_op(
                            &limiter,
                            || async move {
                                self.chunk_put_to_close_group(
                                    chunk.content,
                                    chunk.proof_bytes,
                                    &chunk.quoted_peers,
                                )
                                .await
                            },
                            classify_error,
                        )
                        .await;
                        (chunk_clone, result)
                    }
                })
                .buffer_unordered(store_concurrency);

            let mut failed_this_round = Vec::new();
            while let Some((chunk, result)) = upload_stream.next().await {
                match result {
                    Ok(name) => {
                        let duration_ms = first_seen
                            .get(&chunk.address)
                            .map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX))
                            .unwrap_or(0);
                        store_durations_ms.push(duration_ms);
                        retries_per_chunk.push(attempt);
                        stored.push(name);
                        let stored_num = stored_before + stored.len();
                        if total_chunks > 0 {
                            info!("Stored {stored_num}/{total_chunks}");
                        }
                        if let Some(tx) = progress {
                            let _ = tx.try_send(UploadEvent::ChunkStored {
                                stored: stored_num,
                                total: total_chunks,
                            });
                        }
                    }
                    Err(e) => failed_this_round.push((chunk, e.to_string())),
                }
            }

            if failed_this_round.is_empty() {
                let result = WaveResult {
                    stored,
                    failed: Vec::new(),
                    chunk_attempts_total,
                    store_durations_ms,
                    retries_per_chunk,
                };
                log_wave_summary(&result);
                return result;
            }

            if attempt == MAX_RETRIES {
                let failed = failed_this_round
                    .into_iter()
                    .map(|(c, e)| (c.address, e))
                    .collect();
                let result = WaveResult {
                    stored,
                    failed,
                    chunk_attempts_total,
                    store_durations_ms,
                    retries_per_chunk,
                };
                log_wave_summary(&result);
                return result;
            }

            warn!(
                "{} chunks failed on attempt {}, will retry",
                failed_this_round.len(),
                attempt + 1
            );
            to_retry = failed_this_round.into_iter().map(|(c, _)| c).collect();
        }

        // Unreachable due to loop structure, but satisfy the compiler.
        let result = WaveResult {
            stored,
            failed: Vec::new(),
            chunk_attempts_total,
            store_durations_ms,
            retries_per_chunk,
        };
        log_wave_summary(&result);
        result
    }
}

/// Emit one structured info line summarising a wave's store-side stats.
///
/// Surfaces p50/p95/max chunk wall-clock and per-round retry counts so
/// log-based analysis tooling (Elasticsearch / Kibana) can identify
/// client-side quorum or retry cost without needing the `--json` output.
fn log_wave_summary(result: &WaveResult) {
    let retries_round_1 = result.retries_per_chunk.iter().filter(|&&r| r == 1).count();
    let retries_round_2 = result.retries_per_chunk.iter().filter(|&&r| r == 2).count();
    let retries_round_3 = result.retries_per_chunk.iter().filter(|&&r| r == 3).count();
    let chunk_attempts_total = result.chunk_attempts_total;
    info!(
        chunks_stored = result.stored.len(),
        chunks_failed = result.failed.len(),
        chunk_attempts_total,
        retries_round_1,
        retries_round_2,
        retries_round_3,
        store_duration_p50_ms = percentile(&result.store_durations_ms, 0.50),
        store_duration_p95_ms = percentile(&result.store_durations_ms, 0.95),
        store_duration_max_ms = result.store_durations_ms.iter().max().copied().unwrap_or(0),
        "chunk_store_wave_complete"
    );
}

/// Returns true when a cached proof's embedded quote prices still meet
/// the current fresh quote floor selected for this chunk.
fn cached_proof_matches_current_quote_floor(proof_bytes: &[u8], prepared: &PreparedChunk) -> bool {
    let Some(current_floor) = prepared
        .payment
        .quotes
        .iter()
        .map(|quote| quote.price)
        .max()
    else {
        return false;
    };

    match deserialize_proof(proof_bytes) {
        Ok((proof, _tx_hashes)) => proof_matches_current_quote_floor(&proof, current_floor),
        Err(_) => false,
    }
}

/// A cached proof can only be replayed if every embedded quote is at least
/// as expensive as the fresh quote floor. With one paid quote this is the
/// exact check; with older multi-quote proofs it is deliberately
/// conservative because the storer may derive its payable quote from the
/// full embedded set.
fn proof_matches_current_quote_floor(proof: &ProofOfPayment, current_floor: Amount) -> bool {
    !proof.peer_quotes.is_empty()
        && proof
            .peer_quotes
            .iter()
            .all(|(_, quote)| quote.price >= current_floor)
}

/// Safety margin subtracted from the storer's `QUOTE_MAX_AGE_SECS` (24 h)
/// when deciding to trust a cached proof.
///
/// A proof whose oldest `quote.timestamp` is closer than this to the
/// storer's hard limit is treated as already-expired locally. The
/// margin covers (a) clock skew between client and storer, (b) the
/// in-flight time between the local check and the storer's
/// `validate_quote_timestamps` call, and (c) the time spent uploading
/// the chunk body. 5 minutes is generous for all three combined and
/// cheap: a wrongly-kept proof costs an extra retry round trip, a
/// wrongly-dropped proof costs one re-pay (cheap chunk).
const CACHED_PROOF_SAFETY_MARGIN_SECS: u64 = 300;

/// Storer-side budget for a quote's age. Mirrors `QUOTE_MAX_AGE_SECS`
/// in `ant-node/src/payment/verifier.rs`. If this value drifts on the
/// node side, the worst case is the client either keeps proofs slightly
/// past the storer limit (forced re-pay on next retry, no money lost)
/// or drops them slightly early (one extra re-pay, no money lost).
/// Either way, no payment is double-spent or stranded.
const CACHED_PROOF_MAX_AGE_SECS: u64 = 24 * 60 * 60;

/// How far a cached quote's `timestamp` may be in the future before we
/// classify it as too-skewed-to-trust and prune.
///
/// Mirrors `QUOTE_FUTURE_SKEW_TOLERANCE_SECS = 300` in
/// `ant-node/src/payment/verifier.rs`. If the client's clock runs
/// slow relative to the storer that issued the quote, a perfectly
/// valid proof can appear future-dated to the client — rejecting any
/// forward drift would re-pay those chunks on every retry. Allow the
/// same 5-minute window the storer does so the client and node agree
/// on which proofs are fresh.
const CACHED_PROOF_FUTURE_SKEW_TOLERANCE_SECS: u64 = 300;

/// Drop cached `proof_bytes` whose quote timestamps are too close to
/// the storer's expiry window to safely reuse.
///
/// Why this exists
/// ---------------
/// The cache stores `(chunk_address, proof_bytes)` so a retried upload
/// can skip re-paying. The proof bytes embed `quote.timestamp`s. Each
/// storer evaluates each `quote.timestamp` independently against its
/// 24 h `QUOTE_MAX_AGE_SECS` budget, so close to the 24 h boundary
/// (or on a multi-day-old cache that survived past the receipt's outer
/// expiry for some reason) the storer rejects what the client still
/// believes is fresh.
///
/// The previous design trusted a substring match on the storer's
/// returned error text to detect this and invalidate the cache after
/// the fact. That allowed a Byzantine storer to spoof the marker and
/// force the client to re-pay fresh proofs (double-payment). This
/// implementation is decision-pure: we decode the proof locally and
/// only re-use it if every embedded quote is comfortably within the
/// budget. No remote text involved.
///
/// Side-effect: dropped entries are removed from the on-disk cache so
/// they don't reappear on the next load.
fn prune_locally_expired_proofs(
    resume_key: &str,
    proofs: HashMap<[u8; 32], Vec<u8>>,
) -> HashMap<XorName, Vec<u8>> {
    let now = std::time::SystemTime::now();
    let max_safe_age = Duration::from_secs(
        CACHED_PROOF_MAX_AGE_SECS.saturating_sub(CACHED_PROOF_SAFETY_MARGIN_SECS),
    );
    let max_future_skew = Duration::from_secs(CACHED_PROOF_FUTURE_SKEW_TOLERANCE_SECS);
    let mut kept: HashMap<XorName, Vec<u8>> = HashMap::with_capacity(proofs.len());
    // Pair each expired address with the EXACT bytes we observed at
    // load time. The cache-side drop only removes the entry if those
    // bytes still match, so a concurrent re-pay that refreshed the
    // proof under its own lock is not clobbered (CAS semantics, fixes
    // the TOCTOU between unlocked-load and locked-drop).
    let mut expired: Vec<([u8; 32], Vec<u8>)> = Vec::new();
    for (addr, bytes) in proofs {
        match deserialize_proof(&bytes) {
            Ok((proof, _tx_hashes)) => {
                if proof_is_safely_fresh(&proof, now, max_safe_age, max_future_skew) {
                    kept.insert(addr, bytes);
                } else {
                    expired.push((addr, bytes));
                }
            }
            Err(_) => {
                // Unreadable cached entry: drop it so it doesn't sit
                // here forever. The chunk will re-quote+re-pay.
                expired.push((addr, bytes));
            }
        }
    }
    if !expired.is_empty() {
        info!(
            "Pruning {} stale cached proofs (quote.timestamp past safe-reuse window) \
             before resume",
            expired.len()
        );
        crate::data::client::cached_single::try_drop_proofs_for_file(resume_key, &expired);
    }
    kept
}

/// True iff every quote in the proof has a timestamp not older than
/// `now - max_safe_age` AND not further in the future than
/// `max_future_skew`. The forward-skew check mirrors the storer's
/// `QUOTE_FUTURE_SKEW_TOLERANCE_SECS` (300s) so a slow-running client
/// clock doesn't cause us to wrongly prune perfectly fresh proofs
/// that the storer would still accept.
fn proof_is_safely_fresh(
    proof: &ProofOfPayment,
    now: std::time::SystemTime,
    max_safe_age: Duration,
    max_future_skew: Duration,
) -> bool {
    for (_peer, quote) in &proof.peer_quotes {
        match now.duration_since(quote.timestamp) {
            Ok(age) => {
                if age > max_safe_age {
                    return false;
                }
            }
            Err(future) => {
                if future.duration() > max_future_skew {
                    return false;
                }
            }
        }
    }
    true
}

/// Compile-time assertions that batch method futures are Send.
#[cfg(test)]
mod send_assertions {
    use super::*;

    fn _assert_send<T: Send>(_: &T) {}

    #[allow(dead_code)]
    async fn _batch_upload_is_send(client: &Client) {
        let fut = client.batch_upload_chunks(Vec::new());
        _assert_send(&fut);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::time::SystemTime;
    use xor_name::XorName as QuoteXorName;

    const TEST_QUOTE_HASH_SEED: u8 = 1;
    const TEST_PEER_SEED: u8 = 7;
    const TEST_REWARDS_SEED: u8 = 10;

    /// Helper: build a `PreparedChunk` with one selected paid quote.
    fn make_prepared_chunk(payment_amount: u64) -> PreparedChunk {
        let quote = QuotePaymentInfo {
            quote_hash: QuoteHash::from([TEST_QUOTE_HASH_SEED; 32]),
            rewards_address: RewardsAddress::new([TEST_REWARDS_SEED; 20]),
            amount: Amount::from(payment_amount),
            price: Amount::from(payment_amount),
        };
        let peer_quote = PaymentQuote {
            content: QuoteXorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: Amount::from(payment_amount),
            rewards_address: RewardsAddress::new([TEST_REWARDS_SEED; 20]),
            pub_key: Vec::new(),
            signature: Vec::new(),
        };
        PreparedChunk {
            content: Bytes::from(vec![0xAA; 32]),
            address: [0u8; 32],
            quoted_peers: Vec::new(),
            payment: PreparedChunkPayment {
                quotes: vec![quote],
            },
            peer_quotes: vec![(EncodedPeerId::from([TEST_PEER_SEED; 32]), peer_quote)],
        }
    }

    #[test]
    fn payment_intent_from_single_chunk() {
        let chunk = make_prepared_chunk(300);
        let intent = PaymentIntent::from_prepared_chunks(&[chunk]);

        assert_eq!(intent.payments.len(), 1, "only non-zero amounts");
        assert_eq!(intent.total_amount, Amount::from(300));

        let (hash, addr, amt) = &intent.payments[0];
        assert_eq!(*hash, QuoteHash::from([TEST_QUOTE_HASH_SEED; 32]));
        assert_eq!(*addr, RewardsAddress::new([TEST_REWARDS_SEED; 20]));
        assert_eq!(*amt, Amount::from(300));
    }

    #[test]
    fn payment_intent_from_multiple_chunks() {
        let c1 = make_prepared_chunk(100);
        let c2 = make_prepared_chunk(250);
        let intent = PaymentIntent::from_prepared_chunks(&[c1, c2]);

        assert_eq!(intent.payments.len(), 2);
        assert_eq!(intent.total_amount, Amount::from(350));
    }

    #[test]
    fn payment_intent_skips_all_zero_chunks() {
        let chunk = make_prepared_chunk(0);
        let intent = PaymentIntent::from_prepared_chunks(&[chunk]);

        assert!(intent.payments.is_empty());
        assert_eq!(intent.total_amount, Amount::ZERO);
    }

    #[test]
    fn payment_intent_empty_input() {
        let intent = PaymentIntent::from_prepared_chunks(&[]);
        assert!(intent.payments.is_empty());
        assert_eq!(intent.total_amount, Amount::ZERO);
    }

    #[test]
    fn finalize_batch_payment_builds_proofs() {
        let chunk = make_prepared_chunk(500);
        let quote_hash = chunk.payment.quotes[0].quote_hash;

        let mut tx_map = HashMap::new();
        tx_map.insert(quote_hash, TxHash::from([0xBB; 32]));

        let paid = finalize_batch_payment(vec![chunk], &tx_map).unwrap();

        assert_eq!(paid.len(), 1);
        assert!(!paid[0].proof_bytes.is_empty());
        assert_eq!(paid[0].address, [0u8; 32]);

        let (proof, tx_hashes) = deserialize_proof(&paid[0].proof_bytes).unwrap();
        assert_eq!(proof.peer_quotes.len(), 1);
        assert_eq!(
            proof.peer_quotes[0].0,
            EncodedPeerId::from([TEST_PEER_SEED; 32])
        );
        assert_eq!(tx_hashes, vec![TxHash::from([0xBB; 32])]);
    }

    #[test]
    fn finalize_batch_payment_empty_input() {
        let paid = finalize_batch_payment(vec![], &HashMap::new()).unwrap();
        assert!(paid.is_empty());
    }

    #[test]
    fn finalize_batch_payment_missing_tx_hash_errors() {
        // Missing tx hash for a non-zero-amount quote should error,
        // since the chunk would be rejected by the network without a valid proof.
        let chunk = make_prepared_chunk(500);

        let result = finalize_batch_payment(vec![chunk], &HashMap::new());
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Missing tx hash"), "got: {err}");
    }

    #[test]
    fn finalize_batch_payment_multiple_chunks() {
        let c1 = make_prepared_chunk(100);
        let c2 = make_prepared_chunk(200);
        let q1 = c1.payment.quotes[0].quote_hash;
        let mut tx_map = HashMap::new();
        // Both chunks have the same quote_hash (same index/byte pattern)
        // so one tx_hash covers both
        tx_map.insert(q1, TxHash::from([0xCC; 32]));

        let paid = finalize_batch_payment(vec![c1, c2], &tx_map).unwrap();
        assert_eq!(paid.len(), 2);
    }

    // ---- prune_locally_expired_proofs ----
    //
    // Build synthetic ProofOfPayment instances with controlled
    // timestamps to verify the local pre-flight stale-proof check.
    // This is the "no remote text trust" replacement for the prior
    // substring-matching invalidation path. A bug here is a direct
    // wallet leak (drop-too-eager = re-pay; keep-too-long = doomed
    // PUT round trip but no payment loss).

    fn make_proof_with_timestamps(timestamps: &[std::time::SystemTime]) -> ProofOfPayment {
        let peer_quotes = timestamps
            .iter()
            .enumerate()
            .map(|(i, ts)| {
                let quote = PaymentQuote {
                    content: xor_name::XorName([0u8; 32]),
                    timestamp: *ts,
                    price: Amount::from(1u64),
                    rewards_address: RewardsAddress::new([1u8; 20]),
                    pub_key: vec![],
                    signature: vec![],
                };
                (EncodedPeerId::from([i as u8; 32]), quote)
            })
            .collect();
        ProofOfPayment { peer_quotes }
    }

    fn make_proof_with_prices(prices: &[u64]) -> ProofOfPayment {
        let peer_quotes = prices
            .iter()
            .enumerate()
            .map(|(i, price)| {
                let quote = PaymentQuote {
                    content: xor_name::XorName([0u8; 32]),
                    timestamp: SystemTime::UNIX_EPOCH,
                    price: Amount::from(*price),
                    rewards_address: RewardsAddress::new([1u8; 20]),
                    pub_key: vec![],
                    signature: vec![],
                };
                (EncodedPeerId::from([i as u8; 32]), quote)
            })
            .collect();
        ProofOfPayment { peer_quotes }
    }

    #[test]
    fn proof_matches_current_quote_floor_accepts_current_or_higher_price() {
        let proof = make_proof_with_prices(&[120]);

        assert!(proof_matches_current_quote_floor(&proof, Amount::from(100)));
    }

    #[test]
    fn proof_matches_current_quote_floor_rejects_stale_lower_price() {
        let proof = make_proof_with_prices(&[80]);

        assert!(!proof_matches_current_quote_floor(
            &proof,
            Amount::from(100)
        ));
    }

    #[test]
    fn proof_matches_current_quote_floor_rejects_any_lower_multi_quote_price() {
        let proof = make_proof_with_prices(&[100, 80, 130]);

        assert!(!proof_matches_current_quote_floor(
            &proof,
            Amount::from(100)
        ));
    }

    #[test]
    fn proof_matches_current_quote_floor_rejects_empty_proof() {
        let proof = make_proof_with_prices(&[]);

        assert!(!proof_matches_current_quote_floor(
            &proof,
            Amount::from(100)
        ));
    }

    fn default_max_future_skew() -> Duration {
        Duration::from_secs(CACHED_PROOF_FUTURE_SKEW_TOLERANCE_SECS)
    }

    #[test]
    fn proof_is_safely_fresh_accepts_recent_quote() {
        let proof = make_proof_with_timestamps(&[std::time::SystemTime::now()]);
        assert!(proof_is_safely_fresh(
            &proof,
            std::time::SystemTime::now(),
            Duration::from_secs(CACHED_PROOF_MAX_AGE_SECS),
            default_max_future_skew(),
        ));
    }

    #[test]
    fn proof_is_safely_fresh_rejects_quote_past_safe_window() {
        // 23h57m old: past the 24h - 5min safe-reuse threshold but
        // still within the storer's hard 24h limit. The whole point
        // of the safety margin is to drop these locally before
        // burning a doomed PUT round trip.
        let too_old = std::time::SystemTime::now() - Duration::from_secs(23 * 60 * 60 + 57 * 60);
        let proof = make_proof_with_timestamps(&[too_old]);
        let max_safe = Duration::from_secs(
            CACHED_PROOF_MAX_AGE_SECS.saturating_sub(CACHED_PROOF_SAFETY_MARGIN_SECS),
        );
        assert!(
            !proof_is_safely_fresh(
                &proof,
                std::time::SystemTime::now(),
                max_safe,
                default_max_future_skew(),
            ),
            "23h57m-old quote must fail safe-reuse check (limit is 24h - 5min margin)"
        );
    }

    #[test]
    fn proof_is_safely_fresh_rejects_if_any_quote_is_stale() {
        // The storer rejects on a per-quote basis: a proof with even
        // one stale quote will fail on every retry. We must drop it.
        let now = std::time::SystemTime::now();
        let fresh = now;
        let stale = now - Duration::from_secs(CACHED_PROOF_MAX_AGE_SECS);
        let proof = make_proof_with_timestamps(&[fresh, fresh, stale, fresh]);
        let max_safe = Duration::from_secs(
            CACHED_PROOF_MAX_AGE_SECS.saturating_sub(CACHED_PROOF_SAFETY_MARGIN_SECS),
        );
        assert!(!proof_is_safely_fresh(
            &proof,
            now,
            max_safe,
            default_max_future_skew(),
        ));
    }

    #[test]
    fn proof_is_safely_fresh_accepts_slight_future_skew_within_node_tolerance() {
        // Client clock 60s slow. Quote claims 60s in the future of
        // our local view. Node tolerates 300s forward skew, so the
        // storer would accept this quote — we must too, or we'd
        // wrongly prune fresh proofs and force re-payment.
        let now = std::time::SystemTime::now();
        let slight_future = now + Duration::from_secs(60);
        let proof = make_proof_with_timestamps(&[slight_future]);
        let max_safe = Duration::from_secs(CACHED_PROOF_MAX_AGE_SECS);
        assert!(
            proof_is_safely_fresh(&proof, now, max_safe, default_max_future_skew()),
            "60s-future quote must be accepted (within node's 300s skew tolerance)"
        );
    }

    #[test]
    fn proof_is_safely_fresh_rejects_far_future_dated_quote() {
        // 1 hour in the future of our local clock. Exceeds the
        // node's 300s forward-skew tolerance and the storer would
        // reject it — we drop it locally to avoid a round trip.
        let now = std::time::SystemTime::now();
        let far_future = now + Duration::from_secs(3600);
        let proof = make_proof_with_timestamps(&[far_future]);
        let max_safe = Duration::from_secs(CACHED_PROOF_MAX_AGE_SECS);
        assert!(!proof_is_safely_fresh(
            &proof,
            now,
            max_safe,
            default_max_future_skew(),
        ));
    }

    #[test]
    fn proof_is_safely_fresh_empty_quotes_is_vacuously_safe() {
        // No quotes = no storer-side timestamp check to fail. The
        // proof is structurally invalid for other reasons, but
        // this function's contract is "no stale timestamp present",
        // which is trivially true for an empty list.
        let proof = make_proof_with_timestamps(&[]);
        assert!(proof_is_safely_fresh(
            &proof,
            std::time::SystemTime::now(),
            Duration::from_secs(CACHED_PROOF_MAX_AGE_SECS),
            default_max_future_skew(),
        ));
    }
}
