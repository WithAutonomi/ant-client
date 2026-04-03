//! Batch chunk upload with wave-based pipelined EVM payments.
//!
//! Groups chunks into waves of 64 and pays for each
//! wave in a single EVM transaction. Stores from wave N are pipelined
//! with quote collection for wave N+1 via `tokio::join!`.

use crate::data::client::payment::peer_id_to_encoded;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_node::ant_protocol::DATA_TYPE_CHUNK;
use ant_node::client::{compute_address, XorName};
use ant_node::core::{MultiAddr, PeerId};
use ant_node::payment::{serialize_single_node_proof, PaymentProof, SingleNodePayment};
use bytes::Bytes;
use evmlib::common::{Amount, QuoteHash, TxHash};
use evmlib::wallet::PayForQuotesError;
use evmlib::{EncodedPeerId, PaymentQuote, ProofOfPayment, RewardsAddress};
use futures::stream::{self, StreamExt};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use tracing::{debug, info, warn};

/// Number of chunks per payment wave.
const PAYMENT_WAVE_SIZE: usize = 64;

/// Chunk quoted but not yet paid. Produced by [`Client::prepare_chunk_payment`].
#[derive(Debug)]
pub struct PreparedChunk {
    /// The chunk content bytes.
    pub content: Bytes,
    /// Content address (BLAKE3 hash).
    pub address: XorName,
    /// Closest peers from quote collection — PUT targets for close-group replication.
    pub quoted_peers: Vec<(PeerId, Vec<MultiAddr>)>,
    /// Payment structure (quotes sorted, median selected, not yet paid on-chain).
    pub payment: SingleNodePayment,
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

        let quotes_with_peers = match self
            .get_store_quotes(&address, data_size, DATA_TYPE_CHUNK)
            .await
        {
            Ok(quotes) => quotes,
            Err(Error::AlreadyStored) => {
                debug!("Chunk {} already stored, skipping", hex::encode(address));
                return Ok(None);
            }
            Err(e) => return Err(e),
        };

        // Capture all quoted peers for close-group replication.
        let quoted_peers: Vec<(PeerId, Vec<MultiAddr>)> = quotes_with_peers
            .iter()
            .map(|(peer_id, addrs, _, _)| (*peer_id, addrs.clone()))
            .collect();

        // Build peer_quotes for ProofOfPayment + quotes for SingleNodePayment.
        // Use node-reported prices directly — no contract price fetch needed.
        let mut peer_quotes = Vec::with_capacity(quotes_with_peers.len());
        let mut quotes_for_payment = Vec::with_capacity(quotes_with_peers.len());

        for (peer_id, _addrs, quote, price) in quotes_with_peers {
            let encoded = peer_id_to_encoded(&peer_id)?;
            peer_quotes.push((encoded, quote.clone()));
            quotes_for_payment.push((quote, price));
        }

