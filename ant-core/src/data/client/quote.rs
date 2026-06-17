//! Quote and payment operations.
//!
//! Handles requesting storage quotes from network nodes and
//! managing payment for data storage.

use crate::data::client::peer_xor_distance;
use crate::data::client::Client;
use crate::data::client::PUT_TARGET_WIDTH;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{Amount, PaymentQuote};
use ant_protocol::payment::calculate_price;
use ant_protocol::payment::commitment::{
    commitment_hash, verify_commitment_signature, StorageCommitment, MAX_COMMITMENT_KEY_COUNT,
    MAX_COMMITMENT_SIDECAR_BYTES,
};
use ant_protocol::payment::{verify_quote_content, verify_quote_signature};
use ant_protocol::transport::{
    DHTNode, MultiAddr, P2PNode, PeerId, ResponderView, WitnessedCloseGroup,
};
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

/// Index of the paid median quote after sorting by quoted price.
const MEDIAN_QUOTE_INDEX: usize = CLOSE_GROUP_SIZE / 2;

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

/// One collected quote: the responding peer, its addresses, the signed quote,
/// the price it demands, and (ADR-0003) the opaque signed-commitment blob the
/// node shipped alongside the quote (`None` for a baseline quote), to be
/// forwarded as a sidecar in the PUT bundle.
type QuotedPeer = (
    PeerId,
    Vec<MultiAddr>,
    PaymentQuote,
    Amount,
    Option<Vec<u8>>,
);

/// Per-peer classification outcome: `(quote, price, commitment)` on success.
type QuoteResult = std::result::Result<(PaymentQuote, Amount, Option<Vec<u8>>), Error>;

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
/// This is the cheap structural pre-check. ADR-0003 additionally has the client
/// run `verify_quote_content` + `verify_quote_signature` (the full ML-DSA check)
/// in [`classify_quote_response`] before paying, so a quote the storer would
/// reject never gets paid.
fn quote_binding_is_valid(peer_id: &PeerId, quote: &PaymentQuote) -> bool {
    if quote.pub_key.len() != ML_DSA_PUB_KEY_LEN {
        return false;
    }
    compute_address(&quote.pub_key) == *peer_id.as_bytes()
}

/// ADR-0003 client-side resolve-before-pay gate — "the client pays nothing it
/// cannot resolve", the ceiling's load-bearing wall (ADR §"The client pays
/// nothing it cannot resolve").
///
/// Runs the **full** binding check before paying, identical to the storer's,
/// using the shared `ant-protocol` commitment type + verifier so client and
/// node can never disagree:
/// 1. **Shape.** `(0, None)` baseline or `(n>0, Some(pin))` bound; the mixed
///    shapes `(n>0, None)` (unauditable count) and `(0, Some)` (incoherent
///    baseline) are rejected.
/// 2. **Cap.** `committed_key_count <= MAX_COMMITMENT_KEY_COUNT` — a count a
///    commitment could never legitimately attest is rejected before pricing.
/// 3. **Forced price.** `price == calculate_price(committed_key_count)`, by
///    exact recomputation with the shared `calculate_price` — never inverted.
/// 4. **Resolution (bound quotes).** The shipped commitment must: parse as a
///    `StorageCommitment`, be bound to the quoting peer
///    (`BLAKE3(sender_public_key) == sender_peer_id`), have a valid ML-DSA-65
///    signature, hash to the quote's `commitment_pin`
///    (`commitment_hash == pin`), and attest exactly the claimed count
///    (`key_count == committed_key_count`). A withheld, unparseable, wrong-pin,
///    mis-bound, or count-mismatched commitment is unresolvable → the quote is
///    dropped before payment.
///
/// Returns `Ok(())` if the binding fully resolves, or `Err(detail)` naming the
/// rule that failed.
fn quote_commitment_binding_is_valid(
    peer_id: &PeerId,
    quote: &PaymentQuote,
    commitment: &Option<Vec<u8>>,
) -> std::result::Result<(), String> {
    let count = quote.committed_key_count;
    let pin = quote.commitment_pin;
    match (count, pin.is_some()) {
        (0, false) | (1.., true) => {}
        (1.., false) => {
            return Err(format!(
                "committed_key_count={count} > 0 but commitment_pin is None (unauditable count)"
            ));
        }
        (0, true) => {
            return Err("committed_key_count=0 with a commitment_pin (incoherent baseline)".into());
        }
    }
    if count > MAX_COMMITMENT_KEY_COUNT {
        return Err(format!(
            "committed_key_count={count} exceeds MAX_COMMITMENT_KEY_COUNT={MAX_COMMITMENT_KEY_COUNT}"
        ));
    }
    // Forced price: exact recomputation, never inversion.
    let expected = calculate_price(count as usize);
    if quote.price != expected {
        return Err(format!(
            "price {} does not equal calculate_price(committed_key_count={count}) = {expected}",
            quote.price
        ));
    }

    // Baseline `(0, None)` pins nothing — fully resolved by the checks above.
    let Some(pin) = pin else {
        return Ok(());
    };

    // Bound quote: the commitment MUST have arrived and MUST resolve the pin.
    let Some(blob) = commitment else {
        return Err(
            "bound quote did not ship its commitment; the pin is unresolvable so the quote \
             is dropped before payment"
                .into(),
        );
    };
    // Cap before parsing: bound the deserialize work a malicious responder can
    // force, and never forward an oversized blob in the PUT bundle.
    if blob.len() > MAX_COMMITMENT_SIDECAR_BYTES {
        return Err(format!(
            "shipped commitment is {} bytes, exceeds MAX_COMMITMENT_SIDECAR_BYTES={MAX_COMMITMENT_SIDECAR_BYTES}",
            blob.len()
        ));
    }
    let commitment: StorageCommitment = rmp_serde::from_slice(blob).map_err(|e| {
        format!("shipped commitment did not deserialize as a StorageCommitment: {e}")
    })?;

    // Peer binding: the commitment must belong to the quoting peer, exactly as
    // the storer derives a candidate's peer id (`BLAKE3(pub_key)`).
    if compute_address(&commitment.sender_public_key) != *peer_id.as_bytes()
        || commitment.sender_peer_id != *peer_id.as_bytes()
    {
        return Err("shipped commitment is not bound to the quoting peer".into());
    }
    if !verify_commitment_signature(&commitment) {
        return Err("shipped commitment has an invalid signature".into());
    }
    if commitment_hash(&commitment) != Some(pin) {
        return Err("shipped commitment does not hash to the quote's pin".into());
    }
    if commitment.key_count != count {
        return Err(format!(
            "shipped commitment attests key_count={} but the quote claims {count}",
            commitment.key_count
        ));
    }
    Ok(())
}

