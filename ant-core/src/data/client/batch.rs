//! Batch chunk upload with wave-based pipelined EVM payments.
//!
//! Groups chunks into waves of 64 and pays for each
//! wave in a single EVM transaction. Stores from wave N are pipelined
//! with quote collection for wave N+1 via `tokio::join!`.

use crate::data::client::payment::peer_id_to_encoded;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_evm::{EncodedPeerId, PaymentQuote, ProofOfPayment};
use ant_node::ant_protocol::DATA_TYPE_CHUNK;
use ant_node::client::{compute_address, XorName};
use ant_node::core::{MultiAddr, PeerId};
use ant_node::payment::{serialize_single_node_proof, PaymentProof, SingleNodePayment};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use std::collections::HashSet;
use tracing::{debug, info};

/// Number of chunks per payment wave.
const PAYMENT_WAVE_SIZE: usize = 64;

/// Chunk quoted but not yet paid. Produced by [`Client::prepare_chunk_payment`].
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

impl Client {
    /// Prepare a single chunk for batch payment.
    ///
    /// Collects quotes and fetches contract prices without making any
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

        let wallet = self.require_wallet()?;

        // Capture all quoted peers for close-group replication.
        let quoted_peers: Vec<(PeerId, Vec<MultiAddr>)> = quotes_with_peers
            .iter()
            .map(|(peer_id, addrs, _, _)| (*peer_id, addrs.clone()))
            .collect();

        // Fetch authoritative prices from the on-chain contract.
        let metrics_batch: Vec<_> = quotes_with_peers
            .iter()
            .map(|(_, _, quote, _)| quote.quoting_metrics.clone())
            .collect();

        let contract_prices =
            evmlib::contract::payment_vault::get_market_price(wallet.network(), metrics_batch)
                .await
                .map_err(|e| {
                    Error::Payment(format!("Failed to get market prices from contract: {e}"))
                })?;

        if contract_prices.len() != quotes_with_peers.len() {
            return Err(Error::Payment(format!(
                "Contract returned {} prices for {} quotes",
                contract_prices.len(),
                quotes_with_peers.len()
            )));
        }

        // Build peer_quotes for ProofOfPayment + quotes for SingleNodePayment.
        let mut peer_quotes = Vec::with_capacity(quotes_with_peers.len());
        let mut quotes_for_payment = Vec::with_capacity(quotes_with_peers.len());

        for ((peer_id, _addrs, quote, _local_price), contract_price) in
            quotes_with_peers.into_iter().zip(contract_prices)
        {
            let encoded = peer_id_to_encoded(&peer_id)?;
            peer_quotes.push((encoded, quote.clone()));
            quotes_for_payment.push((quote, contract_price));
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
        let mut all_payments =
            Vec::with_capacity(prepared.len() * prepared[0].payment.quotes.len());
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

        let (tx_hash_map, _gas_info) = wallet.pay_for_quotes(all_payments).await.map_err(
            |evmlib::wallet::PayForQuotesError(err, _)| {
                Error::Payment(format!("Batch payment failed: {err}"))
            },
        )?;

        info!(
            "Batch payment succeeded: {} transactions",
            tx_hash_map.len()
        );

        // Map tx hashes back to per-chunk PaymentProofs.
        let mut paid_chunks = Vec::with_capacity(prepared.len());
        for chunk in prepared {
            let tx_hashes: Vec<_> = chunk
                .payment
                .quotes
                .iter()
                .filter(|info| !info.amount.is_zero())
                .filter_map(|info| tx_hash_map.get(&info.quote_hash).copied())
                .collect();

            let proof = PaymentProof {
                proof_of_payment: ProofOfPayment {
                    peer_quotes: chunk.peer_quotes,
                },
                tx_hashes,
            };

            let proof_bytes = serialize_single_node_proof(&proof).map_err(|e| {
                Error::Serialization(format!("Failed to serialize payment proof: {e}"))
            })?;

            paid_chunks.push(PaidChunk {
                content: chunk.content,
                address: chunk.address,
                quoted_peers: chunk.quoted_peers,
                proof_bytes,
            });
        }

        Ok(paid_chunks)
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

            // Propagate store errors from previous wave.
            if let Some(stored) = store_result {
                all_addresses.extend(stored?);
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
            all_addresses.extend(self.store_paid_chunks(paid_chunks).await?);
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
    async fn store_paid_chunks(&self, paid_chunks: Vec<PaidChunk>) -> Result<Vec<XorName>> {
        let results: Vec<Result<XorName>> = stream::iter(paid_chunks)
            .map(|chunk| async move {
                self.chunk_put_to_close_group(chunk.content, chunk.proof_bytes, &chunk.quoted_peers)
                    .await
            })
            .buffer_unordered(self.config().chunk_concurrency)
            .collect()
            .await;

        results.into_iter().collect()
    }
}
