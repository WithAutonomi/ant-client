//! Quote and payment operations.
//!
//! Handles requesting storage quotes from network nodes and
//! managing payment for data storage.

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
use std::collections::HashSet;
use std::time::Duration;
use tracing::{debug, info, warn};

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

        let per_peer_timeout = Duration::from_secs(self.config().timeout_secs);
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
                            close_group,
                        }) => {
                            if already_stored {
                                debug!("Peer {peer_id_clone} already has chunk");
                                return Some(Err(Error::AlreadyStored));
                            }
                            match rmp_serde::from_slice::<PaymentQuote>(&quote) {
                                Ok(payment_quote) => {
                                    let price = payment_quote.price;
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

        // Collect all responses with an overall timeout to prevent indefinite stalls.
        // Over-query means we have 2x peers, so we can tolerate failures.
        let mut quotes = Vec::with_capacity(over_query_count);
        let mut close_group_views: Vec<(PeerId, Vec<[u8; 32]>)> =
            Vec::with_capacity(over_query_count);
        let mut already_stored_peers: Vec<(PeerId, [u8; 32])> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        let collect_result: std::result::Result<std::result::Result<(), Error>, _> =
            tokio::time::timeout(overall_timeout, async {
                while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
                    match quote_result {
                        Ok((quote, price, close_group)) => {
                            close_group_views.push((peer_id, close_group));
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
            all_peers_by_distance.sort_by(|a, b| a.1.cmp(&b.1));

            let close_group_stored = all_peers_by_distance
                .iter()
                .take(CLOSE_GROUP_SIZE)
                .filter(|(is_stored, _)| *is_stored)
                .count();

            if close_group_stored >= CLOSE_GROUP_MAJORITY {
                info!(
                    "Chunk {} already stored ({close_group_stored}/{CLOSE_GROUP_SIZE} close-group peers confirm)",
                    hex::encode(address)
                );
                return Err(Error::AlreadyStored);
            }
        }

        if quotes.len() < CLOSE_GROUP_SIZE {
            return Err(Error::InsufficientPeers(format!(
                "Got {} quotes, need {CLOSE_GROUP_SIZE}. Failures: [{}]",
                quotes.len(),
                failures.join("; ")
            )));
        }

        // Sort by XOR distance to target, keep the closest CLOSE_GROUP_SIZE.
        // We over-queried, so trim to the true close group before validation.
        quotes.sort_by(|a, b| {
            let dist_a = xor_distance(&a.0, address);
            let dist_b = xor_distance(&b.0, address);
            dist_a.cmp(&dist_b)
        });
        quotes.truncate(CLOSE_GROUP_SIZE);

        // Also trim close_group_views to match the kept quotes.
        let kept_peers: HashSet<[u8; 32]> = quotes
            .iter()
            .map(|(pid, _, _, _)| *pid.as_bytes())
            .collect();
        close_group_views.retain(|(pid, _)| kept_peers.contains(pid.as_bytes()));

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use ant_evm::{Amount, PaymentQuote, QuotingMetrics, RewardsAddress};
    use ant_node::core::{MultiAddr, PeerId};
    use ant_node::CLOSE_GROUP_SIZE;
    use std::collections::HashSet;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::time::SystemTime;

    /// Create a deterministic peer ID from an index byte.
    fn peer(id: u8) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[0] = id;
        bytes
    }

    /// Build a views array where each peer's view contains the given neighbors.
    fn make_views(entries: &[([u8; 32], &[[u8; 32]])]) -> Vec<([u8; 32], HashSet<[u8; 32]>)> {
        entries
            .iter()
            .map(|(id, neighbors)| (*id, neighbors.iter().copied().collect()))
            .collect()
    }

    // ─── next_combination ──────────────────────────────────────────────

    #[test]
    fn next_combination_enumerates_all_c_5_3() {
        let n = 5;
        let k = 3;
        let mut indices: Vec<usize> = (0..k).collect();
        let mut count = 1; // first combination is [0,1,2]
        while Client::next_combination(&mut indices, n) {
            count += 1;
        }
        // C(5,3) = 10
        assert_eq!(count, 10);
    }

    #[test]
    fn next_combination_enumerates_all_c_5_5() {
        let n = 5;
        let k = 5;
        let mut indices: Vec<usize> = (0..k).collect();
        let mut count = 1;
        while Client::next_combination(&mut indices, n) {
            count += 1;
        }
        // C(5,5) = 1
        assert_eq!(count, 1);
    }

    #[test]
    fn next_combination_single_element() {
        let n = 5;
        let k = 1;
        let mut indices: Vec<usize> = (0..k).collect();
        let mut count = 1;
        while Client::next_combination(&mut indices, n) {
            count += 1;
        }
        // C(5,1) = 5
        assert_eq!(count, 5);
    }

    #[test]
    fn next_combination_empty_returns_false() {
        let mut indices: Vec<usize> = vec![];
        assert!(!Client::next_combination(&mut indices, 5));
    }

    // ─── is_mutual_subset ──────────────────────────────────────────────

    #[test]
    fn is_mutual_subset_two_peers_recognize_each_other() {
        let a = peer(1);
        let b = peer(2);
        let peer_ids = vec![a, b];
        let views = make_views(&[(a, &[b]), (b, &[a])]);

        assert!(Client::is_mutual_subset(&peer_ids, &views, &[0, 1]));
    }

    #[test]
    fn is_mutual_subset_asymmetric_fails() {
        let a = peer(1);
        let b = peer(2);
        let peer_ids = vec![a, b];
        // A sees B, but B does NOT see A
        let views = make_views(&[(a, &[b]), (b, &[])]);

        assert!(!Client::is_mutual_subset(&peer_ids, &views, &[0, 1]));
    }

    #[test]
    fn is_mutual_subset_missing_view_returns_false() {
        let a = peer(1);
        let b = peer(2);
        let peer_ids = vec![a, b];
        // Only A has a view entry; B is missing entirely
        let views = make_views(&[(a, &[b])]);

        assert!(!Client::is_mutual_subset(&peer_ids, &views, &[0, 1]));
    }

    // ─── find_largest_mutual_subset ────────────────────────────────────

    #[test]
    fn all_five_peers_mutually_recognize() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        let views = make_views(&[
            (peers[0], &[peers[1], peers[2], peers[3], peers[4]]),
            (peers[1], &[peers[0], peers[2], peers[3], peers[4]]),
            (peers[2], &[peers[0], peers[1], peers[3], peers[4]]),
            (peers[3], &[peers[0], peers[1], peers[2], peers[4]]),
            (peers[4], &[peers[0], peers[1], peers[2], peers[3]]),
        ]);

        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 5);
    }

    #[test]
    fn three_of_five_mutually_recognize() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        // Peers 0,1,2 see each other; peers 3,4 have empty views
        let views = make_views(&[
            (peers[0], &[peers[1], peers[2]]),
            (peers[1], &[peers[0], peers[2]]),
            (peers[2], &[peers[0], peers[1]]),
            (peers[3], &[]),
            (peers[4], &[]),
        ]);

        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 3);
    }

    #[test]
    fn two_of_five_below_majority() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        // Only peers 0 and 1 see each other
        let views = make_views(&[
            (peers[0], &[peers[1]]),
            (peers[1], &[peers[0]]),
            (peers[2], &[]),
            (peers[3], &[]),
            (peers[4], &[]),
        ]);

        // Largest mutual subset is 2, below CLOSE_GROUP_MAJORITY (3)
        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 0);
    }

    #[test]
    fn empty_views_returns_zero() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        let views = make_views(&[
            (peers[0], &[]),
            (peers[1], &[]),
            (peers[2], &[]),
            (peers[3], &[]),
            (peers[4], &[]),
        ]);

        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 0);
    }

    #[test]
    fn four_of_five_one_rogue_peer() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        // Peer 4 doesn't recognize anyone, but the other 4 form a clique
        let views = make_views(&[
            (peers[0], &[peers[1], peers[2], peers[3]]),
            (peers[1], &[peers[0], peers[2], peers[3]]),
            (peers[2], &[peers[0], peers[1], peers[3]]),
            (peers[3], &[peers[0], peers[1], peers[2]]),
            (peers[4], &[]),
        ]);

        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 4);
    }

    #[test]
    fn asymmetric_recognition_reduces_clique() {
        let peers: Vec<[u8; 32]> = (1..=5).map(peer).collect();
        // All see each other except: peer 3 does NOT see peer 0
        let views = make_views(&[
            (peers[0], &[peers[1], peers[2], peers[3], peers[4]]),
            (peers[1], &[peers[0], peers[2], peers[3], peers[4]]),
            (peers[2], &[peers[0], peers[1], peers[3], peers[4]]),
            (peers[3], &[peers[1], peers[2], peers[4]]), // missing peers[0]
            (peers[4], &[peers[0], peers[1], peers[2], peers[3]]),
        ]);

        // {0,1,2,3,4} fails (3 doesn't see 0), but {1,2,3,4} works
        assert_eq!(Client::find_largest_mutual_subset(&peers, &views), 4);
    }

    // ─── validate_close_group_quorum (integration) ─────────────────────

    fn make_test_quote() -> PaymentQuote {
        PaymentQuote {
            content: xor_name::XorName([0u8; 32]),
            timestamp: SystemTime::now(),
            quoting_metrics: QuotingMetrics {
                data_size: 0,
                data_type: 0,
                close_records_stored: 0,
                records_per_type: vec![],
                max_records: 0,
                received_payment_count: 0,
                live_time: 0,
                network_density: None,
                network_size: None,
            },
            pub_key: vec![],
            signature: vec![],
            rewards_address: RewardsAddress::new([0u8; 20]),
        }
    }

    fn make_dummy_addr() -> Vec<MultiAddr> {
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 12345);
        vec![MultiAddr::quic(addr)]
    }

    type Quotes = Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>;
    type CloseGroupViews = Vec<(PeerId, Vec<[u8; 32]>)>;

    fn make_quotes_and_views(
        peer_ids: &[PeerId],
        neighbor_map: &[Vec<[u8; 32]>],
    ) -> (Quotes, CloseGroupViews) {
        let quotes: Quotes = peer_ids
            .iter()
            .map(|pid| (*pid, make_dummy_addr(), make_test_quote(), Amount::ZERO))
            .collect();

        let views: CloseGroupViews = peer_ids
            .iter()
            .zip(neighbor_map.iter())
            .map(|(pid, neighbors)| (*pid, neighbors.clone()))
            .collect();

        (quotes, views)
    }

    #[test]
    fn validate_quorum_all_mutual_passes() {
        let peer_ids: Vec<PeerId> = (1..=CLOSE_GROUP_SIZE)
            .map(|i| PeerId::from_bytes(peer(i as u8)))
            .collect();

        let neighbor_map: Vec<Vec<[u8; 32]>> = peer_ids
            .iter()
            .map(|me| {
                peer_ids
                    .iter()
                    .filter(|p| p != &me)
                    .map(|p| *p.as_bytes())
                    .collect()
            })
            .collect();

        let (quotes, views) = make_quotes_and_views(&peer_ids, &neighbor_map);
        assert!(Client::validate_close_group_quorum(&quotes, &views).is_ok());
    }

    #[test]
    fn validate_quorum_empty_views_fails() {
        let peer_ids: Vec<PeerId> = (1..=CLOSE_GROUP_SIZE)
            .map(|i| PeerId::from_bytes(peer(i as u8)))
            .collect();

        let neighbor_map: Vec<Vec<[u8; 32]>> = vec![vec![]; CLOSE_GROUP_SIZE];

        let (quotes, views) = make_quotes_and_views(&peer_ids, &neighbor_map);
        let result = Client::validate_close_group_quorum(&quotes, &views);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::CloseGroupQuorumFailure(_)
        ));
    }

    #[test]
    fn validate_quorum_exactly_majority_passes() {
        let peer_ids: Vec<PeerId> = (1..=CLOSE_GROUP_SIZE)
            .map(|i| PeerId::from_bytes(peer(i as u8)))
            .collect();

        // Only first CLOSE_GROUP_MAJORITY peers see each other
        let majority = &peer_ids[..CLOSE_GROUP_MAJORITY];
        let neighbor_map: Vec<Vec<[u8; 32]>> = peer_ids
            .iter()
            .map(|me| {
                if majority.contains(me) {
                    majority
                        .iter()
                        .filter(|p| p != &me)
                        .map(|p| *p.as_bytes())
                        .collect()
                } else {
                    vec![]
                }
            })
            .collect();

        let (quotes, views) = make_quotes_and_views(&peer_ids, &neighbor_map);
        assert!(Client::validate_close_group_quorum(&quotes, &views).is_ok());
    }
}