        let payment = SingleNodePayment::from_quotes(quotes_for_payment)
            .map_err(|e| Error::Payment(format!("Failed to create payment: {e}")))?;

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
    pub async fn batch_pay(&self, prepared: Vec<PreparedChunk>) -> Result<Vec<PaidChunk>> {
        if prepared.is_empty() {
            return Ok(Vec::new());
        }

        let wallet = self.require_wallet()?;

        // Flatten all quote payments from all chunks into a single batch.
        let total_quotes: usize = prepared.iter().map(|c| c.payment.quotes.len()).sum();
        let mut all_payments = Vec::with_capacity(total_quotes);
        for chunk in &prepared {
            for info in &chunk.payment.quotes {
                all_payments.push((info.quote_hash, info.rewards_address, info.amount));
            }
        }

        info!(
            "Batch payment for {} chunks ({} quote entries)",
            prepared.len(),
            all_payments.len()
        );

        let (tx_hash_map, _gas_info) =
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
        build_paid_chunks(prepared, &tx_hash_map)
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
    pub async fn batch_upload_chunks(&self, chunks: Vec<Bytes>) -> Result<Vec<XorName>> {
        if chunks.is_empty() {
            return Ok(Vec::new());
        }

        let total_chunks = chunks.len();
        let concurrency = self.config().chunk_concurrency;
        info!("Batch uploading {total_chunks} chunks in waves of {PAYMENT_WAVE_SIZE} (concurrency: {concurrency})");

        let mut all_addresses = Vec::with_capacity(total_chunks);
        let mut seen_addresses: HashSet<XorName> = HashSet::new();

        // Deduplicate chunks by content address.
        let mut unique_chunks = Vec::with_capacity(total_chunks);
        for chunk in chunks {
            let address = compute_address(&chunk);
            if seen_addresses.insert(address) {
                unique_chunks.push(chunk);
            } else {
                debug!("Skipping duplicate chunk {}", hex::encode(address));
                all_addresses.push(address);
            }
        }

        // Split into waves.
        let waves: Vec<Vec<Bytes>> = unique_chunks
            .chunks(PAYMENT_WAVE_SIZE)
            .map(<[Bytes]>::to_vec)
            .collect();
        let wave_count = waves.len();

        info!(
            "{total_chunks} chunks -> {} unique -> {wave_count} waves",
            seen_addresses.len()
        );

        let mut pending_store: Option<Vec<PaidChunk>> = None;

        for (wave_idx, wave_chunks) in waves.into_iter().enumerate() {
            let wave_num = wave_idx + 1;

            // Pipeline: store previous wave while preparing this one.
            let (prepare_result, store_result) = match pending_store.take() {
                Some(paid_chunks) => {
                    let (prep, stored) = tokio::join!(
                        self.prepare_wave(wave_chunks),
                        self.store_paid_chunks(paid_chunks)
                    );
                    (prep, Some(stored))
                }
                None => (self.prepare_wave(wave_chunks).await, None),
            };

            // Track partial progress from previous wave.
            if let Some(wave_result) = store_result {
                all_addresses.extend(&wave_result.stored);
                if !wave_result.failed.is_empty() {
                    let failed_count = wave_result.failed.len();
                    warn!("{failed_count} chunks failed to store after retries");
                    return Err(Error::PartialUpload {
                        stored: all_addresses.clone(),
                        stored_count: all_addresses.len(),
                        failed: wave_result.failed,
                        failed_count,
                        reason: "wave store failed after retries".into(),
                    });
                }
            }

            let (prepared_chunks, already_stored) = prepare_result?;
            all_addresses.extend(already_stored);

            if prepared_chunks.is_empty() {
                info!("Wave {wave_num}/{wave_count}: all chunks already stored");
                continue;
            }

            info!(
                "Wave {wave_num}/{wave_count}: paying for {} chunks",
                prepared_chunks.len()
            );
            let paid_chunks = self.batch_pay(prepared_chunks).await?;
            pending_store = Some(paid_chunks);
        }

        // Store the last wave.
        if let Some(paid_chunks) = pending_store {
            let wave_result = self.store_paid_chunks(paid_chunks).await;
            all_addresses.extend(&wave_result.stored);
            if !wave_result.failed.is_empty() {
                let failed_count = wave_result.failed.len();
                warn!("{failed_count} chunks failed to store after retries (final wave)");
                return Err(Error::PartialUpload {
                    stored: all_addresses.clone(),
                    stored_count: all_addresses.len(),
                    failed: wave_result.failed,
                    failed_count,
                    reason: "final wave store failed after retries".into(),
                });
            }
        }

        info!("Batch upload complete: {} addresses", all_addresses.len());
        Ok(all_addresses)
    }

    /// Prepare a wave of chunks by collecting quotes concurrently.
    ///
    /// Returns `(prepared_chunks, already_stored_addresses)`.
    async fn prepare_wave(&self, chunks: Vec<Bytes>) -> Result<(Vec<PreparedChunk>, Vec<XorName>)> {
        let chunk_count = chunks.len();
        let chunks_with_addr: Vec<(Bytes, XorName)> = chunks
            .into_iter()
            .map(|c| {
                let addr = compute_address(&c);
                (c, addr)
            })
            .collect();

        let results: Vec<(XorName, Result<Option<PreparedChunk>>)> = stream::iter(chunks_with_addr)
            .map(|(content, address)| async move {
                (address, self.prepare_chunk_payment(content).await)
            })
            .buffer_unordered(self.config().chunk_concurrency)
            .collect()
            .await;

        let mut prepared = Vec::with_capacity(chunk_count);
        let mut already_stored = Vec::new();

        for (address, result) in results {
            match result? {
                Some(chunk) => prepared.push(chunk),
                None => already_stored.push(address),
            }
        }

        Ok((prepared, already_stored))
    }

    /// Store a batch of paid chunks concurrently to their close groups.
    ///
    /// Retries failed chunks up to 3 times with exponential backoff (500ms, 1s, 2s).
    /// Returns a [`WaveResult`] with both successes and failures so callers can
    /// track partial progress instead of losing information about stored chunks.
    pub(crate) async fn store_paid_chunks(&self, paid_chunks: Vec<PaidChunk>) -> WaveResult {
        const MAX_RETRIES: u32 = 3;
        const BASE_DELAY_MS: u64 = 500;

        let mut stored = Vec::new();
        let mut to_retry = paid_chunks;

        for attempt in 0..=MAX_RETRIES {
            if attempt > 0 {
                let delay = Duration::from_millis(BASE_DELAY_MS * 2u64.pow(attempt - 1));
                tokio::time::sleep(delay).await;
                info!(
                    "Retry attempt {attempt}/{MAX_RETRIES} for {} chunks",
                    to_retry.len()
                );
            }

            let results: Vec<(PaidChunk, Result<XorName>)> = stream::iter(to_retry)
                .map(|chunk| {
                    let chunk_clone = chunk.clone();
                    async move {
                        let result = self
                            .chunk_put_to_close_group(
                                chunk.content,
                                chunk.proof_bytes,
                                &chunk.quoted_peers,
                            )
                            .await;
                        (chunk_clone, result)
                    }
                })
                .buffer_unordered(self.config().chunk_concurrency)
                .collect()
                .await;

            let mut failed_this_round = Vec::new();
            for (chunk, result) in results {
                match result {
                    Ok(name) => stored.push(name),
                    Err(e) => failed_this_round.push((chunk, e.to_string())),
                }
            }

            if failed_this_round.is_empty() {
                return WaveResult {
                    stored,
                    failed: Vec::new(),
                };
            }

            if attempt == MAX_RETRIES {
                let failed = failed_this_round
                    .into_iter()
                    .map(|(c, e)| (c.address, e))
                    .collect();
                return WaveResult { stored, failed };
            }

            warn!(
                "{} chunks failed on attempt {}, will retry",
                failed_this_round.len(),
                attempt + 1
            );
            to_retry = failed_this_round.into_iter().map(|(c, _)| c).collect();
        }

        // Unreachable due to loop structure, but satisfy the compiler.
        WaveResult {
            stored,
            failed: Vec::new(),
        }
    }
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
    use ant_node::payment::single_node::QuotePaymentInfo;
    use ant_node::CLOSE_GROUP_SIZE;

    /// Median index in the quotes array.
    const MEDIAN_INDEX: usize = CLOSE_GROUP_SIZE / 2;

    /// Helper: build a `PreparedChunk` with `median_amount` at the median
    /// quote index and zero for all other quotes. Adapts automatically to
    /// `CLOSE_GROUP_SIZE` changes.
    fn make_prepared_chunk(median_amount: u64) -> PreparedChunk {
        let quotes: [QuotePaymentInfo; CLOSE_GROUP_SIZE] = std::array::from_fn(|i| {
            let amount = if i == MEDIAN_INDEX { median_amount } else { 0 };
            QuotePaymentInfo {
                quote_hash: QuoteHash::from([i as u8 + 1; 32]),
                rewards_address: RewardsAddress::new([i as u8 + 10; 20]),
                amount: Amount::from(amount),
                price: Amount::from(amount),
            }
        });

        PreparedChunk {
            content: Bytes::from(vec![0xAA; 32]),
            address: [0u8; 32],
            quoted_peers: Vec::new(),
            payment: SingleNodePayment { quotes },
            peer_quotes: Vec::new(),
        }
    }

    #[test]
    fn payment_intent_from_single_chunk() {
        let chunk = make_prepared_chunk(300);
        let intent = PaymentIntent::from_prepared_chunks(&[chunk]);

        assert_eq!(intent.payments.len(), 1, "only non-zero amounts");
        assert_eq!(intent.total_amount, Amount::from(300));

        let (hash, addr, amt) = &intent.payments[0];
        assert_eq!(*hash, QuoteHash::from([MEDIAN_INDEX as u8 + 1; 32]));
        assert_eq!(*addr, RewardsAddress::new([MEDIAN_INDEX as u8 + 10; 20]));
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
        let quote_hash = chunk.payment.quotes[MEDIAN_INDEX].quote_hash;

        let mut tx_map = HashMap::new();
        tx_map.insert(quote_hash, TxHash::from([0xBB; 32]));

        let paid = finalize_batch_payment(vec![chunk], &tx_map).unwrap();

        assert_eq!(paid.len(), 1);
        assert!(!paid[0].proof_bytes.is_empty());
        assert_eq!(paid[0].address, [0u8; 32]);
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
        let q1 = c1.payment.quotes[MEDIAN_INDEX].quote_hash;
        let mut tx_map = HashMap::new();
        // Both chunks have the same quote_hash (same index/byte pattern)
        // so one tx_hash covers both
        tx_map.insert(q1, TxHash::from([0xCC; 32]));

        let paid = finalize_batch_payment(vec![c1, c2], &tx_map).unwrap();
        assert_eq!(paid.len(), 2);
    }
}
