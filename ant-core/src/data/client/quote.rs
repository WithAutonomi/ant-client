//! Quote and payment operations.
//!
//! Handles requesting storage quotes from network nodes and
//! managing payment for data storage.

use crate::data::client::peer_xor_distance;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{Amount, PaymentQuote};
use ant_protocol::transport::{DHTNode, MultiAddr, P2PNode, PeerId, WitnessedCloseGroup};
use ant_protocol::{
    compute_address, send_and_await_chunk_response, ChunkMessage, ChunkMessageBody,
    ChunkQuoteRequest, ChunkQuoteResponse, CLOSE_GROUP_MAJORITY, CLOSE_GROUP_SIZE,
};
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Fault-tolerant quote collection asks one extra close group of peers and
/// keeps the closest successful `CLOSE_GROUP_SIZE` responders. This remains
/// useful for merkle preflight probes, but single-node payments deliberately
/// ask only the actual close group.
const FAULT_TOLERANT_QUOTE_QUERY_MULTIPLIER: usize = 2;

/// Witnessed close-group quorum as a fraction of the initial close group.
/// For today's `CLOSE_GROUP_SIZE = 7`, this yields the requested 5-of-7
/// quorum.
const WITNESSED_QUORUM_NUMERATOR: usize = 2;
const WITNESSED_QUORUM_DENOMINATOR: usize = 3;

/// Number of closest nodes each initial witnessed responder contributes.
const SINGLE_NODE_WITNESSED_VIEW_COUNT: usize = 20;

/// Numerator for the storer-side 20% underpayment tolerance. A selected paid
/// quote can be accepted by a storer when `paid_price >= quoted_price * 4 / 5`.
const PAID_QUOTE_FLOOR_NUMERATOR: u64 = 4;

/// Denominator for the storer-side 20% underpayment tolerance.
const PAID_QUOTE_FLOOR_DENOMINATOR: u64 = 5;

/// Overall timeout for collecting quote responses. Must accommodate
/// connect_with_fallback cascade (direct 5s + hole-punch 15s×3 + relay 30s ≈
/// 80s) plus the per-peer quote timeout.
const QUOTE_COLLECTION_TIMEOUT_SECS: u64 = 120;

/// ML-DSA-65 public key length in bytes. Mirrors the same value defined as
/// `pub const ML_DSA_65_PUBLIC_KEY_SIZE` in `saorsa-pqc::pqc::types`, which
/// the storer's `peer_id_from_public_key_bytes` enforces. We keep a local
/// copy here rather than adding a direct `saorsa-pqc` dep — the constant
/// is FIPS-mandated for ML-DSA-65 and won't change unless we change variant.
///
/// TODO: switch to `saorsa_pqc::pqc::types::ML_DSA_65_PUBLIC_KEY_SIZE` once
/// `ant-protocol` re-exports it (`pqc::ops::ML_DSA_65_PUBLIC_KEY_SIZE`).
const ML_DSA_PUB_KEY_LEN: usize = 1952;

/// Check that a quote's `pub_key` is well-formed and BLAKE3-hashes to the
/// claimed `peer_id`.
///
/// The storer node enforces both constraints in `ant-node/src/payment/verifier.rs`
/// (via `peer_id_from_public_key_bytes` and `validate_peer_bindings`): every
/// quote inside a `ProofOfPayment` must (a) have a 1952-byte `pub_key` parsable
/// as ML-DSA-65 and (b) satisfy `BLAKE3(pub_key) == peer_id`. A single quote
/// failing either check causes the storer to reject the entire close-group
/// proof and burn the chunk's payment.
///
/// We mirror the cheap structural check here. The storer also runs
/// `verify_quote_content` and `verify_quote_signature`; those are ML-DSA
/// verifications (~1 ms per requested quote) and are deliberately NOT mirrored
/// on the client to keep upload latency unchanged. They are tracked as a
/// follow-up if a real attack surfaces them.
fn quote_binding_is_valid(peer_id: &PeerId, quote: &PaymentQuote) -> bool {
    if quote.pub_key.len() != ML_DSA_PUB_KEY_LEN {
        return false;
    }
    compute_address(&quote.pub_key) == *peer_id.as_bytes()
}

/// Classification of a `ChunkQuoteResponse::Success` body for a single peer.
///
/// Mirrors the storer-side `validate_peer_bindings` check from
/// `ant-node/src/payment/verifier.rs` — the cheap BLAKE3 binding —
/// so we drop misbehaving peers' quotes before payment.
///
/// We deliberately do NOT mirror the storer's `verify_quote_signature`
/// (ML-DSA-65 verify, ~1 ms × CLOSE_GROUP_SIZE × every chunk) or
/// `verify_quote_content`. Those are useful defense-in-depth for an
/// attacker who self-consistently crafts a signed-but-stolen or wrong-
/// content quote, but they are NOT cheap and are out of scope for this
/// fix. Adding them changes upload latency materially. Track them as a
/// follow-up if a real attack surfaces them.
///
/// Pulling the logic out of the async closure lets us unit-test the
/// primary defense (not just the post-collect defensive filter).
///
/// # Returns
///
/// - `Ok((quote, price))` — the response is honoured as a quote.
/// - `Err(Error::AlreadyStored)` — the peer claims the chunk is already
///   present AND the quote it provided binds to its peer ID. Vote counts.
/// - `Err(Error::BadQuoteBinding { .. })` — bad binding (mirrors the
///   storer-side rejection). Outer collector counts these via the typed
///   variant (no string matching).
/// - `Err(Error::Serialization(...))` — the quote bytes did not deserialize.
fn classify_quote_response(
    peer_id: &PeerId,
    quote_bytes: &[u8],
    already_stored: bool,
) -> std::result::Result<(PaymentQuote, Amount), Error> {
    let payment_quote = rmp_serde::from_slice::<PaymentQuote>(quote_bytes).map_err(|e| {
        Error::Serialization(format!("Failed to deserialize quote from {peer_id}: {e}"))
    })?;

    // Peer binding: BLAKE3(pub_key) must equal peer_id. This is the
    // exact mitigation Chris and the AI investigation requested for the
    // 2026-04-30 production failure: drop crossed-key peers before they
    // poison the close-group ProofOfPayment.
    if !quote_binding_is_valid(peer_id, &payment_quote) {
        let derived = compute_address(&payment_quote.pub_key);
        warn!(
            "Dropping response from {peer_id} — quote.pub_key BLAKE3 mismatch \
             (peer is signing quotes with another peer's key); the storer \
             would reject this proof"
        );
        return Err(Error::BadQuoteBinding {
            peer_id: peer_id.to_string(),
            detail: format!(
                "BLAKE3(pub_key)={} pub_key_len={}",
                hex::encode(derived),
                payment_quote.pub_key.len(),
            ),
        });
    }

    if already_stored {
        debug!("Peer {peer_id} already has chunk");
        return Err(Error::AlreadyStored);
    }
    let price = payment_quote.price;
    debug!("Received quote from {peer_id}: price = {price}");
    Ok((payment_quote, price))
}

