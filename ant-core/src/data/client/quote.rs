//! Quote and payment operations.
//!
//! Handles requesting storage quotes from network nodes and
//! managing payment for data storage.

use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_evm::{Amount, PaymentQuote};
use ant_node::ant_protocol::{
    ChunkMessage, ChunkMessageBody, ChunkQuoteRequest, ChunkQuoteResponse,
};
use ant_node::client::send_and_await_chunk_response;
use ant_node::core::{MultiAddr, PeerId};
use ant_node::payment::{calculate_price, single_node::REQUIRED_QUOTES};
use futures::stream::{FuturesUnordered, StreamExt};
use std::time::Duration;
use tracing::{debug, info, warn};

impl Client {
    /// Get storage quotes from the closest peers for a given address.
    ///
    /// Queries the DHT for the closest peers and requests quotes from them.
    /// Collects exactly `REQUIRED_QUOTES` (5) quotes.
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

        debug!(
            "Requesting {REQUIRED_QUOTES} quotes for address {} (size: {data_size})",
            hex::encode(address)
        );

        // Use the same DHT-based peer discovery as chunk_get
        let remote_peers = self.close_group_peers(address).await?;

        if remote_peers.len() < REQUIRED_QUOTES {
            return Err(Error::InsufficientPeers(format!(
                "Found {} peers, need {REQUIRED_QUOTES}",
                remote_peers.len()
            )));
        }

        let timeout = Duration::from_secs(self.config().timeout_secs);

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
                let result = send_and_await_chunk_response(
                    &node_clone,
                    &peer_id_clone,
                    message_bytes,
                    request_id,
                    timeout,
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
                                    let price = calculate_price(&payment_quote.quoting_metrics);
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

                (peer_id_clone, addrs_clone, result)
            };

            quote_futures.push(quote_future);
        }

        // Collect quotes as they complete
        let mut quotes_with_peers = Vec::with_capacity(REQUIRED_QUOTES);
        let mut already_stored_count = 0usize;
        let mut failures: Vec<String> = Vec::new();

        while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
            match quote_result {
                Ok((quote, price)) => {
                    quotes_with_peers.push((peer_id, addrs, quote, price));
                    if quotes_with_peers.len() >= REQUIRED_QUOTES {
                        break;
                    }
                }
                Err(Error::AlreadyStored) => {
                    already_stored_count += 1;
                    debug!("Peer {peer_id} reports chunk already stored");
                }
                Err(e) => {
                    warn!("Failed to get quote from {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));
                }
            }
        }

        // If we collected enough quotes, proceed with payment regardless
        // of how many peers reported already_stored.
        if quotes_with_peers.len() >= REQUIRED_QUOTES {
            info!(
                "Collected {} quotes for address {}",
                quotes_with_peers.len(),
                hex::encode(address)
            );
            return Ok(quotes_with_peers);
        }

        // Not enough quotes. If any peer reported already_stored, the chunk
        // likely exists — signal the caller to skip payment.
        if already_stored_count > 0 {
            return Err(Error::AlreadyStored);
        }

        Err(Error::InsufficientPeers(format!(
            "Got {} quotes, need {REQUIRED_QUOTES}. Failures: [{}]",
            quotes_with_peers.len(),
            failures.join("; ")
        )))
    }
}