/// Classification of a `ChunkQuoteResponse::Success` body for a single peer.
///
/// Mirrors the storer-side `validate_peer_bindings` check from
/// `ant-node/src/payment/verifier.rs` — the cheap BLAKE3 binding —
/// so we drop misbehaving peers' quotes before payment.
///
/// ADR-0003: the client now ALSO runs the storer's `verify_quote_content` and
/// `verify_quote_signature` (ML-DSA-65) before paying, so "the client pays
/// nothing it cannot resolve" covers the quote's own validity too, not just the
/// commitment binding. This matches what the merkle path already does
/// client-side and costs ~1 ms × CLOSE_GROUP_SIZE per chunk — accepted, since
/// paying a quote the storer then rejects burns the on-chain payment.
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
/// - `Err(Error::BadQuoteCommitment { .. })` — ADR-0003 forced-price binding
///   failed (price off the curve, incoherent shape, or a bound quote that did
///   not ship its commitment); dropped before payment like a bad binding.
/// - `Err(Error::Serialization(...))` — the quote bytes did not deserialize.
///
/// On success the returned commitment is the opaque signed-commitment blob the
/// node shipped with the quote (`None` for a baseline quote), to be forwarded
/// as a sidecar in the PUT bundle.
fn classify_quote_response(
    peer_id: &PeerId,
    expected_content: &[u8; 32],
    quote_bytes: &[u8],
    already_stored: bool,
    commitment: Option<Vec<u8>>,
) -> std::result::Result<(PaymentQuote, Amount, Option<Vec<u8>>), Error> {
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

    // ADR-0003 "the client runs the full binding check": verify the quote's OWN
    // ML-DSA-65 signature and that it is for THIS content, before paying —
    // exactly what the storer checks and what the merkle path already does
    // client-side. A quote with a valid pub_key binding but a bad signature or
    // wrong content would otherwise be paid and then rejected by the storer.
    if !verify_quote_content(&payment_quote, expected_content) {
        return Err(Error::BadQuoteBinding {
            peer_id: peer_id.to_string(),
            detail: "quote content does not match the requested address".to_string(),
        });
    }
    if !verify_quote_signature(&payment_quote) {
        return Err(Error::BadQuoteBinding {
            peer_id: peer_id.to_string(),
            detail: "quote ML-DSA-65 signature is invalid".to_string(),
        });
    }

    // ADR-0003 forced-price gate: drop a quote whose price is not exactly the
    // public formula of its committed count, whose (count, pin) shape is
    // incoherent, or which is bound but did not ship its commitment. The storer
    // re-runs the arithmetic and would reject the bundle; we drop it here so we
    // never pay a quote we cannot resolve.
    if let Err(detail) = quote_commitment_binding_is_valid(peer_id, &payment_quote, &commitment) {
        warn!("Dropping response from {peer_id} — ADR-0003 binding invalid: {detail}");
        return Err(Error::BadQuoteCommitment {
            peer_id: peer_id.to_string(),
            detail,
        });
    }

    if already_stored {
        debug!("Peer {peer_id} already has chunk");
        return Err(Error::AlreadyStored);
    }
    let price = payment_quote.price;
    debug!("Received quote from {peer_id}: price = {price}");
    Ok((payment_quote, price, commitment))
}