/// Drop quotes whose `pub_key` does not BLAKE3-hash to the peer that supplied
/// them. Logs each dropped quote at WARN.
fn drop_quotes_with_bad_bindings(
    quotes: &mut Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>,
) -> usize {
    let before = quotes.len();
    quotes.retain(|(peer_id, _, quote, _)| {
        if quote_binding_is_valid(peer_id, quote) {
            true
        } else {
            warn!(
                "Dropping quote from peer {peer_id} — quote.pub_key BLAKE3 mismatch \
                 (peer is signing quotes with another peer's key); the storer would \
                 reject this proof"
            );
            false
        }
    });
    before - quotes.len()
}

#[allow(clippy::too_many_arguments)]
async fn request_store_quote_from_peer(
    node: Arc<P2PNode>,
    peer_id: PeerId,
    peer_addrs: Vec<MultiAddr>,
    request_id: u64,
    address: [u8; 32],
    data_size: u64,
    data_type: u32,
    per_peer_timeout: Duration,
) -> StoreQuoteRequestResult {
    let request = ChunkQuoteRequest {
        address,
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
            return (
                peer_id,
                peer_addrs,
                Err(Error::Protocol(format!(
                    "Failed to encode quote request for {peer_id}: {e}"
                ))),
            );
        }
    };

    let result = send_and_await_chunk_response(
        &node,
        &peer_id,
        message_bytes,
        request_id,
        per_peer_timeout,
        &peer_addrs,
        |body| match body {
            ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Success {
                quote,
                already_stored,
            }) => Some(classify_quote_response(&peer_id, &quote, already_stored)),
            ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Error(e)) => Some(Err(
                Error::Protocol(format!("Quote error from {peer_id}: {e}")),
            )),
            _ => None,
        },
        |e| Error::Network(format!("Failed to send quote request to {peer_id}: {e}")),
        || Error::Timeout(format!("Timeout waiting for quote from {peer_id}")),
    )
    .await;

    (peer_id, peer_addrs, result)
}

#[allow(clippy::too_many_arguments)]
fn record_store_quote_result(
    peer_id: PeerId,
    addrs: Vec<MultiAddr>,
    quote_result: Result<(PaymentQuote, Amount)>,
    address: &[u8; 32],
    quotes: &mut Vec<StoreQuote>,
    already_stored_peers: &mut Vec<(PeerId, [u8; 32])>,
    failures: &mut Vec<String>,
    bad_quote_count: &mut usize,
) {
    match quote_result {
        Ok((quote, price)) => {
            quotes.push((peer_id, addrs, quote, price));
        }
        Err(Error::AlreadyStored) => {
            info!("Peer {peer_id} reports chunk already stored");
            let dist = peer_xor_distance(&peer_id, address);
            already_stored_peers.push((peer_id, dist));
        }
        Err(e) => {
            if matches!(&e, Error::BadQuoteBinding { .. }) {
                *bad_quote_count += 1;
            }
            warn!("Failed to get quote from {peer_id}: {e}");
            failures.push(format!("{peer_id}: {e}"));
        }
    }
}

fn single_node_quote_query_count() -> usize {
    CLOSE_GROUP_SIZE
}

fn fault_tolerant_quote_query_count() -> usize {
    CLOSE_GROUP_SIZE * FAULT_TOLERANT_QUOTE_QUERY_MULTIPLIER
}

fn witnessed_close_group_quorum() -> usize {
    (CLOSE_GROUP_SIZE * WITNESSED_QUORUM_NUMERATOR).div_ceil(WITNESSED_QUORUM_DENOMINATOR)
}

fn paid_quote_acceptance_target() -> usize {
    witnessed_close_group_quorum()
}

fn witnessed_close_group_quorum_for_missing_views(missing_views: usize) -> usize {
    witnessed_close_group_quorum()
        .saturating_sub(missing_views)
        .max(1)
}

fn missing_witnessed_responder_views(witnessed: &WitnessedCloseGroup) -> usize {
    witnessed
        .initial_closest
        .len()
        .saturating_sub(witnessed.responder_views.len())
}

fn witnessed_close_group_quorum_for_transcript(witnessed: &WitnessedCloseGroup) -> usize {
    witnessed_close_group_quorum_for_missing_views(missing_witnessed_responder_views(witnessed))
}

fn peer_list(peers: &[PeerId]) -> Vec<String> {
    peers.iter().map(ToString::to_string).collect()
}

pub(crate) type StoreQuote = (PeerId, Vec<MultiAddr>, PaymentQuote, Amount);
type StoreQuoteRequestResult = (PeerId, Vec<MultiAddr>, Result<(PaymentQuote, Amount)>);
type VotersByPeer = HashMap<PeerId, HashSet<PeerId>>;
type WitnessedVoteData = (HashMap<PeerId, DHTNode>, VotersByPeer, Vec<(PeerId, usize)>);

pub(crate) struct StoreQuotePlan {
    pub(crate) quotes: Vec<StoreQuote>,
    pub(crate) put_peers: Vec<(PeerId, Vec<MultiAddr>)>,
}

#[derive(Debug, Clone)]
struct WitnessedQuoteCandidate {
    node: DHTNode,
    votes: usize,
}

#[derive(Debug, Clone)]
struct WitnessedQuotePeer {
    peer_id: PeerId,
    addrs: Vec<MultiAddr>,
}

#[derive(Debug, Clone)]
struct WitnessedQuoteSelection {
    quote_peers: Vec<WitnessedQuotePeer>,
    initial_put_peers: Vec<(PeerId, Vec<MultiAddr>)>,
}

#[derive(Clone, Copy)]
enum QuoteSelectionPolicy {
    ClosestByDistance,
    PaidQuoteOnly,
}

fn witnessed_initial_peers(witnessed: &WitnessedCloseGroup) -> Vec<String> {
    witnessed
        .initial_closest
        .iter()
        .map(|node| node.peer_id.to_string())
        .collect()
}

fn witnessed_responder_views(witnessed: &WitnessedCloseGroup) -> Vec<String> {
    witnessed
        .responder_views
        .iter()
        .map(|view| {
            let peers = view
                .closest
                .iter()
                .map(|node| node.peer_id)
                .collect::<Vec<_>>();
            format!("{}=>{:?}", view.responder, peer_list(&peers))
        })
        .collect()
}

fn merge_witnessed_node(nodes: &mut HashMap<PeerId, DHTNode>, node: DHTNode) {
    match nodes.entry(node.peer_id) {
        std::collections::hash_map::Entry::Occupied(mut entry) => {
            entry.get_mut().merge_from(node);
        }
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(node);
        }
    }
}

fn sort_vote_counts_by_distance(vote_counts: &mut [(PeerId, usize)], address: &[u8; 32]) {
    vote_counts.sort_by(|left, right| {
        peer_xor_distance(&left.0, address)
            .cmp(&peer_xor_distance(&right.0, address))
            .then_with(|| left.0.as_bytes().cmp(right.0.as_bytes()))
    });
}

fn witnessed_vote_counts_and_nodes(
    witnessed: &WitnessedCloseGroup,
    address: &[u8; 32],
) -> WitnessedVoteData {
    let mut known_nodes = HashMap::new();
    for node in &witnessed.initial_closest {
        merge_witnessed_node(&mut known_nodes, node.clone());
    }

    let mut voters_by_peer: HashMap<PeerId, HashSet<PeerId>> = HashMap::new();
    for view in &witnessed.responder_views {
        let mut voted = HashSet::new();
        for node in &view.closest {
            merge_witnessed_node(&mut known_nodes, node.clone());
            if voted.insert(node.peer_id) {
                voters_by_peer
                    .entry(node.peer_id)
                    .or_default()
                    .insert(view.responder);
            }
        }
    }

    let mut vote_counts: Vec<(PeerId, usize)> = voters_by_peer
        .iter()
        .map(|(peer_id, voters)| (*peer_id, voters.len()))
        .collect();
    sort_vote_counts_by_distance(&mut vote_counts, address);
    (known_nodes, voters_by_peer, vote_counts)
}

