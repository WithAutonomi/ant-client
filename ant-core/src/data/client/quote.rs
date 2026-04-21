//! Quote and payment operations.
//!
//! Handles requesting storage quotes from network nodes and
//! managing payment for data storage.

use crate::data::client::peer_cache::record_peer_outcome;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_node::ant_protocol::{
    ChunkMessage, ChunkMessageBody, ChunkQuoteRequest, ChunkQuoteResponse,
};
use ant_node::client::send_and_await_chunk_response;
use ant_node::core::{MultiAddr, PeerId};
use ant_node::{CLOSE_GROUP_MAJORITY, CLOSE_GROUP_SIZE};
use evmlib::common::Amount;
use evmlib::PaymentQuote;
use futures::stream::{FuturesUnordered, StreamExt};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Compute XOR distance between a peer's ID bytes and a target address.
///
/// Uses the first 32 bytes of the peer ID (or fewer if shorter) XORed with
/// the target address. Returns a byte array suitable for lexicographic comparison.
fn xor_distance(peer_id: &PeerId, target: &[u8; 32]) -> [u8; 32] {
    let peer_bytes = peer_id.as_bytes();
    let mut distance = [0u8; 32];
    for (i, d) in distance.iter_mut().enumerate() {
        let pb = peer_bytes.get(i).copied().unwrap_or(0);
        *d = pb ^ target[i];
    }
    distance
}

impl Client {
    /// Get storage quotes from the closest peers for a given address.
    ///
    /// Queries 2x `CLOSE_GROUP_SIZE` peers from the DHT for fault tolerance,
    /// requests quotes from all of them concurrently, and returns the
    /// `CLOSE_GROUP_SIZE` closest successful responders sorted by XOR distance.
    ///
    /// Returns `Error::AlreadyStored` early if `CLOSE_GROUP_MAJORITY` peers
    /// report the chunk is already stored.
    ///
    /// # Errors
    ///
    /// Returns an error if insufficient quotes can be collected.
    #[allow(clippy::too_many_lines)]
    pub async fn get_store_quotes(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>> {
        let node = self.network().node();

        // Over-query for fault tolerance: ask 2x peers, keep closest successful ones.
        let over_query_count = CLOSE_GROUP_SIZE * 2;
        debug!(
            "Requesting quotes from up to {over_query_count} peers for address {} (size: {data_size})",
            hex::encode(address)
        );

        let remote_peers = self
            .network()
            .find_closest_peers(address, over_query_count)
            .await?;

        if remote_peers.len() < CLOSE_GROUP_SIZE {
            return Err(Error::InsufficientPeers(format!(
                "Found {} peers, need {CLOSE_GROUP_SIZE}",
                remote_peers.len()
            )));
        }

        let per_peer_timeout = Duration::from_secs(self.config().quote_timeout_secs);
        // Overall timeout for collecting all quotes. Must accommodate
        // connect_with_fallback cascade (direct 5s + hole-punch 15s×3 + relay 30s ≈ 80s)
        // plus the per-peer quote timeout. 120s is generous.
        let overall_timeout = Duration::from_secs(120);

        // Request quotes from all peers concurrently
        let mut quote_futures = FuturesUnordered::new();

        for (peer_id, peer_addrs) in &remote_peers {
            let request_id = self.next_request_id();
            let request = ChunkQuoteRequest {
                address: *address,
                data_size,
                data_type,
            };
            let message = ChunkMessage {
                request_id,
                body: ChunkMessageBody::QuoteRequest(request),
            };

            let message_bytes = match message.encode() {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("Failed to encode quote request for {peer_id}: {e}");
                    continue;
                }
            };

            let peer_id_clone = *peer_id;
            let addrs_clone = peer_addrs.clone();
            let node_clone = node.clone();

            let quote_future = async move {
                let start = Instant::now();
                let result = send_and_await_chunk_response(
                    &node_clone,
                    &peer_id_clone,
                    message_bytes,
                    request_id,
                    per_peer_timeout,
                    &addrs_clone,
                    |body| match body {
                        ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Success {
                            quote,
                            already_stored,
                        }) => {
                            if already_stored {
                                debug!("Peer {peer_id_clone} already has chunk");
                                return Some(Err(Error::AlreadyStored));
                            }
                            match rmp_serde::from_slice::<PaymentQuote>(&quote) {
                                Ok(payment_quote) => {
                                    let price = payment_quote.price;
                                    debug!("Received quote from {peer_id_clone}: price = {price}");
                                    Some(Ok((payment_quote, price)))
                                }
                                Err(e) => Some(Err(Error::Serialization(format!(
                                    "Failed to deserialize quote from {peer_id_clone}: {e}"
                                )))),
                            }
                        }
                        ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Error(e)) => Some(Err(
                            Error::Protocol(format!("Quote error from {peer_id_clone}: {e}")),
                        )),
                        _ => None,
                    },
                    |e| {
                        Error::Network(format!(
                            "Failed to send quote request to {peer_id_clone}: {e}"
                        ))
                    },
                    || Error::Timeout(format!("Timeout waiting for quote from {peer_id_clone}")),
                )
                .await;

                let success = result.is_ok();
                let rtt_ms = success.then(|| start.elapsed().as_millis() as u64);
                record_peer_outcome(&node_clone, peer_id_clone, &addrs_clone, success, rtt_ms)
                    .await;

                (peer_id_clone, addrs_clone, result)
            };

