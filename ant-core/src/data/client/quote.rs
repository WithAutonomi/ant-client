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
use std::collections::HashSet;
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
                            close_group,
                        }) => {
                            if already_stored {
                                debug!("Peer {peer_id_clone} already has chunk");
                                return Some(Err(Error::AlreadyStored));
                            }
                            match rmp_serde::from_slice::<PaymentQuote>(&quote) {
                                Ok(payment_quote) => {
                                    let price = calculate_price(&payment_quote.quoting_metrics);
                                    debug!("Received quote from {peer_id_clone}: price = {price}");
                                    Some(Ok((payment_quote, price, close_group)))
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
        let mut close_group_views: Vec<(PeerId, Vec<[u8; 32]>)> =
            Vec::with_capacity(CLOSE_GROUP_SIZE);
        let mut already_stored_count = 0usize;
        let mut failures: Vec<String> = Vec::new();

        while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
            match quote_result {
                Ok((quote, price, close_group)) => {
                    close_group_views.push((peer_id, close_group));
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

        if quotes.len() < CLOSE_GROUP_SIZE {
            return Err(Error::InsufficientPeers(format!(
                "Got {} quotes, need {CLOSE_GROUP_SIZE}. Failures: [{}]",
                quotes.len(),
                failures.join("; ")
            )));
        }

        // Validate close-group quorum: each responding peer should recognize
        // most of the other queried peers in its own close-group view.
        Self::validate_close_group_quorum(&quotes, &close_group_views)?;

        info!(
            "Collected {} quotes for address {} (close-group quorum verified)",
            quotes.len(),
            hex::encode(address)
        );
        Ok(quotes)
    }

    /// Validate close-group quorum by finding the largest subset of queried
    /// peers that mutually recognize each other.
    ///
    /// "Mutual recognition" means: for every pair (P, Q) in the subset,
    /// Q appears in P's close-group view. This matches the server-side
    /// `CLOSE_GROUP_MAJORITY` threshold that nodes enforce during payment
    /// verification.
    ///
    /// Fails if no mutually-recognizing subset of size `CLOSE_GROUP_MAJORITY`
    /// exists. Larger subsets are better — they increase the likelihood of
    /// durable storage and replication.
    fn validate_close_group_quorum(
        quotes: &[(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)],
        close_group_views: &[(PeerId, Vec<[u8; 32]>)],
    ) -> Result<()> {
        let peer_ids: Vec<[u8; 32]> = quotes
            .iter()
            .map(|(peer_id, _, _, _)| *peer_id.as_bytes())
            .collect();

        // Build a lookup: peer_bytes → set of peers it recognizes
        let views: Vec<([u8; 32], HashSet<[u8; 32]>)> = close_group_views
            .iter()
            .map(|(peer_id, view)| (*peer_id.as_bytes(), view.iter().copied().collect()))
            .collect();

        // Check subsets from largest to smallest (CLOSE_GROUP_SIZE down to
        // CLOSE_GROUP_MAJORITY). For CLOSE_GROUP_SIZE=5 this is at most
        // C(5,5) + C(5,4) + C(5,3) = 1 + 5 + 10 = 16 checks.
        let clique_size = Self::find_largest_mutual_subset(&peer_ids, &views);

        if clique_size >= CLOSE_GROUP_MAJORITY {
            info!(
                "Close-group quorum passed: {clique_size}/{} peers mutually recognize each other",
                peer_ids.len()
            );
            Ok(())
        } else {
            Err(Error::CloseGroupQuorumFailure(format!(
                "Largest mutually-recognizing subset is {clique_size} peers (need {CLOSE_GROUP_MAJORITY})"
            )))
        }
    }

    /// Find the size of the largest subset of `peer_ids` where every peer
    /// in the subset appears in every other peer's close-group view.
    fn find_largest_mutual_subset(
        peer_ids: &[[u8; 32]],
        views: &[([u8; 32], HashSet<[u8; 32]>)],
    ) -> usize {
        let n = peer_ids.len();

        // Try subset sizes from largest to smallest.
        for size in (CLOSE_GROUP_MAJORITY..=n).rev() {
            // Iterate all index combinations of the given size.
            let mut indices: Vec<usize> = (0..size).collect();
            loop {
                if Self::is_mutual_subset(peer_ids, views, &indices) {
                    return size;
                }
                if !Self::next_combination(&mut indices, n) {
                    break;
                }
            }
        }

        0
    }

    /// Check whether the peers at the given indices mutually recognize each other.
    fn is_mutual_subset(
        peer_ids: &[[u8; 32]],
        views: &[([u8; 32], HashSet<[u8; 32]>)],
        indices: &[usize],
    ) -> bool {
        for &i in indices {
            // Find this peer's view
            let peer_bytes = peer_ids[i];
            let view = views
                .iter()
                .find(|(id, _)| *id == peer_bytes)
                .map(|(_, v)| v);

            let Some(view) = view else {
                return false;
            };

            // Every OTHER peer in the subset must appear in this peer's view
            for &j in indices {
                if i == j {
                    continue;
                }
                if !view.contains(&peer_ids[j]) {
                    return false;
                }
            }
        }
        true
    }

    /// Advance an index combination to the next one in lexicographic order.
    /// Returns false when all combinations have been exhausted.
    fn next_combination(indices: &mut [usize], n: usize) -> bool {
        let k = indices.len();
        // Find the rightmost index that can be incremented
        let mut i = k;
        while i > 0 {
            i -= 1;
            if indices[i] < n - k + i {
                indices[i] += 1;
                // Reset all subsequent indices
                for j in (i + 1)..k {
                    indices[j] = indices[j - 1] + 1;
                }
                return true;
            }
        }
        false
    }
}