fn witnessed_consensus_candidates(
    witnessed: &WitnessedCloseGroup,
    address: &[u8; 32],
    quorum: usize,
) -> Vec<WitnessedQuoteCandidate> {
    let (known_nodes, voters_by_peer, vote_counts) =
        witnessed_vote_counts_and_nodes(witnessed, address);
    let mut candidates = vote_counts
        .iter()
        .filter_map(|(peer_id, votes)| {
            if *votes < quorum {
                return None;
            }
            known_nodes.get(peer_id).cloned().and_then(|node| {
                voters_by_peer
                    .contains_key(peer_id)
                    .then_some(WitnessedQuoteCandidate {
                        node,
                        votes: *votes,
                    })
            })
        })
        .collect::<Vec<_>>();

    candidates.sort_by(|left, right| {
        peer_xor_distance(&left.node.peer_id, address)
            .cmp(&peer_xor_distance(&right.node.peer_id, address))
            .then_with(|| right.votes.cmp(&left.votes))
            .then_with(|| {
                left.node
                    .peer_id
                    .as_bytes()
                    .cmp(right.node.peer_id.as_bytes())
            })
    });
    candidates
}

fn witnessed_vote_counts(witnessed: &WitnessedCloseGroup, address: &[u8; 32]) -> Vec<String> {
    let (_, _, vote_counts) = witnessed_vote_counts_and_nodes(witnessed, address);
    vote_counts
        .iter()
        .map(|(peer_id, votes)| format!("{peer_id}:{votes}"))
        .collect()
}

fn witnessed_consensus(
    witnessed: &WitnessedCloseGroup,
    address: &[u8; 32],
    quorum: usize,
) -> Vec<String> {
    witnessed_consensus_candidates(witnessed, address, quorum)
        .iter()
        .map(|candidate| format!("{}:{}", candidate.node.peer_id, candidate.votes))
        .collect()
}

fn witnessed_close_group_diagnostics(
    address: &[u8; 32],
    witnessed: &WitnessedCloseGroup,
    quorum: usize,
) -> String {
    format!(
        "target={}, initial={:?}, responder_views={:?}, vote_counts={:?}, quorum={}, final={:?}",
        hex::encode(address),
        witnessed_initial_peers(witnessed),
        witnessed_responder_views(witnessed),
        witnessed_vote_counts(witnessed, address),
        quorum,
        witnessed_consensus(witnessed, address, quorum)
    )
}

fn witnessed_quote_selection_or_error(
    address: &[u8; 32],
    witnessed: &WitnessedCloseGroup,
    required: usize,
    quorum: usize,
) -> Result<WitnessedQuoteSelection> {
    let candidates = witnessed_consensus_candidates(witnessed, address, quorum);
    if candidates.len() < required {
        return Err(Error::InsufficientPeers(format!(
            "Witnessed close group inconclusive before payment: got {}/{} quorum-recognised peers. {}",
            candidates.len(),
            required,
            witnessed_close_group_diagnostics(address, witnessed, quorum)
        )));
    }

    let initial_put_peers = witnessed
        .initial_closest
        .iter()
        .take(CLOSE_GROUP_SIZE)
        .map(|node| (node.peer_id, node.addresses_by_priority()))
        .collect::<Vec<_>>();

    if initial_put_peers.len() < CLOSE_GROUP_SIZE {
        return Err(Error::InsufficientPeers(format!(
            "Witnessed close group returned only {}/{} initial PUT peers before payment. {}",
            initial_put_peers.len(),
            CLOSE_GROUP_SIZE,
            witnessed_close_group_diagnostics(address, witnessed, quorum)
        )));
    }

    let quote_peers = candidates
        .into_iter()
        .take(required)
        .map(|candidate| WitnessedQuotePeer {
            peer_id: candidate.node.peer_id,
            addrs: candidate.node.addresses_by_priority(),
        })
        .collect();

    Ok(WitnessedQuoteSelection {
        quote_peers,
        initial_put_peers,
    })
}

fn sort_quotes_by_distance(quotes: &mut [StoreQuote], address: &[u8; 32]) {
    quotes.sort_by(|left, right| {
        peer_xor_distance(&left.0, address)
            .cmp(&peer_xor_distance(&right.0, address))
            .then_with(|| left.0.as_bytes().cmp(right.0.as_bytes()))
    });
}

fn paid_quote_would_pass_floor(paid_price: Amount, quoted_price: Amount) -> bool {
    let Some(paid_scaled) = paid_price.checked_mul(Amount::from(PAID_QUOTE_FLOOR_DENOMINATOR))
    else {
        return false;
    };
    let Some(quoted_floor) = quoted_price.checked_mul(Amount::from(PAID_QUOTE_FLOOR_NUMERATOR))
    else {
        return false;
    };

    paid_scaled >= quoted_floor
}

fn paid_quote_acceptance_count(paid_price: Amount, quotes: &[StoreQuote]) -> usize {
    quotes
        .iter()
        .filter(|(_, _, quote, _)| paid_quote_would_pass_floor(paid_price, quote.price))
        .count()
}

pub(crate) fn select_paid_quote_for_payment(quotes: &[StoreQuote]) -> Option<&StoreQuote> {
    let acceptance_target = paid_quote_acceptance_target();
    if quotes.len() < acceptance_target {
        return None;
    }

    let mut by_price: Vec<(usize, Amount)> = quotes
        .iter()
        .enumerate()
        .map(|(index, (_, _, quote, _))| (index, quote.price))
        .collect();
    by_price.sort_by_key(|(index, price)| (*price, *index));
    by_price
        .iter()
        .find(|(_, price)| paid_quote_acceptance_count(*price, quotes) >= acceptance_target)
        .map(|(quote_index, _)| &quotes[*quote_index])
}

fn select_closest_quotes(mut quotes: Vec<StoreQuote>, address: &[u8; 32]) -> Vec<StoreQuote> {
    sort_quotes_by_distance(&mut quotes, address);
    quotes.truncate(CLOSE_GROUP_SIZE);
    quotes
}

fn sort_paid_quote_candidates(mut quotes: Vec<StoreQuote>, address: &[u8; 32]) -> Vec<StoreQuote> {
    sort_quotes_by_distance(&mut quotes, address);
    quotes
}