/// Drop quotes whose `pub_key` does not BLAKE3-hash to the peer that supplied
/// them. Logs each dropped quote at WARN.
fn drop_quotes_with_bad_bindings(quotes: &mut Vec<QuotedPeer>) -> usize {
    let before = quotes.len();
    quotes.retain(|(peer_id, _, quote, _, _)| {
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

fn witnessed_quote_launch_budget(
    successful_quotes: usize,
    in_flight: usize,
    remaining_peers: usize,
) -> usize {
    CLOSE_GROUP_SIZE
        .saturating_sub(successful_quotes.saturating_add(in_flight))
        .min(remaining_peers)
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

/// Restrict a witnessed transcript to its closest `CLOSE_GROUP_SIZE` peers.
///
/// The witnessed query is widened to `PUT_TARGET_WIDTH` peers so we
/// have addresses for the full PUT-target set, but the consensus/quorum/quote
/// logic must still run on the close group only. Keeping just the closest-7
/// initial peers and the responder views contributed by those peers leaves the
/// `missing_witnessed_responder_views` math — and the quorum derived from it —
/// byte-for-byte identical to a `CLOSE_GROUP_SIZE`-wide query.
fn scope_witnessed_to_close_group(witnessed: &WitnessedCloseGroup) -> WitnessedCloseGroup {
    let initial_closest: Vec<DHTNode> = witnessed
        .initial_closest
        .iter()
        .take(CLOSE_GROUP_SIZE)
        .cloned()
        .collect();
    let scope: HashSet<PeerId> = initial_closest.iter().map(|node| node.peer_id).collect();
    let responder_views: Vec<ResponderView> = witnessed
        .responder_views
        .iter()
        .filter(|view| scope.contains(&view.responder))
        .cloned()
        .collect();
    WitnessedCloseGroup {
        target: witnessed.target,
        k: CLOSE_GROUP_SIZE,
        initial_closest,
        responder_views,
    }
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
    voters: HashSet<PeerId>,
}

#[derive(Debug, Clone)]
struct WitnessedQuotePeer {
    peer_id: PeerId,
    addrs: Vec<MultiAddr>,
    voters: HashSet<PeerId>,
}

#[derive(Debug, Clone)]
struct WitnessedQuoteSelection {
    quote_peers: Vec<WitnessedQuotePeer>,
    initial_put_peers: Vec<(PeerId, Vec<MultiAddr>)>,
    quorum: usize,
}

enum QuoteSelectionPolicy {
    ClosestByDistance,
    WitnessedMedianVoters {
        voters_by_peer: VotersByPeer,
        quorum: usize,
    },
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
                    .get(peer_id)
                    .cloned()
                    .map(|voters| WitnessedQuoteCandidate {
                        node,
                        votes: *votes,
                        voters,
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
        .map(|candidate| WitnessedQuotePeer {
            peer_id: candidate.node.peer_id,
            addrs: candidate.node.addresses_by_priority(),
            voters: candidate.voters,
        })
        .collect();

    Ok(WitnessedQuoteSelection {
        quote_peers,
        initial_put_peers,
        quorum,
    })
}

pub(crate) fn median_paid_quote_issuer(
    quotes: &[(PeerId, Vec<MultiAddr>, PaymentQuote, Amount)],
) -> Option<(PeerId, Amount)> {
    if quotes.len() <= MEDIAN_QUOTE_INDEX {
        return None;
    }

    let mut by_price: Vec<(usize, PeerId, Amount)> = quotes
        .iter()
        .enumerate()
        .map(|(index, (peer_id, _, _, price))| (index, *peer_id, *price))
        .collect();
    by_price.sort_by_key(|(index, _, price)| (*price, *index));
    by_price
        .get(MEDIAN_QUOTE_INDEX)
        .map(|(_, peer_id, price)| (*peer_id, *price))
}

fn sort_quotes_by_distance(quotes: &mut [StoreQuote], address: &[u8; 32]) {
    quotes.sort_by(|left, right| {
        peer_xor_distance(&left.0, address)
            .cmp(&peer_xor_distance(&right.0, address))
            .then_with(|| left.0.as_bytes().cmp(right.0.as_bytes()))
    });
}

fn median_paid_quote_issuer_for_indices(
    quotes: &[StoreQuote],
    indices: &[usize],
) -> Option<(PeerId, Amount)> {
    if indices.len() <= MEDIAN_QUOTE_INDEX {
        return None;
    }

    let mut by_price: Vec<(usize, PeerId, Amount)> = indices
        .iter()
        .enumerate()
        .map(|(selected_index, quote_index)| {
            let (peer_id, _, _, price) = &quotes[*quote_index];
            (selected_index, *peer_id, *price)
        })
        .collect();
    by_price.sort_by_key(|(selected_index, _, price)| (*price, *selected_index));
    by_price
        .get(MEDIAN_QUOTE_INDEX)
        .map(|(_, peer_id, price)| (*peer_id, *price))
}

fn median_issuer_voter_support(
    quotes: &[StoreQuote],
    indices: &[usize],
    voters_by_peer: &VotersByPeer,
) -> Option<(PeerId, usize)> {
    let (median_peer_id, _) = median_paid_quote_issuer_for_indices(quotes, indices)?;
    let voters = voters_by_peer.get(&median_peer_id)?;
    Some((median_peer_id, voters.len()))
}

fn visit_quote_subsets<F>(
    quote_count: usize,
    subset_size: usize,
    start_index: usize,
    current: &mut Vec<usize>,
    visit: &mut F,
) where
    F: FnMut(&[usize]),
{
    if current.len() == subset_size {
        visit(current);
        return;
    }

    let remaining = subset_size - current.len();
    let last_start = quote_count - remaining;
    for index in start_index..=last_start {
        current.push(index);
        visit_quote_subsets(quote_count, subset_size, index + 1, current, visit);
        current.pop();
    }
}

fn select_closest_quotes(mut quotes: Vec<StoreQuote>, address: &[u8; 32]) -> Vec<StoreQuote> {
    sort_quotes_by_distance(&mut quotes, address);
    quotes.truncate(CLOSE_GROUP_SIZE);
    quotes
}

fn select_witnessed_median_voter_quotes(
    mut quotes: Vec<StoreQuote>,
    address: &[u8; 32],
    voters_by_peer: &VotersByPeer,
    required_support: usize,
) -> Option<Vec<StoreQuote>> {
    if quotes.len() < CLOSE_GROUP_SIZE {
        return None;
    }

    sort_quotes_by_distance(&mut quotes, address);

    let mut best_indices: Option<(usize, Vec<usize>)> = None;
    let mut current_indices = Vec::with_capacity(CLOSE_GROUP_SIZE);
    visit_quote_subsets(
        quotes.len(),
        CLOSE_GROUP_SIZE,
        0,
        &mut current_indices,
        &mut |indices| {
            let Some((_, support)) = median_issuer_voter_support(&quotes, indices, voters_by_peer)
            else {
                return;
            };
            if support < required_support {
                return;
            }
            match &best_indices {
                Some((best_support, best)) if *best_support > support => {}
                Some((best_support, best))
                    if *best_support == support && best.as_slice() <= indices => {}
                _ => best_indices = Some((support, indices.to_vec())),
            }
        },
    );

    best_indices.map(|(_, indices)| {
        indices
            .into_iter()
            .map(|index| quotes[index].clone())
            .collect()
    })
}

fn put_peers_with_median_voters_first(
    quotes: &[StoreQuote],
    put_peers: &[(PeerId, Vec<MultiAddr>)],
    voters_by_peer: &VotersByPeer,
    required_support: usize,
) -> Option<Vec<(PeerId, Vec<MultiAddr>)>> {
    let (median_peer_id, _) = median_paid_quote_issuer(quotes)?;
    let voters = voters_by_peer.get(&median_peer_id)?;

    let mut supporting_peers = Vec::new();
    let mut fallback_peers = Vec::new();
    for (peer_id, addrs) in put_peers {
        let peer = (*peer_id, addrs.clone());
        if voters.contains(peer_id) {
            supporting_peers.push(peer);
        } else {
            fallback_peers.push(peer);
        }
    }

    if supporting_peers.len() < required_support {
        return None;
    }

    supporting_peers.extend(fallback_peers);
    Some(supporting_peers)
}

impl Client {
    /// Get storage quotes from the closest peers for a given address.
    ///
    /// Builds a quorum-witnessed candidate set with at least
    /// `CLOSE_GROUP_SIZE` peers, requests quotes from all of them concurrently,
    /// and returns the closest supported `CLOSE_GROUP_SIZE` successful
    /// responders. When multiple sets are possible, the client prefers the
    /// one with the strongest paid-median voter support, then the closest
    /// peers by XOR distance.
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

    /// Get storage quotes plus PUT targets ordered for paid-median acceptance.
    ///
    /// Quote order is preserved for proof construction because tied quote
    /// prices rely on stable median selection. PUT target order is separate:
    /// peers that voted for the paid median issuer are placed first so the
    /// initial write wave is locally acceptable to a storage majority.
    pub(crate) async fn get_store_quote_plan(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<StoreQuotePlan> {
        let witnessed_selection = self.select_witnessed_quote_selection(address).await?;
        let voters_by_peer: VotersByPeer = witnessed_selection
            .quote_peers
            .iter()
            .map(|peer| (peer.peer_id, peer.voters.clone()))
            .collect();
        let remote_peers = witnessed_selection
            .quote_peers
            .into_iter()
            .map(|peer| (peer.peer_id, peer.addrs))
            .collect();
        let initial_put_peers = witnessed_selection.initial_put_peers;
        let quorum = witnessed_selection.quorum;
        let quotes = self
            .collect_store_quotes_from_remote_peers(
                address,
                data_size,
                data_type,
                remote_peers,
                QuoteSelectionPolicy::WitnessedMedianVoters {
                    voters_by_peer: voters_by_peer.clone(),
                    quorum,
                },
            )
            .await?;
        let put_peers = put_peers_with_median_voters_first(
            &quotes,
            &initial_put_peers,
            &voters_by_peer,
            quorum,
        )
        .ok_or_else(|| {
            Error::InsufficientPeers(format!(
                "Collected {} witnessed quotes, but fewer than {} initial witness PUT peers \
                 voted for the paid median issuer for {}",
                quotes.len(),
                quorum,
                hex::encode(address)
            ))
        })?;

        Ok(StoreQuotePlan { quotes, put_peers })
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
        // The quote/quorum/consensus scope is the closest CLOSE_GROUP_SIZE.
        let required = single_node_quote_query_count();
        // Contact the closest PUT_TARGET_WIDTH peers directly so the whole
        // PUT-target set's addresses arrive in this single query. A network
        // with fewer than that near the target can't satisfy the wide lookup,
        // so fall back to the close-group width — the upload still proceeds with
        // a narrower (but valid) PUT-target set rather than failing.
        let witnessed = match self
            .network()
            .find_witnessed_close_group_with_view_count(
                address,
                PUT_TARGET_WIDTH,
                SINGLE_NODE_WITNESSED_VIEW_COUNT,
            )
            .await
        {
            Ok(witnessed) => witnessed,
            Err(wide_err) => {
                debug!(
                    target = %hex::encode(address),
                    "Wide witnessed lookup ({PUT_TARGET_WIDTH}) failed ({wide_err}); \
                     retrying at close-group width ({required})"
                );
                self.network()
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
                    })?
            }
        };
        // Run quoting/quorum on the closest CLOSE_GROUP_SIZE only, so payment
        // semantics are unaffected by the wider PUT query.
        let witnessed_quote = scope_witnessed_to_close_group(&witnessed);
        let base_quorum = witnessed_close_group_quorum();
        let missing_views = missing_witnessed_responder_views(&witnessed_quote);
        let quorum = witnessed_close_group_quorum_for_transcript(&witnessed_quote);

        if missing_views > 0 {
            warn!(
                target = %hex::encode(address),
                initial = witnessed_quote.initial_closest.len(),
                responder_views = witnessed_quote.responder_views.len(),
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
            initial = ?witnessed_initial_peers(&witnessed_quote),
            responder_views = ?witnessed_responder_views(&witnessed_quote),
            vote_counts = ?witnessed_vote_counts(&witnessed_quote, address),
            final_witnessed_set = ?witnessed_consensus(&witnessed_quote, address, quorum),
            "Witnessed close group selected for SNP quote collection"
        );

        let mut selection =
            witnessed_quote_selection_or_error(address, &witnessed_quote, required, quorum)?;
        // Widen the PUT-target set to the closest PUT_TARGET_WIDTH
        // directly-contacted peers; the quote set above stays the closest
        // CLOSE_GROUP_SIZE. The same proof is reused on all of them.
        selection.initial_put_peers = witnessed
            .initial_closest
            .iter()
            .take(PUT_TARGET_WIDTH)
            .map(|node| (node.peer_id, node.addresses_by_priority()))
            .collect();
        Ok(selection)
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

        // Collect quote responses. SNP/witnessed collection deliberately tries
        // the closest witnessed peers first and only falls back to further
        // witnessed peers when a closer peer fails to produce a usable quote.
        let mut quotes = Vec::with_capacity(peer_query_count);
        let mut already_stored_peers: Vec<(PeerId, [u8; 32])> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        // Track storer-rejecting peers separately (binding, content, signature
        // failures) so we can surface their count in diagnostics — they're a
        // special class of failure (peer misconfigured or hostile, not
        // network-broken) and the user benefits from seeing them called out.
        let mut bad_quote_count = 0usize;

        let staged_witnessed_collection = matches!(
            &quote_selection_policy,
            QuoteSelectionPolicy::WitnessedMedianVoters { .. }
        );

        if staged_witnessed_collection {
            let mut quote_futures = FuturesUnordered::new();
            let mut next_peer_index = 0usize;
            let collect_result: std::result::Result<std::result::Result<(), Error>, _> =
                tokio::time::timeout(overall_timeout, async {
                    loop {
                        let launch_count = witnessed_quote_launch_budget(
                            quotes.len(),
                            quote_futures.len(),
                            remote_peers.len().saturating_sub(next_peer_index),
                        );
                        for _ in 0..launch_count {
                            let (peer_id, peer_addrs) = &remote_peers[next_peer_index];
                            next_peer_index += 1;
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

                        if quotes.len() >= CLOSE_GROUP_SIZE || quote_futures.is_empty() {
                            break;
                        }

                        let Some((peer_id, addrs, quote_result)) = quote_futures.next().await
                        else {
                            break;
                        };
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
                }
                Ok(Err(e)) => return Err(e),
                Ok(Ok(())) => {}
            }
        } else {
            // Merkle preflight keeps the previous behaviour: query the full
            // over-query set concurrently because those quote responses are
            // only used as an already-stored probe.
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
            for (peer_id, _, _, _, _) in &quotes {
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

        if quotes.len() >= CLOSE_GROUP_SIZE {
            let selected_quotes = match quote_selection_policy {
                QuoteSelectionPolicy::ClosestByDistance => select_closest_quotes(quotes, address),
                QuoteSelectionPolicy::WitnessedMedianVoters {
                    voters_by_peer,
                    quorum,
                } => select_witnessed_median_voter_quotes(quotes, address, &voters_by_peer, quorum)
                    .ok_or_else(|| {
                        Error::InsufficientPeers(format!(
                            "Got {quote_count} quotes, need {CLOSE_GROUP_SIZE} whose paid \
                                 median issuer is recognised by at least {} \
                                 selected witness peers ({total_responses} responses: \
                                 {already_stored_count} already_stored, {failure_count} failed \
                                 including {bad_quote_count} with mismatched peer bindings). \
                                 Failures: [{}]",
                            quorum,
                            failures.join("; ")
                        ))
                    })?,
            };

            info!(
                "Collected {} quotes for address {} ({total_responses} responses: \
                 {quote_count} ok, {already_stored_count} already_stored, {failure_count} failed, \
                 {bad_quote_count} bad-binding)",
                selected_quotes.len(),
                hex::encode(address),
            );
            return Ok(selected_quotes);
        }

        Err(Error::InsufficientPeers(format!(
            "Got {quote_count} quotes, need {CLOSE_GROUP_SIZE} ({total_responses} responses: \
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
        secret_key_bytes: Vec<u8>,
    }

    fn gen_keypair() -> Keypair {
        let ml_dsa = MlDsa65::new();
        let (pub_key, sk) = ml_dsa.generate_keypair().expect("ML-DSA-65 keygen");
        let pub_key_bytes = pub_key.as_bytes().to_vec();
        let peer_id = PeerId::from_bytes(compute_address(&pub_key_bytes));
        Keypair {
            peer_id,
            pub_key_bytes,
            secret_key_bytes: sk.as_bytes().to_vec(),
        }
    }

    /// Build a PROPERLY-SIGNED baseline quote for `content`, signed by a real
    /// ML-DSA-65 key whose `BLAKE3(pub_key)` is the returned peer id. Passes the
    /// client's full classifier gate (binding + content + signature + price).
    fn signed_baseline_quote(content: [u8; 32]) -> (PeerId, PaymentQuote) {
        use ant_protocol::pqc::ops::MlDsaSecretKey;
        let kp = gen_keypair();
        let mut quote = PaymentQuote {
            content: XorName(content),
            timestamp: SystemTime::UNIX_EPOCH,
            price: calculate_price(0),
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: kp.pub_key_bytes.clone(),
            signature: Vec::new(),
            committed_key_count: 0,
            commitment_pin: None,
        };
        let ml_dsa = MlDsa65::new();
        let sk = MlDsaSecretKey::from_bytes(&kp.secret_key_bytes).expect("sk");
        let msg = quote.bytes_for_sig();
        quote.signature = ml_dsa.sign(&sk, &msg).expect("sign").as_bytes().to_vec();
        (kp.peer_id, quote)
    }

    /// Build a quote tuple whose `pub_key` correctly hashes to its peer_id.
    /// Signature is left empty: this filter does not verify signatures.
    ///
    /// The quote is a valid ADR-0003 **baseline**: `(0, None)` priced at
    /// `calculate_price(0)`, so it passes the forced-price gate in
    /// `classify_quote_response`. The 5th tuple element is the (absent)
    /// commitment sidecar.
    fn good_quote_real() -> QuotedPeer {
        let kp = gen_keypair();
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: calculate_price(0),
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: kp.pub_key_bytes,
            signature: Vec::new(),
            committed_key_count: 0,
            commitment_pin: None,
        };
        (kp.peer_id, Vec::new(), quote, calculate_price(0), None)
    }

    /// Build a quote tuple where the quote carries a different keypair's
    /// `pub_key` than the peer_id derives from. Mirrors the production
    /// failure shape: peer A advertised on the transport, but the quote
    /// carries peer B's key.
    fn bad_quote_real() -> QuotedPeer {
        let claimed = gen_keypair();
        let signing = gen_keypair();
        assert_ne!(claimed.pub_key_bytes, signing.pub_key_bytes);
        assert_ne!(claimed.peer_id.as_bytes(), signing.peer_id.as_bytes());
        let quote = PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price: calculate_price(0),
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: signing.pub_key_bytes,
            signature: Vec::new(),
            committed_key_count: 0,
            commitment_pin: None,
        };
        (claimed.peer_id, Vec::new(), quote, calculate_price(0), None)
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

    fn synthetic_voters(seeds: &[u8]) -> HashSet<PeerId> {
        seeds.iter().copied().map(synthetic_peer).collect()
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

    fn put_peers_from_seeds(seeds: &[u8]) -> Vec<(PeerId, Vec<MultiAddr>)> {
        seeds
            .iter()
            .copied()
            .map(|seed| (synthetic_peer(seed), Vec::new()))
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
        let (peer_id, _, quote, _, _) = good_quote_real();
        // Property under test: the predicate accepts a quote whose pub_key
        // genuinely belongs to the claimed peer.
        assert!(quote_binding_is_valid(&peer_id, &quote));
        // Cross-check against the independent full storer-spec implementation.
        assert!(storer_binding_would_accept(&peer_id, &quote));
    }

    #[test]
    fn binding_rejects_real_crossed_keypair() {
        let (peer_id, _, quote, _, _) = bad_quote_real();
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
            committed_key_count: 0,
            commitment_pin: None,
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
            committed_key_count: 0,
            commitment_pin: None,
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
    fn witnessed_quote_launch_budget_keeps_exact_quote_window() {
        assert_eq!(
            witnessed_quote_launch_budget(0, 0, CLOSE_GROUP_SIZE * 2),
            CLOSE_GROUP_SIZE,
            "initial SNP quote fetch should launch the closest seven peers"
        );
        assert_eq!(
            witnessed_quote_launch_budget(1, CLOSE_GROUP_SIZE - 1, CLOSE_GROUP_SIZE),
            0,
            "a successful quote should not launch an extra fallback"
        );
        assert_eq!(
            witnessed_quote_launch_budget(0, CLOSE_GROUP_SIZE - 1, CLOSE_GROUP_SIZE),
            1,
            "a failed in-flight quote should launch the next closest fallback"
        );
        assert_eq!(
            witnessed_quote_launch_budget(CLOSE_GROUP_SIZE - 1, 0, 3),
            1,
            "only one more peer is needed for the seventh quote"
        );
        assert_eq!(
            witnessed_quote_launch_budget(0, 0, CLOSE_GROUP_SIZE - 1),
            CLOSE_GROUP_SIZE - 1,
            "launch budget is capped by remaining candidates"
        );
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

    /// Ascending seeds `1..=count`, each a valid `u8` peer seed.
    fn ascending_seeds(count: usize) -> Vec<u8> {
        (1..=count)
            .map(|n| u8::try_from(n).expect("test seed fits in u8"))
            .collect()
    }

    #[test]
    fn scope_witnessed_to_close_group_matches_native_close_group_query() {
        // How many of the closest-`CLOSE_GROUP_SIZE` responders returned a view.
        // The remainder are "missing", so the scoped transcript also exercises
        // the missing-views quorum adjustment.
        const RESPONDED_IN_SCOPE: usize = 5;
        // Responders past the close group whose views scoping must drop.
        const OUT_OF_SCOPE_RESPONDERS: usize = 2;

        let address = [0u8; 32];
        let close_seeds = ascending_seeds(CLOSE_GROUP_SIZE);
        // Each view's closest list mixes in-group (1, 2) and far (8, 9) peers so
        // candidate selection is non-trivial and must survive scoping verbatim.
        let view_closest = [1, 2, 8, 9];
        let in_scope_views = || -> Vec<ResponderView> {
            ascending_seeds(RESPONDED_IN_SCOPE)
                .into_iter()
                .map(|responder| witnessed_test_view(responder, &view_closest))
                .collect()
        };

        // A wide PUT_TARGET_WIDTH-peer transcript, ordered closest-first (seed n
        // == PeerId [n; 32], whose XOR distance to the zero address is n). Two
        // responders past the close group (out of scope) must be dropped.
        let mut wide_views = in_scope_views();
        for offset in 1..=OUT_OF_SCOPE_RESPONDERS {
            let responder =
                u8::try_from(CLOSE_GROUP_SIZE + offset).expect("out-of-scope seed fits in u8");
            wide_views.push(witnessed_test_view(responder, &[1, 2, 3]));
        }
        let wide = WitnessedCloseGroup {
            target: address,
            k: PUT_TARGET_WIDTH,
            initial_closest: witnessed_test_nodes(&ascending_seeds(PUT_TARGET_WIDTH)),
            responder_views: wide_views,
        };

        // The hand-built equivalent: a native CLOSE_GROUP_SIZE-wide query with
        // the same in-scope responders.
        let native = WitnessedCloseGroup {
            target: address,
            k: CLOSE_GROUP_SIZE,
            initial_closest: witnessed_test_nodes(&close_seeds),
            responder_views: in_scope_views(),
        };

        let scoped = scope_witnessed_to_close_group(&wide);

        // Target preserved; k and the initial set collapse to the close group.
        assert_eq!(scoped.target, wide.target);
        assert_eq!(scoped.k, CLOSE_GROUP_SIZE);
        assert_eq!(
            scoped
                .initial_closest
                .iter()
                .map(|node| node.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            close_seeds,
            "initial set must be the closest CLOSE_GROUP_SIZE, in order"
        );

        // Out-of-close-group responder views are dropped; the in-scope ones keep
        // their closest lists untouched (scoping filters by responder only).
        assert_eq!(
            scoped
                .responder_views
                .iter()
                .map(|view| view.responder.as_bytes()[0])
                .collect::<Vec<_>>(),
            ascending_seeds(RESPONDED_IN_SCOPE),
            "only responders inside the close group survive"
        );
        assert_eq!(
            scoped.responder_views[0]
                .closest
                .iter()
                .map(|node| node.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            view_closest.to_vec(),
            "a surviving view's closest set must be preserved verbatim"
        );

        // The quorum math and candidate consensus run on the close group only
        // and are byte-for-byte identical to the native CLOSE_GROUP_SIZE query.
        assert_eq!(
            missing_witnessed_responder_views(&scoped),
            missing_witnessed_responder_views(&native),
        );
        let quorum = witnessed_close_group_quorum_for_transcript(&scoped);
        assert_eq!(quorum, witnessed_close_group_quorum_for_transcript(&native));
        let candidate_seeds = |group: &WitnessedCloseGroup| {
            witnessed_consensus_candidates(group, &address, quorum)
                .iter()
                .map(|candidate| candidate.node.peer_id.as_bytes()[0])
                .collect::<Vec<_>>()
        };
        assert_eq!(
            candidate_seeds(&scoped),
            candidate_seeds(&native),
            "scoped consensus must match a native close-group query"
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
    fn witnessed_quote_peers_include_quorum_fallback_candidates() {
        const EXTRA_QUORUM_CANDIDATES: usize = 1;

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
        .expect("fallback candidates should be retained for quote collection");

        assert_eq!(
            selection.quote_peers.len(),
            CLOSE_GROUP_SIZE + EXTRA_QUORUM_CANDIDATES
        );
        assert_eq!(
            selection
                .quote_peers
                .iter()
                .map(|peer| peer.peer_id.as_bytes()[0])
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
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
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(selection.quorum, quorum);
    }

    #[test]
    fn witnessed_quote_selection_keeps_closest_set_with_median_voter_quorum() {
        const MEDIAN_ISSUER_SEED: u8 = 7;
        const FAR_SUPPORTING_VOTER_SEED: u8 = 20;
        const UNSUCCESSFUL_SUPPORTING_VOTER_SEED: u8 = 21;

        let address = [0u8; 32];
        let quotes = vec![
            synthetic_quote(1, 10),
            synthetic_quote(2, 20),
            synthetic_quote(3, 30),
            synthetic_quote(6, 50),
            synthetic_quote(MEDIAN_ISSUER_SEED, 40),
            synthetic_quote(8, 60),
            synthetic_quote(9, 70),
            synthetic_quote(FAR_SUPPORTING_VOTER_SEED, 80),
        ];
        let mut voters_by_peer = HashMap::new();
        voters_by_peer.insert(
            synthetic_peer(MEDIAN_ISSUER_SEED),
            synthetic_voters(&[
                1,
                2,
                3,
                MEDIAN_ISSUER_SEED,
                FAR_SUPPORTING_VOTER_SEED,
                UNSUCCESSFUL_SUPPORTING_VOTER_SEED,
            ]),
        );

        let quorum = witnessed_close_group_quorum();
        let selected =
            select_witnessed_median_voter_quotes(quotes, &address, &voters_by_peer, quorum)
                .expect("a supported close-group quote set should be selected");

        assert_eq!(quote_peer_seeds(&selected), vec![1, 2, 3, 6, 7, 8, 9]);
        let (median_peer_id, _) =
            median_paid_quote_issuer(&selected).expect("selected quotes have a median");
        assert_eq!(median_peer_id, synthetic_peer(MEDIAN_ISSUER_SEED));
        assert!(voters_by_peer[&median_peer_id].len() >= quorum);
    }

    #[test]
    fn witnessed_quote_selection_uses_direct_median_witness_recognition() {
        const MEDIAN_ISSUER_SEED: u8 = 7;

        let address = [0u8; 32];
        let quotes = vec![
            synthetic_quote(1, 10),
            synthetic_quote(2, 20),
            synthetic_quote(3, 30),
            synthetic_quote(4, 50),
            synthetic_quote(MEDIAN_ISSUER_SEED, 40),
            synthetic_quote(8, 60),
            synthetic_quote(9, 70),
        ];
        let mut voters_by_peer = HashMap::new();
        voters_by_peer.insert(
            synthetic_peer(MEDIAN_ISSUER_SEED),
            synthetic_voters(&[20, 21, 22, 23, 24]),
        );

        let quorum = witnessed_close_group_quorum();
        let selected =
            select_witnessed_median_voter_quotes(quotes, &address, &voters_by_peer, quorum)
                .expect("direct witness recognition should support the paid median issuer");

        let (median_peer_id, _) =
            median_paid_quote_issuer(&selected).expect("selected quotes have a median");
        let selected_peers = selected
            .iter()
            .map(|(peer_id, _, _, _)| *peer_id)
            .collect::<HashSet<_>>();
        assert_eq!(median_peer_id, synthetic_peer(MEDIAN_ISSUER_SEED));
        assert_eq!(
            voters_by_peer[&median_peer_id]
                .intersection(&selected_peers)
                .count(),
            0,
            "recognising witnesses need not also be selected quote issuers"
        );
        assert_eq!(voters_by_peer[&median_peer_id].len(), quorum);
    }

    #[test]
    fn witnessed_quote_selection_rejects_median_without_witness_quorum() {
        const MEDIAN_ISSUER_SEED: u8 = 7;

        let address = [0u8; 32];
        let quotes = vec![
            synthetic_quote(1, 10),
            synthetic_quote(2, 20),
            synthetic_quote(3, 30),
            synthetic_quote(6, 50),
            synthetic_quote(MEDIAN_ISSUER_SEED, 40),
            synthetic_quote(8, 60),
            synthetic_quote(9, 70),
            synthetic_quote(10, 80),
        ];
        let mut voters_by_peer = HashMap::new();
        voters_by_peer.insert(
            synthetic_peer(MEDIAN_ISSUER_SEED),
            synthetic_voters(&[1, 2, 3, 20]),
        );

        let selected = select_witnessed_median_voter_quotes(
            quotes,
            &address,
            &voters_by_peer,
            witnessed_close_group_quorum(),
        );

        assert!(
            selected.is_none(),
            "the selector must not return a paid quote set when fewer than the \
             witnessed median voter quorum recognised the paid median issuer"
        );
    }

    #[test]
    fn put_peers_prioritise_median_voters_without_reordering_quotes() {
        const MEDIAN_ISSUER_SEED: u8 = 7;

        let quotes = vec![
            synthetic_quote(1, 10),
            synthetic_quote(2, 20),
            synthetic_quote(3, 30),
            synthetic_quote(4, 50),
            synthetic_quote(5, 60),
            synthetic_quote(6, 70),
            synthetic_quote(MEDIAN_ISSUER_SEED, 40),
        ];
        let mut voters_by_peer = HashMap::new();
        voters_by_peer.insert(
            synthetic_peer(MEDIAN_ISSUER_SEED),
            synthetic_voters(&[3, 4, 5, 6, MEDIAN_ISSUER_SEED]),
        );

        let put_candidates = put_peers_from_seeds(&[1, 2, 3, 4, 5, 6, 7]);
        let put_peers = put_peers_with_median_voters_first(
            &quotes,
            &put_candidates,
            &voters_by_peer,
            witnessed_close_group_quorum(),
        )
        .expect("median voters should produce an ordered PUT set");

        assert_eq!(quote_peer_seeds(&quotes), vec![1, 2, 3, 4, 5, 6, 7]);
        let (median_peer_id, _) =
            median_paid_quote_issuer(&quotes).expect("selected quotes have a median");
        assert_eq!(median_peer_id, synthetic_peer(MEDIAN_ISSUER_SEED));
        assert_eq!(put_peer_seeds(&put_peers), vec![3, 4, 5, 6, 7, 1, 2]);
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
        for (peer_id, _, quote, _, _) in &quotes {
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
        for (peer_id, _, quote, _, _) in &quotes {
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
        let (peer_id, addrs, original_quote, amount, commitment) = good_quote_real();
        let mut quotes = vec![(
            peer_id,
            addrs.clone(),
            original_quote.clone(),
            amount,
            commitment,
        )];
        let _ = drop_quotes_with_bad_bindings(&mut quotes);

        let (kept_peer, kept_addrs, kept_quote, kept_amount, _kept_commitment) =
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
            .filter(|(p, _, q, _, _)| !storer_binding_would_accept(p, q))
            .count();
        assert_eq!(
            storer_would_reject_count, 1,
            "exactly one quote (the crossed-key one) must be rejected by the storer spec"
        );

        // Step 2: run the patched filter.
        let dropped = drop_quotes_with_bad_bindings(&mut quotes);
        assert_eq!(dropped, 1, "exactly the crossed-key quote must be filtered");

        // Step 3: prove the storer would accept every survivor under the FULL spec.
        for (peer_id, _, quote, _, _) in &quotes {
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
        for (peer_id, _, quote, _, _) in &quotes {
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
        // A properly-signed baseline quote for the requested content passes the
        // full client gate (binding + content + signature + price).
        let content = [7u8; 32];
        let (peer_id, quote) = signed_baseline_quote(content);
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &content, &bytes, false, None);
        match result {
            Ok((q, price, commitment)) => {
                assert_eq!(q.pub_key, quote.pub_key);
                assert_eq!(price, quote.price);
                assert!(commitment.is_none(), "baseline quote ships no commitment");
            }
            Err(e) => panic!("expected Ok, got {e}"),
        }
    }

    #[test]
    fn classifier_rejects_quote_with_invalid_signature() {
        // A quote whose pub_key binds correctly but whose signature is bogus is
        // dropped BEFORE payment (the storer would reject it and burn the pay).
        let content = [7u8; 32];
        let (peer_id, mut quote) = signed_baseline_quote(content);
        quote.signature = vec![0u8; quote.signature.len()]; // corrupt the signature
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &content, &bytes, false, None);
        assert!(
            matches!(result, Err(Error::BadQuoteBinding { .. })),
            "a quote with an invalid signature must be rejected; got {result:?}"
        );
    }

    #[test]
    fn classifier_rejects_quote_for_wrong_content() {
        // A validly-signed quote for a DIFFERENT address is dropped before pay.
        let (peer_id, quote) = signed_baseline_quote([7u8; 32]);
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &[9u8; 32], &bytes, false, None);
        assert!(
            matches!(result, Err(Error::BadQuoteBinding { .. })),
            "a quote for the wrong content must be rejected; got {result:?}"
        );
    }

    #[test]
    fn classifier_rejects_crossed_keypair_with_typed_error() {
        let (peer_id, _, quote, _, _) = bad_quote_real();
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &[0u8; 32], &bytes, false, None);
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
        let (peer_id, _, quote, _, _) = bad_quote_real();
        let bytes = serialize_quote(&quote);
        // The peer claims already_stored=true, but its quote has a crossed key.
        let result = classify_quote_response(&peer_id, &[0u8; 32], &bytes, true, None);
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
        let content = [7u8; 32];
        let (peer_id, quote) = signed_baseline_quote(content);
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&peer_id, &content, &bytes, true, None);
        assert!(
            matches!(result, Err(Error::AlreadyStored)),
            "honest peer's already_stored vote must be honoured; got {result:?}"
        );
    }

    #[test]
    fn classifier_returns_serialization_error_on_bad_bytes() {
        let (peer_id, _, _, _, _) = good_quote_real();
        let garbage = b"this is not a valid msgpack PaymentQuote".to_vec();
        let result = classify_quote_response(&peer_id, &[0u8; 32], &garbage, false, None);
        assert!(
            matches!(result, Err(Error::Serialization(_))),
            "garbage bytes must produce a Serialization error; got {result:?}"
        );
    }

    /// Cross-validate the classifier's binding verdict against the
    /// independent storer-spec re-derivation across mixed responders.
    #[test]
    fn classifier_verdict_matches_storer_binding_spec_for_mixed_responders() {
        let content = [7u8; 32];
        let mut responders: Vec<(PeerId, PaymentQuote)> =
            (0..12).map(|_| signed_baseline_quote(content)).collect();
        for _ in 0..4 {
            let (p, _, q, _, _) = bad_quote_real();
            responders.push((p, q));
        }

        for (peer_id, quote) in &responders {
            let bytes = serialize_quote(quote);
            let storer_verdict = storer_binding_would_accept(peer_id, quote);
            let classifier_verdict =
                classify_quote_response(peer_id, &content, &bytes, false, None).is_ok();
            assert_eq!(
                classifier_verdict, storer_verdict,
                "classifier and storer-binding-spec must agree on every responder \
                 (peer_id={}, storer={storer_verdict}, classifier={classifier_verdict})",
                peer_id
            );
        }
    }

    // ============================================================
    // ADR-0003: quote_commitment_binding_is_valid (forced-price gate)
    //
    // Mirrors the storer-side `binding_violation` in
    // `ant-node/src/payment/verifier.rs`. The client runs this before
    // paying so it never pays a quote the storer's arithmetic gate would
    // reject. The client now runs the FULL check (shape, cap, exact price,
    // and for bound quotes: parse + peer-binding + signature + hash==pin +
    // count==key_count) using the shared ant-protocol commitment type, so an
    // unresolvable/forged commitment is never paid. A live resolve against a
    // REAL signed commitment is proven in the e2e suite (e2e_adr0003.rs).
    // ============================================================

    /// A throwaway peer id for tests that fail BEFORE commitment resolution
    /// (shape/cap/price checks don't depend on the peer).
    fn any_peer() -> PeerId {
        PeerId::from_bytes([0u8; 32])
    }

    /// Build a quote carrying a specific `(count, pin, price)` binding.
    fn quote_with_binding(
        committed_key_count: u32,
        commitment_pin: Option<[u8; 32]>,
        price: Amount,
    ) -> PaymentQuote {
        PaymentQuote {
            content: XorName([0u8; 32]),
            timestamp: SystemTime::UNIX_EPOCH,
            price,
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: Vec::new(),
            signature: Vec::new(),
            committed_key_count,
            commitment_pin,
        }
    }

    #[test]
    fn binding_baseline_ok_only_at_baseline_price() {
        // (0, None) priced at calculate_price(0) is the valid baseline.
        let q = quote_with_binding(0, None, calculate_price(0));
        assert!(quote_commitment_binding_is_valid(&any_peer(), &q, &None).is_ok());

        // (0, None) priced above baseline is rejected — the forged-shape
        // bypass (strip the pin, charge more than the empty-node price).
        let q = quote_with_binding(0, None, calculate_price(500));
        assert!(quote_commitment_binding_is_valid(&any_peer(), &q, &None).is_err());
    }

    #[test]
    fn binding_rejects_incoherent_shapes() {
        // count > 0 but no pin: unauditable.
        let q = quote_with_binding(500, None, calculate_price(500));
        assert!(quote_commitment_binding_is_valid(&any_peer(), &q, &None).is_err());
        // count 0 but a pin: incoherent baseline.
        let q = quote_with_binding(0, Some([9u8; 32]), calculate_price(0));
        assert!(quote_commitment_binding_is_valid(&any_peer(), &q, &None).is_err());
    }

    #[test]
    fn binding_rejects_count_above_cap() {
        let over = MAX_COMMITMENT_KEY_COUNT + 1;
        let q = quote_with_binding(over, Some([9u8; 32]), calculate_price(over as usize));
        assert!(
            quote_commitment_binding_is_valid(&any_peer(), &q, &Some(vec![1u8; 16])).is_err(),
            "a count above MAX_COMMITMENT_KEY_COUNT must be rejected before payment"
        );
    }

    #[test]
    fn binding_rejects_on_curve_wrong_count() {
        // Priced for 499 but claims count 500 — on a real price curve but the
        // wrong count. Rejected at the exact-price check, before resolution.
        let q = quote_with_binding(500, Some([9u8; 32]), calculate_price(499));
        assert!(quote_commitment_binding_is_valid(&any_peer(), &q, &Some(vec![1u8; 16])).is_err());
    }

    #[test]
    fn binding_rejects_bound_quote_without_shipped_commitment() {
        // A bound quote whose commitment did not arrive is unresolvable, so it
        // is dropped before payment even though its price is on the curve.
        let q = quote_with_binding(500, Some([9u8; 32]), calculate_price(500));
        assert!(
            quote_commitment_binding_is_valid(&any_peer(), &q, &None).is_err(),
            "a bound quote missing its commitment must be rejected"
        );
    }

    #[test]
    fn binding_rejects_garbage_and_wrong_pin_commitment() {
        // A bound quote whose shipped commitment is garbage (doesn't even
        // deserialize) is rejected — the client never pays an unresolvable pin.
        let q = quote_with_binding(500, Some([9u8; 32]), calculate_price(500));
        assert!(
            quote_commitment_binding_is_valid(&any_peer(), &q, &Some(vec![0xFF; 8])).is_err(),
            "an unparseable commitment must be rejected before payment"
        );

        // A well-formed-but-wrong commitment (valid StorageCommitment bytes that
        // do NOT hash to the quote's pin / aren't bound to the peer) is also
        // rejected. We serialize a real StorageCommitment shape with mismatched
        // fields; it fails peer-binding first, then would fail hash==pin.
        let bogus = StorageCommitment {
            root: [1u8; 32],
            key_count: 500,
            sender_peer_id: [2u8; 32], // not the quoting peer
            sender_public_key: vec![3u8; 1952],
            signature: vec![4u8; 3293],
        };
        let blob = rmp_serde::to_vec(&bogus).expect("serialize bogus commitment");
        assert!(
            quote_commitment_binding_is_valid(&any_peer(), &q, &Some(blob)).is_err(),
            "a commitment not bound to the quoting peer must be rejected before payment"
        );
    }

    #[test]
    fn binding_rejects_oversized_commitment_before_parsing() {
        // A bound quote shipping a blob larger than the sidecar cap is rejected
        // before any deserialize attempt (DoS guard on the hot path).
        let q = quote_with_binding(500, Some([9u8; 32]), calculate_price(500));
        let huge = Some(vec![0u8; MAX_COMMITMENT_SIDECAR_BYTES + 1]);
        assert!(
            quote_commitment_binding_is_valid(&any_peer(), &q, &huge).is_err(),
            "an oversized commitment blob must be rejected before payment"
        );
    }

    #[test]
    fn classifier_drops_off_curve_quote_with_typed_error() {
        // End-to-end through the classifier: a VALIDLY-SIGNED, correctly-bound
        // quote for the right content, but with an off-curve price, is dropped
        // as BadQuoteCommitment (the forced-price extraction guard fires after
        // the quote's own signature/content checks pass).
        use ant_protocol::pqc::ops::MlDsaSecretKey;
        let content = [7u8; 32];
        let kp = gen_keypair();
        let mut quote = PaymentQuote {
            content: XorName(content),
            timestamp: SystemTime::UNIX_EPOCH,
            // claims baseline shape but charges a non-baseline price
            price: calculate_price(500),
            rewards_address: RewardsAddress::new([0u8; 20]),
            pub_key: kp.pub_key_bytes.clone(),
            signature: Vec::new(),
            committed_key_count: 0,
            commitment_pin: None,
        };
        let ml_dsa = MlDsa65::new();
        let sk = MlDsaSecretKey::from_bytes(&kp.secret_key_bytes).expect("sk");
        quote.signature = ml_dsa
            .sign(&sk, &quote.bytes_for_sig())
            .expect("sign")
            .as_bytes()
            .to_vec();
        let bytes = serialize_quote(&quote);
        let result = classify_quote_response(&kp.peer_id, &content, &bytes, false, None);
        assert!(
            matches!(result, Err(Error::BadQuoteCommitment { .. })),
            "off-curve quote must be dropped as BadQuoteCommitment; got {result:?}"
        );
    }
}
