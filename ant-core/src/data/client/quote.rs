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
use ant_node::payment::calculate_price;
use ant_node::{CLOSE_GROUP_MAJORITY, CLOSE_GROUP_SIZE};
use futures::stream::{FuturesUnordered, StreamExt};
use std::time::Duration;
use tracing::{debug, info, warn};

impl Client {
    /// Get storage quotes from the closest peers for a given address.
    ///
    /// Queries the DHT for the `CLOSE_GROUP_SIZE` closest peers and requests
    /// quotes from each. Waits for all requests to complete or timeout.
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

        debug!(
            "Requesting {CLOSE_GROUP_SIZE} quotes for address {} (size: {data_size})",
            hex::encode(address)
        );

        // Query DHT and take only the CLOSE_GROUP_SIZE closest peers.
        let mut remote_peers = self.close_group_peers(address).await?;
        remote_peers.truncate(CLOSE_GROUP_SIZE);

        if remote_peers.len() < CLOSE_GROUP_SIZE {
            return Err(Error::InsufficientPeers(format!(
                "Found {} peers, need {CLOSE_GROUP_SIZE}",
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

        // Wait for all quote requests to complete or timeout.
        // Early-return if CLOSE_GROUP_MAJORITY peers report already stored.
        let mut quotes = Vec::with_capacity(CLOSE_GROUP_SIZE);
        let mut already_stored_count = 0usize;
        let mut failures: Vec<String> = Vec::new();

        while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
            match quote_result {
                Ok((quote, price)) => {
                    quotes.push((peer_id, addrs, quote, price));
                }
                Err(Error::AlreadyStored) => {
                    already_stored_count += 1;
                    debug!("Peer {peer_id} reports chunk already stored");
                    if already_stored_count >= CLOSE_GROUP_MAJORITY {
                        info!(
                            "Chunk {} already stored ({already_stored_count}/{CLOSE_GROUP_SIZE} peers confirm)",
                            hex::encode(address)
                        );
                        return Err(Error::AlreadyStored);
                    }
                }
                Err(e) => {
                    warn!("Failed to get quote from {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));
                }
            }
        }

        if quotes.len() >= CLOSE_GROUP_SIZE {
            info!(
                "Collected {} quotes for address {}",
                quotes.len(),
                hex::encode(address)
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