            quote_futures.push(quote_future);
        }

        // Collect all responses with an overall timeout to prevent indefinite stalls.
        // Over-query means we have 2x peers, so we can tolerate failures.
        let mut quotes = Vec::with_capacity(over_query_count);
        let mut already_stored_peers: Vec<(PeerId, [u8; 32])> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        let collect_result: std::result::Result<std::result::Result<(), Error>, _> =
            tokio::time::timeout(overall_timeout, async {
                while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
                    match quote_result {
                        Ok((quote, price)) => {
                            quotes.push((peer_id, addrs, quote, price));
                        }
                        Err(Error::AlreadyStored) => {
                            debug!("Peer {peer_id} reports chunk already stored");
                            let dist = xor_distance(&peer_id, address);
                            already_stored_peers.push((peer_id, dist));
                        }
                        Err(e) => {
                            warn!("Failed to get quote from {peer_id}: {e}");
                            failures.push(format!("{peer_id}: {e}"));
                        }
                    }
                }
                Ok(())
            })
            .await;

        match collect_result {
            Err(_elapsed) => {
                warn!(
                    "Quote collection timed out after {overall_timeout:?} for address {}",
                    hex::encode(address)
                );
                // Fall through to check if we have enough quotes despite timeout.
                // The timeout fires when slow peers haven't responded yet, but we
                // may already have enough successful quotes from fast peers.
            }
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) => {}
        }

        // Check already-stored: only count votes from the closest CLOSE_GROUP_SIZE peers.
        if !already_stored_peers.is_empty() {
            let mut all_peers_by_distance: Vec<(bool, [u8; 32])> = Vec::new();
            for (peer_id, _, _, _) in &quotes {
                all_peers_by_distance.push((false, xor_distance(peer_id, address)));
            }
            for (_, dist) in &already_stored_peers {
                all_peers_by_distance.push((true, *dist));
            }
            all_peers_by_distance.sort_by_key(|a| a.1);

            let close_group_stored = all_peers_by_distance
                .iter()
                .take(CLOSE_GROUP_SIZE)
                .filter(|(is_stored, _)| *is_stored)
                .count();

            if close_group_stored >= CLOSE_GROUP_MAJORITY {
                debug!(
                    "Chunk {} already stored ({close_group_stored}/{CLOSE_GROUP_SIZE} close-group peers confirm)",
                    hex::encode(address)
                );
                return Err(Error::AlreadyStored);
            }
        }

        if quotes.len() >= CLOSE_GROUP_SIZE {
            let total_responses = quotes.len() + failures.len() + already_stored_peers.len();

            // Sort by XOR distance to target, keep the closest CLOSE_GROUP_SIZE.
            quotes.sort_by(|a, b| {
                let dist_a = xor_distance(&a.0, address);
                let dist_b = xor_distance(&b.0, address);
                dist_a.cmp(&dist_b)
            });
            quotes.truncate(CLOSE_GROUP_SIZE);

            debug!(
                "Collected {} quotes for address {} (from {total_responses} responses)",
                quotes.len(),
                hex::encode(address),
            );
            return Ok(quotes);
        }

        Err(Error::InsufficientPeers(format!(
            "Got {} quotes, need {CLOSE_GROUP_SIZE}. Failures: [{}]",
            quotes.len(),
            failures.join("; ")
        )))
    }
}