impl Client {
    /// Get storage quotes from the closest peers for a given address.
    ///
    /// Builds a quorum-witnessed candidate set, requests quotes from the
    /// closest `CLOSE_GROUP_SIZE` witnessed peers concurrently, and requires
    /// enough successful responses to select one paid quote that should pass
    /// the witnessed-quorum price floors.
    ///
    /// Returns `Error::AlreadyStored` early if `CLOSE_GROUP_MAJORITY` peers
    /// report the chunk is already stored.
    ///
    /// # Errors
    ///
    /// Returns an error if insufficient quotes can be collected.
    pub async fn get_store_quotes(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>> {
        Ok(self
            .get_store_quote_plan(address, data_size, data_type)
            .await?
            .quotes)
    }

    /// Get storage quotes plus PUT targets.
    ///
    /// The quote request is still fanned out to `CLOSE_GROUP_SIZE` witnessed
    /// peers, but callers only include one selected paid quote in the proof.
    /// PUT targets remain the initial witnessed close group.
    pub(crate) async fn get_store_quote_plan(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<StoreQuotePlan> {
        let witnessed_selection = self.select_witnessed_quote_selection(address).await?;
        let remote_peers = witnessed_selection
            .quote_peers
            .into_iter()
            .map(|peer| (peer.peer_id, peer.addrs))
            .collect();
        let initial_put_peers = witnessed_selection.initial_put_peers;
        let quotes = self
            .collect_store_quotes_from_remote_peers(
                address,
                data_size,
                data_type,
                remote_peers,
                QuoteSelectionPolicy::PaidQuoteOnly,
            )
            .await?;

        Ok(StoreQuotePlan {
            quotes,
            put_peers: initial_put_peers,
        })
    }

    /// Get storage quotes with the previous over-query behaviour.
    ///
    /// Merkle preflight uses quote responses only as an already-stored probe;
    /// the actual payment still happens through merkle candidate pools. Keep
    /// the extra peer buffer there so merkle upload behaviour remains
    /// unchanged when a few peers are slow or return unusable quote bindings.
    pub(crate) async fn get_store_quotes_with_fault_tolerance(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>> {
        let peer_query_count = fault_tolerant_quote_query_count();
        let remote_peers = self
            .network()
            .find_closest_peers(address, peer_query_count)
            .await?;

        self.collect_store_quotes_from_remote_peers(
            address,
            data_size,
            data_type,
            remote_peers,
            QuoteSelectionPolicy::ClosestByDistance,
        )
        .await
    }

    async fn select_witnessed_quote_selection(
        &self,
        address: &[u8; 32],
    ) -> Result<WitnessedQuoteSelection> {
        let required = single_node_quote_query_count();
        let witnessed = self
            .network()
            .find_witnessed_close_group_with_view_count(
                address,
                required,
                SINGLE_NODE_WITNESSED_VIEW_COUNT,
            )
            .await
            .map_err(|e| {
                Error::InsufficientPeers(format!(
                    "Witnessed close group lookup failed before payment for target {}: {e}",
                    hex::encode(address)
                ))
            })?;
        let base_quorum = witnessed_close_group_quorum();
        let missing_views = missing_witnessed_responder_views(&witnessed);
        let quorum = witnessed_close_group_quorum_for_transcript(&witnessed);

        if missing_views > 0 {
            warn!(
                target = %hex::encode(address),
                initial = witnessed.initial_closest.len(),
                responder_views = witnessed.responder_views.len(),
                missing_views = missing_views,
                base_quorum = base_quorum,
                adjusted_quorum = quorum,
                "Witnessed close group transcript is missing responder views; lowering SNP witness quorum"
            );
        }

        debug!(
            target = %hex::encode(address),
            quorum = quorum,
            view_count = SINGLE_NODE_WITNESSED_VIEW_COUNT,
            initial = ?witnessed_initial_peers(&witnessed),
            responder_views = ?witnessed_responder_views(&witnessed),
            vote_counts = ?witnessed_vote_counts(&witnessed, address),
            final_witnessed_set = ?witnessed_consensus(&witnessed, address, quorum),
            "Witnessed close group selected for SNP quote collection"
        );

        witnessed_quote_selection_or_error(address, &witnessed, required, quorum)
    }

    #[allow(clippy::too_many_lines)]
    async fn collect_store_quotes_from_remote_peers(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
        remote_peers: Vec<(PeerId, Vec<MultiAddr>)>,
        quote_selection_policy: QuoteSelectionPolicy,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)>> {
        let peer_query_count = remote_peers.len();

        let node = self.network().node();

        debug!(
            "Requesting quotes from up to {peer_query_count} peers for address {} (size: {data_size})",
            hex::encode(address)
        );

        if remote_peers.len() < CLOSE_GROUP_SIZE {
            return Err(Error::InsufficientPeers(format!(
                "Found {} peers, need {CLOSE_GROUP_SIZE}",
                remote_peers.len()
            )));
        }
        debug_assert!(peer_query_count >= CLOSE_GROUP_SIZE);

        let per_peer_timeout = Duration::from_secs(self.config().quote_timeout_secs);
        let overall_timeout = Duration::from_secs(QUOTE_COLLECTION_TIMEOUT_SECS);

        // Collect quote responses. SNP/witnessed collection receives exactly
        // the seven witnessed quote peers selected above, so it does not fall
        // back to further peers when one of them fails.
        let mut quotes = Vec::with_capacity(peer_query_count);
        let mut already_stored_peers: Vec<(PeerId, [u8; 32])> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        // Track storer-rejecting peers separately (binding, content, signature
        // failures) so we can surface their count in diagnostics — they're a
        // special class of failure (peer misconfigured or hostile, not
        // network-broken) and the user benefits from seeing them called out.
        let mut bad_quote_count = 0usize;

        let mut quote_futures = FuturesUnordered::new();

        for (peer_id, peer_addrs) in &remote_peers {
            quote_futures.push(request_store_quote_from_peer(
                node.clone(),
                *peer_id,
                peer_addrs.clone(),
                self.next_request_id(),
                *address,
                data_size,
                data_type,
                per_peer_timeout,
            ));
        }

        let collect_result: std::result::Result<std::result::Result<(), Error>, _> =
            tokio::time::timeout(overall_timeout, async {
                while let Some((peer_id, addrs, quote_result)) = quote_futures.next().await {
                    record_store_quote_result(
                        peer_id,
                        addrs,
                        quote_result,
                        address,
                        &mut quotes,
                        &mut already_stored_peers,
                        &mut failures,
                        &mut bad_quote_count,
                    );
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

        // Defensive double-check: the per-peer handler already filters
        // bad-binding responses into `failures`, but if any path slipped a bad
        // quote into `quotes` (e.g. a future refactor) this catches it before
        // we sort by distance and return. `bad_dropped` should be 0 in normal
        // operation; non-zero indicates an upstream regression worth investigating.
        let bad_dropped = drop_quotes_with_bad_bindings(&mut quotes);
        if bad_dropped > 0 {
            warn!(
                "Defensive filter dropped {bad_dropped} quotes with mismatched peer bindings \
                 for address {} — the per-peer handler should have caught these earlier \
                 (this indicates an upstream regression)",
                hex::encode(address),
            );
            bad_quote_count += bad_dropped;
        }

        // Check already-stored: only count votes from the closest CLOSE_GROUP_SIZE peers.
        if !already_stored_peers.is_empty() {
            let mut all_peers_by_distance: Vec<(bool, [u8; 32])> = Vec::new();
            for (peer_id, _, _, _) in &quotes {
                all_peers_by_distance.push((false, peer_xor_distance(peer_id, address)));
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

        let already_stored_count = already_stored_peers.len();
        let failure_count = failures.len();
        let quote_count = quotes.len();
        let total_responses = quote_count + failure_count + already_stored_count;

        let required_quotes = match quote_selection_policy {
            QuoteSelectionPolicy::ClosestByDistance => CLOSE_GROUP_SIZE,
            QuoteSelectionPolicy::PaidQuoteOnly => paid_quote_acceptance_target(),
        };

        if quotes.len() >= required_quotes {
            let selected_quotes = match quote_selection_policy {
                QuoteSelectionPolicy::ClosestByDistance => select_closest_quotes(quotes, address),
                QuoteSelectionPolicy::PaidQuoteOnly => sort_paid_quote_candidates(quotes, address),
            };

            info!(
                "Collected {} usable quotes for address {} ({total_responses} responses: \
                 {quote_count} ok, {already_stored_count} already_stored, {failure_count} failed, \
                 {bad_quote_count} bad-binding)",
                selected_quotes.len(),
                hex::encode(address),
            );
            return Ok(selected_quotes);
        }

        Err(Error::InsufficientPeers(format!(
            "Got {quote_count} quotes, need {required_quotes} ({total_responses} responses: \
             {already_stored_count} already_stored, {failure_count} failed including \
             {bad_quote_count} with mismatched peer bindings). Failures: [{}]",
            failures.join("; ")
        )))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    //! Test fixtures use real ML-DSA-65 keypairs (1952-byte public keys), the
    //! same key material that ships on the wire. The "bad" quote is built by
    //! **swapping** the public key field with a different real keypair's
    //! public key — the exact shape produced by the Apr 30 production
    //! failure (an operator running two co-located identities with crossed
    //! quote-signing keys). Signatures are not exercised here because this
    //! filter only mirrors `validate_peer_bindings` (BLAKE3 binding); see
    //! the doc-comment on `quote_binding_is_valid` for why
    //! `verify_quote_signature` and `verify_quote_content` are deliberately
    //! NOT mirrored.

    use super::*;
    use ant_protocol::evm::RewardsAddress;
    use ant_protocol::pqc::ops::{MlDsaOperations, MlDsaPublicKey};
    use ant_protocol::transport::{DHTNode, MlDsa65, ResponderView, WitnessedCloseGroup};
    use std::time::SystemTime;
    use xor_name::XorName;

    /// A real ML-DSA-65 keypair plus its derived peer ID.
    struct Keypair {
        peer_id: PeerId,
        pub_key_bytes: Vec<u8>,
    }

    fn gen_keypair() -> Keypair {
        let ml_dsa = MlDsa65::new();
        let (pub_key, _sk) = ml_dsa.generate_keypair().expect("ML-DSA-65 keygen");
        let pub_key_bytes = pub_key.as_bytes().to_vec();
        let peer_id = PeerId::from_bytes(compute_address(&pub_key_bytes));
        Keypair {
            peer_id,
            pub_key_bytes,
        }
    }

    /// Build a quote tuple whose `pub_key` correctly hashes to its peer_id.
    /// Signature is left empty: this filter does not verify signatures.
    fn good_quote_real() -> (PeerId, Vec<MultiAddr>, PaymentQuote, Amount) {
        let kp = gen_keypair();
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: Amount::ZERO,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: kp.pub_key_bytes,
            signature: Vec::new(),
        };
        (kp.peer_id, Vec::new(), quote, Amount::ZERO)
    }

    /// Build a quote tuple where the quote carries a different keypair's
    /// `pub_key` than the peer_id derives from. Mirrors the production
    /// failure shape: peer A advertised on the transport, but the quote
    /// carries peer B's key.
    fn bad_quote_real() -> (PeerId, Vec<MultiAddr>, PaymentQuote, Amount) {
        let claimed = gen_keypair();
        let signing = gen_keypair();
        assert_ne!(claimed.pub_key_bytes, signing.pub_key_bytes);
        assert_ne!(claimed.peer_id.as_bytes(), signing.peer_id.as_bytes());
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: Amount::ZERO,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: signing.pub_key_bytes,
            signature: Vec::new(),
        };
        (claimed.peer_id, Vec::new(), quote, Amount::ZERO)
    }

    fn witnessed_test_node(seed: u8) -> DHTNode {
        DHTNode {
            peer_id: PeerId::from_bytes([seed; 32]),
            addresses: Vec::new(),
            address_types: Vec::new(),
            distance: None,
            reliability: 1.0,
        }
    }

    fn witnessed_test_nodes(seeds: &[u8]) -> Vec<DHTNode> {
        seeds.iter().copied().map(witnessed_test_node).collect()
    }

    fn witnessed_test_view(responder: u8, closest: &[u8]) -> ResponderView {
        ResponderView {
            responder: PeerId::from_bytes([responder; 32]),
            closest: witnessed_test_nodes(closest),
        }
    }

    fn synthetic_peer(seed: u8) -> PeerId {
        PeerId::from_bytes([seed; 32])
    }

    fn synthetic_quote(seed: u8, price: u64) -> (PeerId, Vec<MultiAddr>, PaymentQuote, Amount) {
        let amount = Amount::from(price);
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: amount,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: Vec::new(),
            signature: Vec::new(),
        };
        (synthetic_peer(seed), Vec::new(), quote, amount)
    }

    fn quote_peer_seeds(quotes: &[(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)]) -> Vec<u8> {
        quotes
            .iter()
            .map(|(peer_id, _, _, _)| peer_id.as_bytes()[0])
            .collect()
    }

    fn put_peer_seeds(peers: &[(PeerId, Vec<MultiAddr>)]) -> Vec<u8> {
        peers
            .iter()
            .map(|(peer_id, _)| peer_id.as_bytes()[0])
            .collect()
    }

    /// Independent re-implementation of the storer-side binding spec
    /// (`ant-node/src/payment/verifier.rs::validate_peer_bindings` +
    /// `peer_id_from_public_key_bytes`):
    /// (a) `pub_key` parses as ML-DSA-65 (length 1952), and
    /// (b) `BLAKE3(pub_key) == peer_id`.
    ///
    /// Re-derived from spec, NOT delegating to `quote_binding_is_valid`,
    /// so cross-checks are not "function == itself".
    fn storer_binding_would_accept(peer_id: &PeerId, quote: &PaymentQuote) -> bool {
        if MlDsaPublicKey::from_bytes(&quote.pub_key).is_err() {
            return false;
        }
        compute_address(&quote.pub_key) == *peer_id.as_bytes()
    }

    // ============================================================
    // Tests for `quote_binding_is_valid` (the predicate)
    // ============================================================

    #[test]
    fn binding_accepts_real_self_consistent_keypair() {
        let (peer_id, _, quote, _) = good_quote_real();
        // Property under test: the predicate accepts a quote whose pub_key
        // genuinely belongs to the claimed peer.
        assert!(quote_binding_is_valid(&peer_id, &quote));
        // Cross-check against the independent full storer-spec implementation.
        assert!(storer_binding_would_accept(&peer_id, &quote));
    }

    #[test]
    fn binding_rejects_real_crossed_keypair() {
        let (peer_id, _, quote, _) = bad_quote_real();
        assert!(!quote_binding_is_valid(&peer_id, &quote));
        assert!(!storer_binding_would_accept(&peer_id, &quote));
    }

    #[test]
    fn binding_rejects_oversize_pubkey() {
        // A pub_key longer than ML-DSA-65 (1952 bytes) must be rejected
        // even if BLAKE3 happens to agree, because the storer rejects on
        // length first via `peer_id_from_public_key_bytes`.
        let oversized = vec![0u8; ML_DSA_PUB_KEY_LEN + 1];
        let peer_id = PeerId::from_bytes(compute_address(&oversized));
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: Amount::ZERO,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: oversized,
            signature: Vec::new(),
        };
        // BLAKE3(pub_key) DOES equal the peer_id we constructed, so the
        // bare hash check would pass — but the length guard must reject.
        assert_eq!(compute_address(&quote.pub_key), *peer_id.as_bytes());
        assert!(
            !quote_binding_is_valid(&peer_id, &quote),
            "predicate must reject oversize pub_key even when BLAKE3 happens to match"
        );
        assert!(!storer_binding_would_accept(&peer_id, &quote));
    }

    #[test]
    fn binding_rejects_undersize_pubkey() {
        let undersized = vec![0u8; ML_DSA_PUB_KEY_LEN - 1];
        let peer_id = PeerId::from_bytes(compute_address(&undersized));
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: Amount::ZERO,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: undersized,
            signature: Vec::new(),
        };
        assert!(!quote_binding_is_valid(&peer_id, &quote));
        assert!(!storer_binding_would_accept(&peer_id, &quote));
    }

    // ============================================================
    // Tests for the filter (`drop_quotes_with_bad_bindings`)
    // ============================================================

    #[test]
    fn quote_query_counts_keep_single_node_close_group_only() {
        assert_eq!(single_node_quote_query_count(), CLOSE_GROUP_SIZE);
        assert_eq!(SINGLE_NODE_WITNESSED_VIEW_COUNT, 20);
        assert!(SINGLE_NODE_WITNESSED_VIEW_COUNT > single_node_quote_query_count());
        assert_eq!(witnessed_close_group_quorum(), 5);
        assert_eq!(witnessed_close_group_quorum_for_missing_views(0), 5);
        assert_eq!(witnessed_close_group_quorum_for_missing_views(1), 4);
        assert_eq!(witnessed_close_group_quorum_for_missing_views(2), 3);
        assert_eq!(
            fault_tolerant_quote_query_count(),
            CLOSE_GROUP_SIZE * FAULT_TOLERANT_QUOTE_QUERY_MULTIPLIER
        );
        assert!(fault_tolerant_quote_query_count() > single_node_quote_query_count());
    }

    #[test]
    fn witnessed_candidates_sort_by_xor_distance_then_votes() {
        let address = [0u8; 32];
        let witnessed = WitnessedCloseGroup {
            target: address,
            k: CLOSE_GROUP_SIZE,
            initial_closest: witnessed_test_nodes(&[1, 2, 3, 4, 5, 6, 7]),
            responder_views: vec![
                witnessed_test_view(1, &[1, 9]),
                witnessed_test_view(2, &[1, 9]),
                witnessed_test_view(3, &[1, 9]),
                witnessed_test_view(4, &[1, 9]),
                witnessed_test_view(5, &[1, 9]),
                witnessed_test_view(6, &[9]),
                witnessed_test_view(7, &[9]),
            ],
        };

        let candidates =
            witnessed_consensus_candidates(&witnessed, &address, witnessed_close_group_quorum());

        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.node.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            vec![1, 9],
            "XOR closeness must be the primary sort before quote collection"
        );
    }

    #[test]
    fn witnessed_quote_peers_error_is_typed_and_pre_payment_when_consensus_is_short() {
        let address = [0u8; 32];
        let responder_views = (1..=7)
            .map(|responder| witnessed_test_view(responder, &[1, 2, 3, 4]))
            .collect();
        let witnessed = WitnessedCloseGroup {
            target: address,
            k: CLOSE_GROUP_SIZE,
            initial_closest: witnessed_test_nodes(&[1, 2, 3, 4, 5, 6, 7]),
            responder_views,
        };

        let err = witnessed_quote_selection_or_error(
            &address,
            &witnessed,
            CLOSE_GROUP_SIZE,
            witnessed_close_group_quorum(),
        )
        .expect_err("short witnessed consensus must fail before payment");

        match err {
            Error::InsufficientPeers(message) => {
                assert!(message.contains("before payment"));
                assert!(message.contains("vote_counts"));
                assert!(message.contains("quorum"));
            }
            other => panic!("expected typed InsufficientPeers error, got {other:?}"),
        }
    }

    #[test]
    fn witnessed_quote_peers_keep_exact_quote_window() {
        let address = [0u8; 32];
        let witnessed = WitnessedCloseGroup {
            target: address,
            k: CLOSE_GROUP_SIZE,
            initial_closest: witnessed_test_nodes(&[1, 2, 3, 4, 5, 6, 7]),
            responder_views: vec![
                witnessed_test_view(1, &[1, 2, 3, 4, 5, 6, 7]),
                witnessed_test_view(2, &[1, 2, 3, 4, 5, 6, 8]),
                witnessed_test_view(3, &[1, 2, 3, 4, 5, 7, 8]),
                witnessed_test_view(4, &[1, 2, 3, 4, 6, 7, 8]),
                witnessed_test_view(5, &[1, 2, 3, 5, 6, 7, 8]),
                witnessed_test_view(6, &[1, 2, 4, 5, 6, 7, 8]),
                witnessed_test_view(7, &[1, 3, 4, 5, 6, 7, 8]),
            ],
        };

        let selection = witnessed_quote_selection_or_error(
            &address,
            &witnessed,
            CLOSE_GROUP_SIZE,
            witnessed_close_group_quorum(),
        )
        .expect("seven quote candidates should be selected");

        assert_eq!(selection.quote_peers.len(), CLOSE_GROUP_SIZE);
        assert_eq!(
            selection
                .quote_peers
                .iter()
                .map(|peer| peer.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
        assert_eq!(
            put_peer_seeds(&selection.initial_put_peers),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
    }

    #[test]
    fn witnessed_quote_peers_lower_quorum_for_missing_responder_views() {
        let address = [0u8; 32];
        let witnessed = WitnessedCloseGroup {
            target: address,
            k: CLOSE_GROUP_SIZE,
            initial_closest: witnessed_test_nodes(&[1, 2, 3, 4, 5, 6, 7]),
            responder_views: vec![
                witnessed_test_view(1, &[1, 2, 3, 4, 5, 6, 7]),
                witnessed_test_view(2, &[1, 2, 3, 4, 5, 6, 8]),
                witnessed_test_view(3, &[1, 2, 3, 4, 5, 7, 8]),
                witnessed_test_view(4, &[1, 2, 3, 4, 6, 7, 8]),
                witnessed_test_view(5, &[1, 2, 3, 5, 6, 7, 8]),
                witnessed_test_view(6, &[1, 2, 4, 5, 6, 7, 8]),
            ],
        };
        let quorum = witnessed_close_group_quorum_for_transcript(&witnessed);

        assert_eq!(missing_witnessed_responder_views(&witnessed), 1);
        assert_eq!(quorum, 4);

        let selection =
            witnessed_quote_selection_or_error(&address, &witnessed, CLOSE_GROUP_SIZE, quorum)
                .expect(
                    "one missing responder view should lower quorum and still select candidates",
                );

        assert_eq!(
            selection
                .quote_peers
                .iter()
                .map(|peer| peer.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7]
        );
    }

    #[test]
    fn paid_quote_selection_uses_lowest_quote_that_clears_witnessed_floor_target() {
        let quotes = vec![
            synthetic_quote(1, 595),
            synthetic_quote(2, 690),
            synthetic_quote(3, 762),
            synthetic_quote(4, 1046),
            synthetic_quote(5, 1048),
        ];
        let (peer_id, _, _, price) =
            select_paid_quote_for_payment(&quotes).expect("quotes should have a paid quote");
        assert_eq!(*peer_id, synthetic_peer(4));
        assert_eq!(*price, Amount::from(1046u64));
    }

    #[test]
    fn paid_quote_selection_ignores_two_high_outliers_when_five_lower_quotes_clear() {
        let quotes = vec![
            synthetic_quote(1, 100),
            synthetic_quote(2, 110),
            synthetic_quote(3, 120),
            synthetic_quote(4, 130),
            synthetic_quote(5, 140),
            synthetic_quote(6, 1000),
            synthetic_quote(7, 1100),
        ];
        let (peer_id, _, _, price) =
            select_paid_quote_for_payment(&quotes).expect("quotes should have a paid quote");
        assert_eq!(*peer_id, synthetic_peer(3));
        assert_eq!(*price, Amount::from(120u64));
    }

    #[test]
    fn paid_quote_selection_requires_witnessed_quorum_of_successful_quotes() {
        let quotes = vec![synthetic_quote(4, 40)];
        assert!(
            select_paid_quote_for_payment(&quotes).is_none(),
            "payment selection should fail before spending without quorum-priced quote data"
        );
    }

    #[test]
    fn paid_quote_candidates_sort_by_distance_before_tie_breaking() {
        let address = [0u8; 32];
        let sorted = sort_paid_quote_candidates(
            vec![
                synthetic_quote(3, 10),
                synthetic_quote(1, 10),
                synthetic_quote(2, 10),
                synthetic_quote(5, 10),
                synthetic_quote(4, 10),
            ],
            &address,
        );

        assert_eq!(quote_peer_seeds(&sorted), vec![1, 2, 3, 4, 5]);
        let (peer_id, _, _, _) =
            select_paid_quote_for_payment(&sorted).expect("sorted quotes should have a paid quote");
        assert_eq!(
            *peer_id,
            synthetic_peer(1),
            "equal-price selection should use the first distance-sorted candidate"
        );
    }

    #[test]
    fn filter_drops_only_bad_bindings_and_leaves_storer_acceptable_quotes() {
        let mut quotes = vec![
            good_quote_real(),
            bad_quote_real(),
            good_quote_real(),
            bad_quote_real(),
            good_quote_real(),
        ];

        let dropped = drop_quotes_with_bad_bindings(&mut quotes);

        assert_eq!(dropped, 2, "two crossed-key quotes must be dropped");
        assert_eq!(quotes.len(), 3, "three real-key quotes must remain");

        // Cross-checked invariant: every retained quote would be accepted by
        // a storer running the full spec. The defensive filter only checks
        // the binding, so this asserts the binding-only filter is correct
        // for binding-only failures (other failure modes are filtered by
        // the per-peer classifier upstream).
        for (peer_id, _, quote, _) in &quotes {
            assert!(
                storer_binding_would_accept(peer_id, quote),
                "every retained quote must satisfy the full storer-side spec"
            );
        }
    }

    #[test]
    fn filter_is_noop_when_all_quotes_are_storer_acceptable() {
        let mut quotes: Vec<_> = (0..5).map(|_| good_quote_real()).collect();
        let before = quotes.len();
        let dropped = drop_quotes_with_bad_bindings(&mut quotes);
        assert_eq!(dropped, 0);
        assert_eq!(quotes.len(), before);
        for (peer_id, _, quote, _) in &quotes {
            assert!(storer_binding_would_accept(peer_id, quote));
        }
    }

    #[test]
    fn filter_drops_all_when_every_responder_is_bad() {
        // The "all hostile" case: every peer returned a bad binding. The
        // patch should leave us with zero quotes (not panic, not skip the
        // filter, not return malformed quotes). The caller then surfaces
        // InsufficientPeers.
        let mut quotes: Vec<_> = (0..fault_tolerant_quote_query_count())
            .map(|_| bad_quote_real())
            .collect();
        let dropped = drop_quotes_with_bad_bindings(&mut quotes);
        assert_eq!(dropped, fault_tolerant_quote_query_count());
        assert!(quotes.is_empty());
    }

    #[test]
    fn filter_preserves_quote_payload_byte_for_byte() {
        // After filtering, the retained quotes must be untouched — pub_key,
        // signature, content, timestamp, price, rewards_address. The patch
        // is a filter, not a transformation; this test catches any future
        // regression that mutates a retained quote.
        let (peer_id, addrs, original_quote, amount) = good_quote_real();
        let mut quotes = vec![(peer_id, addrs.clone(), original_quote.clone(), amount)];
        let _ = drop_quotes_with_bad_bindings(&mut quotes);

        let (kept_peer, kept_addrs, kept_quote, kept_amount) =
            quotes.pop().expect("the good quote must survive filtering");
        assert_eq!(kept_peer.as_bytes(), peer_id.as_bytes());
        assert_eq!(kept_addrs.len(), addrs.len());
        assert_eq!(kept_amount, amount);
        assert_eq!(kept_quote.pub_key, original_quote.pub_key);
        assert_eq!(kept_quote.signature, original_quote.signature);
        assert_eq!(kept_quote.content.0, original_quote.content.0);
        assert_eq!(kept_quote.timestamp, original_quote.timestamp);
        assert_eq!(kept_quote.price, original_quote.price);
        assert_eq!(kept_quote.rewards_address, original_quote.rewards_address);
    }

    // ============================================================
    // The Apr 30 production-failure repro
    // ============================================================

    /// Repro of the production failure from 2026-04-30 testnet runs.
    ///
    /// An external operator on `75.48.86.24` ran two co-located ant-node
    /// identities (peer `0755ecb55b…` and peer `073db92f…`) that crossed
    /// their quote-signing keys. Every chunk whose XOR-closest set happened
    /// to include peer `0755ecb5` got a payment proof with one malformed
    /// quote, and the storer's `validate_peer_bindings` rejected the
    /// entire close-group proof — burning the chunk's payment.
    ///
    /// This test proves the fault-tolerant quote path still fixes that failure
    /// shape:
    ///
    /// 1. We assemble `2x CLOSE_GROUP_SIZE` real ML-DSA-65 quotes — the same
    ///    buffer merkle preflight and merkle-mode estimates retain for probes.
    /// 2. One of them is a *crossed-key* quote — the production failure shape.
    /// 3. We run an independent `storer_would_accept` check (re-derived from
    ///    the storer spec, not from `quote_binding_is_valid`) over the
    ///    pre-filter set; we confirm the bad peer is rejected, proving the
    ///    storer **would** burn the chunk's payment if we proceeded unfiltered.
    /// 4. We run `drop_quotes_with_bad_bindings`.
    /// 5. We re-run `storer_would_accept` over the post-filter set; we confirm
    ///    EVERY remaining quote would be accepted, proving the filtered set
    ///    will not trigger the `validate_peer_bindings` rejection that caused
    ///    the Apr 30 outage.
    /// 6. We confirm the post-filter set has at least `CLOSE_GROUP_SIZE`
    ///    quotes — the over-query buffer (2x) is sufficient.
    #[test]
    fn repro_apr_30_storer_would_have_rejected_pre_filter_and_accepts_post_filter() {
        let over_query_count = fault_tolerant_quote_query_count();
        let mut quotes: Vec<_> = (0..over_query_count - 1)
            .map(|_| good_quote_real())
            .collect();
        // Splice the crossed-key quote in the middle (mirrors the random
        // position the bad peer takes in the DHT-returned closest set).
        quotes.insert(over_query_count / 2, bad_quote_real());
        assert_eq!(quotes.len(), over_query_count);

        // Step 1: prove the storer would reject the pre-filter set.
        let storer_would_reject_count = quotes
            .iter()
            .filter(|(p, _, q, _)| !storer_binding_would_accept(p, q))
            .count();
        assert_eq!(
            storer_would_reject_count, 1,
            "exactly one quote (the crossed-key one) must be rejected by the storer spec"
        );

        // Step 2: run the patched filter.
        let dropped = drop_quotes_with_bad_bindings(&mut quotes);
        assert_eq!(dropped, 1, "exactly the crossed-key quote must be filtered");

        // Step 3: prove the storer would accept every survivor under the FULL spec.
        for (peer_id, _, quote, _) in &quotes {
            assert!(
                storer_binding_would_accept(peer_id, quote),
                "every post-filter quote must be accepted by the storer spec — \
                 this is what the filter guarantees before any quote set is used"
            );
        }

        // Step 4: prove the over-query buffer is sufficient to refill.
        assert!(
            quotes.len() >= CLOSE_GROUP_SIZE,
            "after filtering, at least CLOSE_GROUP_SIZE good quotes must remain \
             so a fault-tolerant probe can still return a full close group"
        );
    }

    /// When more than the over-query buffer of peers misbehave, the filter
    /// must NOT silently produce a short proof. The downstream caller in
    /// `get_store_quotes` must see fewer than `CLOSE_GROUP_SIZE` survivors
    /// and return `InsufficientPeers`.
    #[test]
    fn filter_leaves_short_set_when_too_many_bad_peers() {
        let good_count = CLOSE_GROUP_SIZE - 1;
        let bad_count = fault_tolerant_quote_query_count() - good_count;
        let mut quotes: Vec<_> = std::iter::repeat_with(bad_quote_real)
            .take(bad_count)
            .chain(std::iter::repeat_with(good_quote_real).take(good_count))
            .collect();

        let dropped = drop_quotes_with_bad_bindings(&mut quotes);
        assert_eq!(dropped, bad_count);
        assert!(
            quotes.len() < CLOSE_GROUP_SIZE,
            "this is the precondition for InsufficientPeers downstream"
        );
        // Sanity: every survivor is storer-acceptable under the full spec.
        for (peer_id, _, quote, _) in &quotes {
            assert!(storer_binding_would_accept(peer_id, quote));
        }
    }

    // ============================================================
    // Tests for the per-peer response classifier (the PRIMARY defense).
    //
    // These tests exercise the production code path that runs inside
    // get_store_quotes' per-peer async closure. The defensive
    // `drop_quotes_with_bad_bindings` is a second line of defence —
    // these tests make sure the FIRST line is what actually catches
    // misbehaving peers in production. Without these, a regression
    // that removes the per-peer check could be masked by the post-
    // collect filter and pass the rest of the suite.
    // ============================================================

    /// Helper: serialize a `PaymentQuote` to bytes the way the wire layer
    /// does (rmp_serde / msgpack), to feed into `classify_quote_response`.
    fn serialize_quote(quote: &PaymentQuote) -> Vec<u8> {
        rmp_serde::to_vec(quote).expect("serialize quote")
    }

    #[test]
    fn classifier_accepts_real_self_consistent_quote() {
        let (peer_id, _, quote, _) = good_quote_real();
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &bytes, false);
        match result {
            Ok((q, price)) => {
                assert_eq!(q.pub_key, quote.pub_key);
                assert_eq!(price, quote.price);
            }
            Err(e) => panic!("expected Ok, got {e}"),
        }
    }

    #[test]
    fn classifier_rejects_crossed_keypair_with_typed_error() {
        let (peer_id, _, quote, _) = bad_quote_real();
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &bytes, false);
        match result {
            Err(Error::BadQuoteBinding {
                peer_id: pid,
                detail,
            }) => {
                assert_eq!(pid, peer_id.to_string());
                assert!(
                    detail.contains("BLAKE3(pub_key)="),
                    "diagnostic detail must include the derived peer id: {detail}"
                );
            }
            other => panic!("expected BadQuoteBinding for crossed-key quote, got {other:?}"),
        }
    }

    /// CRITICAL: a misbehaving peer that votes `already_stored=true` must
    /// NOT be allowed to influence the close-group "already stored"
    /// majority decision. The bind-check runs before the AlreadyStored
    /// short-circuit, so a crossed-key peer voting "already stored" is
    /// classified as `BadQuoteBinding`, not `AlreadyStored`.
    ///
    /// This locks in a specific reviewer concern from round 1:
    ///   "A peer with a crossed/garbage signing key could simply respond
    ///   already_stored=true and its vote enters already_stored_peers
    ///   unfiltered."
    #[test]
    fn classifier_rejects_already_stored_vote_from_bad_binding_peer() {
        let (peer_id, _, quote, _) = bad_quote_real();
        let bytes = serialize_quote(&quote);
        // The peer claims already_stored=true, but its quote has a crossed key.
        let result = classify_quote_response(&peer_id, &bytes, true);
        assert!(
            matches!(result, Err(Error::BadQuoteBinding { .. })),
            "crossed-key peer must be classified BadQuoteBinding even when \
             voting already_stored=true; got {result:?}"
        );
    }

    /// An honest peer's `already_stored=true` vote IS honoured (after
    /// passing the bind-check). This is the contrast to the test above.
    #[test]
    fn classifier_honours_already_stored_vote_from_good_binding_peer() {
        let (peer_id, _, quote, _) = good_quote_real();
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &bytes, true);
        assert!(
            matches!(result, Err(Error::AlreadyStored)),
            "honest peer's already_stored vote must be honoured; got {result:?}"
        );
    }

    #[test]
    fn classifier_returns_serialization_error_on_bad_bytes() {
        let (peer_id, _, _, _) = good_quote_real();
        let garbage = b"this is not a valid msgpack PaymentQuote".to_vec();
        let result = classify_quote_response(&peer_id, &garbage, false);
        assert!(
            matches!(result, Err(Error::Serialization(_))),
            "garbage bytes must produce a Serialization error; got {result:?}"
        );
    }

    /// Cross-validate the classifier's binding verdict against the
    /// independent storer-spec re-derivation across mixed responders.
    #[test]
    fn classifier_verdict_matches_storer_binding_spec_for_mixed_responders() {
        let mut responders: Vec<(PeerId, PaymentQuote)> = (0..12)
            .map(|_| {
                let (p, _, q, _) = good_quote_real();
                (p, q)
            })
            .collect();
        for _ in 0..4 {
            let (p, _, q, _) = bad_quote_real();
            responders.push((p, q));
        }

        for (peer_id, quote) in &responders {
            let bytes = serialize_quote(quote);
            let storer_verdict = storer_binding_would_accept(peer_id, quote);
            let classifier_verdict = classify_quote_response(peer_id, &bytes, false).is_ok();
            assert_eq!(
                classifier_verdict, storer_verdict,
                "classifier and storer-binding-spec must agree on every responder \
                 (peer_id={}, storer={storer_verdict}, classifier={classifier_verdict})",
                peer_id
            );
        }
    }
}
