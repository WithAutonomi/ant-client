//! Chunk storage operations.
//!
//! Chunks are immutable, content-addressed data blocks where the address
//! is the BLAKE3 hash of the content.

use crate::data::client::adaptive::Outcome;
use crate::data::client::batch::{finalize_batch_payment, PreparedChunk};
use crate::data::client::peer_xor_distance;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{QuoteHash, TxHash};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{
    compute_address, detect_proof_type, send_and_await_chunk_response, ChunkGetRequest,
    ChunkGetResponse, ChunkMessage, ChunkMessageBody, ChunkPutRequest, ChunkPutResponse, DataChunk,
    ProofType, ProtocolError, XorName, CLOSE_GROUP_MAJORITY,
};
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tracing::{debug, info, warn};

/// Data type identifier for chunks (used in quote requests).
const CHUNK_DATA_TYPE: u32 = 0;

/// Why a single-peer PUT was declined. Drives the surfaced aggregate error
/// and keeps the store AIMD limiter honest — only genuine local backpressure
/// (a PUT-response `Timeout`) is a "client is sending too fast" signal; a node
/// that responds with a structured rejection is an application-level decline
/// (ADR-0002 / V2-468), and a bare dial/relay failure is remote peer churn,
/// not local capacity (V2-554).
#[derive(Clone, Copy)]
enum PutRejection {
    /// Node is out of storage (`ProtocolError::StorageFailed`) — try a
    /// further peer.
    Full,
    /// Payment did not clear the node's local price floor, or the proof's
    /// issuers are not close enough in this peer's view
    /// (`ProtocolError::PaymentFailed`), or the node asked for more than was
    /// paid (`ChunkPutResponse::PaymentRequired` → [`Error::Payment`]) — skip
    /// this peer, do not re-quote.
    PriceFloor,
    /// Some other structured remote rejection.
    OtherRemote,
    /// The peer accepted the connection but did not answer the PUT within the
    /// deadline (`Error::Timeout`). This is the genuine local-backpressure
    /// signal: under real congestion the client's own requests time out, so a
    /// shortfall carrying any timeout stays a capacity signal (V2-554).
    Timeout,
    /// A dial/relay/transport failure — the peer could not be reached at all
    /// (`Error::Network` and other non-response errors), typically a dead or
    /// stale relayed DHT address. This is remote peer churn, not local
    /// backpressure, so a shortfall made up purely of these must not push the
    /// store AIMD limiter down (V2-554).
    Dial,
}

/// Classify a failed single-peer PUT (ADR-0002 / V2-468 / V2-554). A
/// `RemotePut` carries the node's structured `ProtocolError`; a
/// `PaymentRequired` response surfaces as [`Error::Payment`]; a
/// [`Error::Timeout`] is genuine local backpressure; anything else is a
/// dial/relay failure (remote churn).
fn classify_put_failure(error: &Error) -> PutRejection {
    match error {
        Error::RemotePut { source, .. } => match source {
            ProtocolError::StorageFailed(_) => PutRejection::Full,
            ProtocolError::PaymentFailed(_) => PutRejection::PriceFloor,
            _ => PutRejection::OtherRemote,
        },
        // A `PaymentRequired` PUT response (the node wants more than was paid)
        // arrives as `Error::Payment`. It is a structured application-level
        // decline — skip the peer and advance fallback, exactly like a
        // price-floor `PaymentFailed` — not a transport shortfall, so it must
        // not push the store AIMD limiter down (ADR-0002 / V2-468).
        Error::Payment(_) => PutRejection::PriceFloor,
        // The peer did not answer in time: genuine local backpressure.
        Error::Timeout(_) => PutRejection::Timeout,
        // Could not reach the peer at all: dial/relay churn (remote), not a
        // local-capacity signal.
        _ => PutRejection::Dial,
    }
}

/// Decide the error for a close-group store that fell short of quorum.
///
/// Only genuine local backpressure should push the store AIMD limiter down.
/// The failure mix decides which signal to surface:
///
/// 1. **Any PUT-response timeout** (`timeout > 0`): the client's own requests
///    are timing out — genuine local congestion. Surface `InsufficientPeers`
///    (classified `NetworkError`) so the limiter still backs off (V2-554).
/// 2. **No timeouts, no dial failures** — every failure was an application
///    decline (full / price-floor / `PaymentRequired` / other remote
///    rejection): surface the representative application error so the shortfall
///    classifies `ApplicationError` and does not suppress the limiter
///    (ADR-0002 / V2-468).
/// 3. **No timeouts, but dial/relay failures present**: the shortfall is
///    close-group dial churn (dead/stale relayed peer addresses) — remote peer
///    churn, not local capacity. Surface [`Error::CloseGroupShortfall`]
///    (classified `ApplicationError`) so it does NOT push the limiter down
///    (V2-554). Still recoverable/retryable.
fn put_shortfall_error(
    timeout: usize,
    dial: usize,
    first_app_rejection: Option<Error>,
    shortfall_message: String,
) -> Error {
    if timeout > 0 {
        return Error::InsufficientPeers(shortfall_message);
    }
    if dial == 0 {
        if let Some(app_rejection) = first_app_rejection {
            return app_rejection;
        }
    }
    Error::CloseGroupShortfall(shortfall_message)
}

/// Result of one sweep over a chunk's close group.
///
/// Either we got the chunk from some peer, or every peer in the group
/// returned NotFound, timed out, or hit a transport / protocol error.
/// The counts feed the retry decision (`is_authoritative_not_found`):
/// only a *unanimous* NotFound from a *well-sampled* close group counts
/// as authoritative data absence — anything else (a non-unanimous
/// result, or a thin/under-sampled DHT walk) leaves room for the actual
/// storer to be in the timeout / network-error / protocol-error bucket
/// or outside the sampled view, and is worth a retry against a freshly
/// re-walked close group.
struct CloseGroupOutcome {
    chunk: Option<DataChunk>,
    queried: usize,
    not_found: usize,
    timeout: usize,
    network_err: usize,
    /// Counts peers that responded with a remote `Error` (e.g.
    /// "Chunk verification failed") or any other protocol-level error
    /// that classifies as `Error::Protocol`. Treated the same as
    /// `timeout` / `network_err` for retry decisions: one peer's bad
    /// response must not abort the whole close-group sweep — the
    /// remaining peers might still have a clean copy.
    protocol_err: usize,
}

/// `true` if the close-group sweep is strong enough evidence to
/// conclude the chunk is genuinely absent, so retrying is pointless.
///
/// Two conditions, both required:
///
/// 1. *Unanimous*: every peer we managed to query responded with an
///    authoritative NotFound (`not_found == queried`). An earlier
///    version used a majority quorum (`not_found >= close_group_size /
///    2 + 1`), but production traffic disproved that: storage
///    replicates to `CLOSE_GROUP_MAJORITY` (4) of the K=7 close-group
///    peers, so up to 3 peers legitimately don't store any given chunk
///    and a `not_found=4 timeout=3` result is "3 storers we couldn't
///    reach" plus "4 non-storers," not data loss.
///
/// 2. *Well-sampled*: at least `CLOSE_GROUP_MAJORITY` peers were
///    queried. `closest_peers` (via `find_closest_peers`) accepts
///    any non-empty DHT result, so a thin/under-sampled walk can return
///    1 or 2 peers. A `1/1` or `3/3` NotFound from such a walk is NOT
///    authoritative — the real replica majority may sit entirely
///    outside that narrow view. Requiring a majority-sized sample means
///    a thin lookup falls through to the retry (which re-walks the DHT)
///    instead of being declared a final absence.
fn is_authoritative_not_found(not_found: usize, queried: usize) -> bool {
    queried >= CLOSE_GROUP_MAJORITY && not_found == queried
}

/// Store-response timeout for non-merkle chunk PUTs.
const STORE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Extra waves allowed after the computed diagnostic peer-sweep deadline.
const DIAGNOSTIC_TIMEOUT_PADDING_WAVES: usize = 1;

/// Result of fetching one chunk address from one close-group peer.
pub struct ChunkPeerGetResult {
    /// Peer queried for the chunk.
    pub peer_id: PeerId,
    /// Known network addresses used for the peer.
    pub peer_addrs: Vec<MultiAddr>,
    /// XOR distance from `peer_id` to the chunk address.
    pub xor_distance: [u8; 32],
    /// Per-peer fetch result.
    pub chunk_result: Result<Option<DataChunk>>,
}

#[derive(Clone)]
struct ChunkPeerGetTarget {
    index: usize,
    peer_id: PeerId,
    peer_addrs: Vec<MultiAddr>,
    xor_distance: [u8; 32],
}

fn chunk_peer_get_targets(
    peers: Vec<(PeerId, Vec<MultiAddr>)>,
    address: &XorName,
) -> Vec<ChunkPeerGetTarget> {
    peers
        .into_iter()
        .enumerate()
        .map(|(index, (peer_id, peer_addrs))| ChunkPeerGetTarget {
            index,
            peer_id,
            peer_addrs,
            xor_distance: peer_xor_distance(&peer_id, address),
        })
        .collect()
}

fn sort_chunk_peer_get_results(results: &mut [ChunkPeerGetResult]) {
    results.sort_by_key(|result| result.xor_distance);
}

fn diagnostic_peer_get_concurrency(peer_count: usize, close_group_size: usize) -> usize {
    peer_count.min(close_group_size.max(1))
}

fn diagnostic_peer_get_overall_timeout(
    per_peer_timeout: Duration,
    target_count: usize,
    concurrency_limit: usize,
) -> Duration {
    let concurrency_limit = concurrency_limit.max(1);
    let peer_get_waves = target_count.div_ceil(concurrency_limit);
    let timeout_waves = peer_get_waves.saturating_add(DIAGNOSTIC_TIMEOUT_PADDING_WAVES);
    let timeout_waves = u32::try_from(timeout_waves).unwrap_or(u32::MAX);

    per_peer_timeout.saturating_mul(timeout_waves)
}

fn timed_out_chunk_peer_get_result(
    target: &ChunkPeerGetTarget,
    address: &XorName,
    timeout: Duration,
) -> ChunkPeerGetResult {
    let addr_hex = hex::encode(address);
    let timeout_secs = timeout.as_secs();
    ChunkPeerGetResult {
        peer_id: target.peer_id,
        peer_addrs: target.peer_addrs.clone(),
        xor_distance: target.xor_distance,
        chunk_result: Err(Error::Timeout(format!(
            "Diagnostic chunk GET sweep timed out before peer {} completed for chunk {addr_hex} after {timeout_secs}s",
            target.peer_id
        ))),
    }
}

fn store_response_timeout_for_proof(proof: &[u8], merkle_timeout_secs: u64) -> Duration {
    match detect_proof_type(proof) {
        Some(ProofType::Merkle) => Duration::from_secs(merkle_timeout_secs),
        _ => STORE_RESPONSE_TIMEOUT,
    }
}

impl Client {
    /// Run `chunk_get` and feed one byte-aware observation per call to
    /// the adaptive fetch limiter. Use this from any consumer that
    /// drives chunk-fetch concurrency from `controller().fetch.current()`
    /// — the controller's window relies on every call along the hot
    /// path producing an observation.
    ///
    /// Classifier semantics: see `chunk_get_outcome`. Most importantly,
    /// `Ok(None)` is treated as `Outcome::Timeout`, not Success, so a
    /// sustained run of close-group exhaustions correctly drives the
    /// cap down rather than silently inflating it.
    pub(crate) async fn chunk_get_observed(&self, address: &XorName) -> Result<Option<DataChunk>> {
        self.chunk_get_observed_from_closest_peers(address, self.config().close_group_size)
            .await
    }

    pub(crate) async fn chunk_get_observed_from_closest_peers(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<Option<DataChunk>> {
        let started = Instant::now();
        let result = self.chunk_get_from_closest_peers(address, peer_count).await;
        let latency = started.elapsed();
        let bytes = result
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map_or(0, |chunk| chunk.content.len() as u64);
        self.controller()
            .fetch
            .observe_with_bytes(chunk_get_outcome(&result), latency, bytes);
        result
    }
}

/// Map a `chunk_get` outcome to an adaptive controller `Outcome`.
///
/// This is the result-aware classifier used by the file-download paths.
/// It differs from `classify_error` in one critical way: an `Ok(None)`
/// from `chunk_get` is `Outcome::Timeout`, not `Outcome::Success`. By
/// the time `chunk_get` returns `Ok(None)` it has already exhausted
/// the close group across its first attempt + retry sweep, so
/// `Ok(None)` is the controller's load-shedding signal — a sustained
/// run of them on a saturated home link is exactly the case where the
/// cap should shrink.
///
/// Healthy returns (`Ok(Some(_))`) are Success regardless of how many
/// internal peer attempts the chunk_get had to make. The controller
/// does not need to see internal peer noise; that's noise about the
/// production network's natural peer-side variability, not about the
/// client's effective capacity.
pub(crate) fn chunk_get_outcome(result: &Result<Option<DataChunk>>) -> Outcome {
    match result {
        Ok(Some(_)) => Outcome::Success,
        Ok(None) => Outcome::Timeout,
        Err(Error::Timeout(_)) => Outcome::Timeout,
        Err(Error::Network(_)) => Outcome::NetworkError,
        Err(_) => Outcome::ApplicationError,
    }
}

impl Client {
    /// Store a chunk on the Autonomi network with payment.
    ///
    /// Checks if the chunk already exists before paying. If it does,
    /// returns the address immediately without incurring on-chain costs.
    /// Otherwise collects quotes, pays on-chain, then stores with proof
    /// to `CLOSE_GROUP_MAJORITY` peers.
    ///
    /// # Errors
    ///
    /// Returns an error if payment or the network operation fails.
    pub async fn chunk_put(&self, content: Bytes) -> Result<XorName> {
        let address = compute_address(&content);
        let data_size = u64::try_from(content.len())
            .map_err(|e| Error::InvalidData(format!("content size too large: {e}")))?;

        match self
            .pay_for_storage(&address, data_size, CHUNK_DATA_TYPE)
            .await
        {
            Ok((proof, peers)) => self.chunk_put_to_close_group(content, proof, &peers).await,
            Err(Error::AlreadyStored) => {
                debug!(
                    "Chunk {} already stored on network, skipping payment",
                    hex::encode(address)
                );
                Ok(address)
            }
            Err(e) => Err(e),
        }
    }

    /// Test-only: pay for `content`, then store it with `dead_count`
    /// unreachable peers prepended to the real put-target set.
    ///
    /// Every initial send hits a dead peer and fails, so the store can only
    /// reach quorum by falling back through the real put-targets (the closest-K
    /// set the quote plan already returned), reusing the same `ProofOfPayment`.
    /// Pass `dead_count >= CLOSE_GROUP_MAJORITY` so a full quorum's worth of
    /// replacements must come from the fallback; a success proves the fallback
    /// works end-to-end.
    ///
    /// # Errors
    ///
    /// Returns an error if payment fails or quorum cannot be reached.
    #[cfg(feature = "test-utils")]
    pub async fn chunk_put_with_dead_initial_peers(
        &self,
        content: Bytes,
        dead_count: usize,
    ) -> Result<XorName> {
        let address = compute_address(&content);
        let data_size = u64::try_from(content.len())
            .map_err(|e| Error::InvalidData(format!("content size too large: {e}")))?;
        let (proof, real_peers) = self
            .pay_for_storage(&address, data_size, CHUNK_DATA_TYPE)
            .await?;
        // Unreachable peers (random id, no addresses) first: every initial send
        // fails, so quorum can only be reached by falling back through the real
        // put-target set that follows.
        let mut peers: Vec<(PeerId, Vec<MultiAddr>)> = (0..dead_count)
            .map(|_| (PeerId::random(), Vec::new()))
            .collect();
        peers.extend(real_peers);
        self.chunk_put_to_close_group(content, proof, &peers).await
    }

    /// Store a chunk to `CLOSE_GROUP_MAJORITY` peers, falling back past full or
    /// over-priced members of the supplied put-target set (ADR-0002).
    ///
    /// Sends the PUT concurrently to the first `CLOSE_GROUP_MAJORITY` peers. On
    /// each failure it advances to the next peer in `peers` — which the caller
    /// supplies as the chunk's closest ~K neighbourhood, so no further DHT
    /// lookup is needed. Every peer reuses the same payment proof: a node
    /// accepts it as long as one of the proof's quote issuers is within that
    /// peer's own local closest view, so the client never needs to re-quote or
    /// re-pay to route around a full node.
    ///
    /// # Errors
    ///
    /// Returns an error if fewer than `CLOSE_GROUP_MAJORITY` peers accept
    /// the chunk.
    pub(crate) async fn chunk_put_to_close_group(
        &self,
        content: Bytes,
        proof: Vec<u8>,
        peers: &[(PeerId, Vec<MultiAddr>)],
    ) -> Result<XorName> {
        let address = compute_address(&content);

        let outcome = crate::client_engine::quorum_with_fallback(
            peers.iter().cloned(),
            CLOSE_GROUP_MAJORITY,
            |(peer_id, addrs)| {
                let content = content.clone();
                let proof = proof.clone();
                async move { self.spawn_chunk_put(content, proof, peer_id, addrs).await.1 }
            },
        )
        .await;
        let success_count = outcome.successes;
        let mut failures: Vec<String> = Vec::new();
        // Tally the *cause* of each failure. The store AIMD limiter must only be
        // pushed down by a transport shortfall (V2-468): a node that responds —
        // a structured `RemotePut` decline, or `PaymentRequired` surfacing as
        // `Error::Payment` — declined at the application layer and is not
        // evidence the client is sending too fast. The per-cause counts also
        // surface a legible aggregate reason; hold the first application-level
        // rejection as the representative error.
        let mut full = 0usize;
        let mut price_floor = 0usize;
        let mut other_remote = 0usize;
        let mut timeout = 0usize;
        let mut dial = 0usize;
        let mut first_app_rejection: Option<Error> = None;

        for ((peer_id, _), error) in outcome.failures {
            warn!("Failed to store chunk on {peer_id}: {error}");
            failures.push(format!("{peer_id}: {error}"));
            match classify_put_failure(&error) {
                PutRejection::Full => full += 1,
                PutRejection::PriceFloor => price_floor += 1,
                PutRejection::OtherRemote => other_remote += 1,
                PutRejection::Timeout => timeout += 1,
                PutRejection::Dial => dial += 1,
            }
            // An application-level decline is `RemotePut` (a structured node
            // rejection) or `Error::Payment` (`PaymentRequired`): capture the
            // first so an all-application shortfall surfaces as
            // `ApplicationError`, not `InsufficientPeers` (`NetworkError`).
            if matches!(error, Error::RemotePut { .. } | Error::Payment(_))
                && first_app_rejection.is_none()
            {
                first_app_rejection = Some(error);
            }
        }

        if outcome.reached {
            debug!(
                "Chunk {} stored on {success_count} peers (majority reached)",
                hex::encode(address)
            );
            return Ok(address);
        }

        // Quorum not reached. A timeout-bearing shortfall is genuine local
        // backpressure (capacity signal); an application-only shortfall surfaces
        // the representative app error; a pure dial-churn shortfall surfaces a
        // neutral `CloseGroupShortfall` (V2-554). See `put_shortfall_error`.
        let aggregate = format!(
            "Stored on {success_count} peers, need {CLOSE_GROUP_MAJORITY} \
             (full: {full}, price-floor: {price_floor}, other-rejection: {other_remote}, \
             timeout: {timeout}, dial: {dial}). Failures: [{}]",
            failures.join("; ")
        );
        Err(put_shortfall_error(
            timeout,
            dial,
            first_app_rejection,
            aggregate,
        ))
    }

    /// Build a chunk PUT future for a single peer. Takes owned peer data so
    /// the future can outlive a fallback queue entry popped per iteration.
    async fn spawn_chunk_put(
        &self,
        content: Bytes,
        proof: Vec<u8>,
        peer_id: PeerId,
        addrs: Vec<MultiAddr>,
    ) -> (PeerId, Result<XorName>) {
        let result = self
            .chunk_put_with_proof(content, proof, &peer_id, &addrs)
            .await;
        (peer_id, result)
    }

    /// Store a chunk on the Autonomi network with a pre-built payment proof.
    ///
    /// Sends to a single peer. Callers that need replication across the
    /// close group should use `chunk_put_to_close_group` instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_put_with_proof(
        &self,
        content: Bytes,
        proof: Vec<u8>,
        target_peer: &PeerId,
        peer_addrs: &[MultiAddr],
    ) -> Result<XorName> {
        let address = compute_address(&content);
        let node = self.network().node();
        let timeout =
            store_response_timeout_for_proof(&proof, self.config().merkle_store_timeout_secs);
        let timeout_secs = timeout.as_secs();

        let request_id = self.next_request_id();
        // `content` is a refcounted `Bytes` shared with the sibling
        // close-group sends; pass it through directly so each peer shares
        // the same backing buffer instead of deep-copying the 4 MB payload.
        let request = ChunkPutRequest::with_payment(address, content, proof);
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::PutRequest(request),
        };
        let message_bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("Failed to encode PUT request: {e}")))?;

        let addr_hex = hex::encode(address);

        let result = send_and_await_chunk_response(
            node,
            target_peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            |body| match body {
                ChunkMessageBody::PutResponse(ChunkPutResponse::Success { address: addr }) => {
                    debug!("Chunk stored at {}", hex::encode(addr));
                    Some(Ok(addr))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::AlreadyExists {
                    address: addr,
                }) => {
                    debug!("Chunk already exists at {}", hex::encode(addr));
                    Some(Ok(addr))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::PaymentRequired { message }) => {
                    Some(Err(Error::Payment(format!("Payment required: {message}"))))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::Error(e)) => {
                    // Preserve the structured remote reason instead of
                    // flattening it into `Error::Protocol`. The node
                    // responded, so the transport round-trip succeeded —
                    // this is an application-level rejection and must not
                    // suppress the store AIMD limiter (V2-468).
                    Some(Err(Error::RemotePut {
                        address: addr_hex.clone(),
                        source: e,
                    }))
                }
                _ => None,
            },
            |e| Error::Network(format!("Failed to send PUT to peer: {e}")),
            || {
                Error::Timeout(format!(
                    "Timeout waiting for store response after {timeout_secs}s"
                ))
            },
        )
        .await;

        result
    }

    /// Retrieve a chunk from the Autonomi network.
    ///
    /// Queries all peers in the close group for the chunk address,
    /// returning the first successful response. This handles the case
    /// where the storing peer differs from the first peer returned by
    /// DHT routing.
    ///
    /// ## Adaptive controller feedback
    ///
    /// Each per-peer GET attempt is fed individually to the adaptive
    /// fetch limiter via `controller().fetch.observe(...)`. This is
    /// deliberately finer-grained than wrapping the outer `chunk_get`
    /// with `observe_op`: when a chunk takes 6 peer tries to land,
    /// 5 of them are real capacity signals (timeouts / network errors)
    /// that should pull the cap down even if the chunk eventually
    /// succeeds. The outer `Ok(_)` would mask all five as a single
    /// `Outcome::Success`. See `adaptive::Outcome` for the per-attempt
    /// classification rules used below.
    ///
    /// Callers should therefore NOT wrap `chunk_get` in `observe_op`.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_get(&self, address: &XorName) -> Result<Option<DataChunk>> {
        self.chunk_get_from_closest_peers(address, self.config().close_group_size)
            .await
    }

    /// Retrieve a chunk from the requested number of closest peers.
    ///
    /// Queries peers in XOR-distance order for the chunk address,
    /// returning the first successful response. This handles the case
    /// where the storing peer differs from the first peer returned by
    /// DHT routing.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_get_from_closest_peers(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<Option<DataChunk>> {
        // Check cache first, with integrity verification.
        if let Some(cached) = self.chunk_cache().get(address) {
            let computed = compute_address(&cached);
            if computed == *address {
                debug!("Cache hit for chunk {}", hex::encode(address));
                return Ok(Some(DataChunk::new(*address, cached)));
            }
            // Cache entry corrupted — evict and fall through to network fetch.
            debug!(
                "Cache corruption detected for {}: evicting",
                hex::encode(address)
            );
            self.chunk_cache().remove(address);
        }

        let addr_hex = hex::encode(address);

        // First attempt against the current close-group view. A
        // lookup/transport error here (e.g. closest_peers' DHT walk
        // momentarily returning an error, or InsufficientPeers from a
        // thin routing table) is NOT fatal: fall through to the retry
        // path exactly as a non-authoritative miss would. Otherwise one
        // transient error on the *initial* close-group walk for a single
        // chunk would fail an entire multi-hundred-chunk download. A
        // zeroed outcome (queried=0) is never authoritative, so it flows
        // straight to the retry below.
        let first = match self.chunk_get_try_closest_peers(address, peer_count).await {
            Ok(outcome) => outcome,
            Err(e) => {
                info!("chunk_get first close-group lookup failed for {addr_hex}: {e}; will retry");
                CloseGroupOutcome {
                    chunk: None,
                    queried: 0,
                    not_found: 0,
                    timeout: 0,
                    network_err: 0,
                    protocol_err: 0,
                }
            }
        };
        if let Some(chunk) = first.chunk {
            self.chunk_cache().put(chunk.address, chunk.content.clone());
            return Ok(Some(chunk));
        }

        // Only treat as authoritative absence when *every* queried peer
        // responded NotFound. Anything less leaves the actual storer
        // possibly in the timeout / network-error bucket, which a retry
        // could reach.
        if is_authoritative_not_found(first.not_found, first.queried) {
            info!(
                "chunk_get giving up on {addr_hex} (unanimous NotFound): \
                 queried={} not_found={} timeout={} network_err={} protocol_err={}",
                first.queried,
                first.not_found,
                first.timeout,
                first.network_err,
                first.protocol_err,
            );
            return Ok(None);
        }

        // Otherwise the failure looks like reachability (most peers timed out
        // or hit transport errors). The chunk is most likely still on the
        // network but the current close-group view either (a) caught a
        // transient transport blip or (b) converged on the wrong neighbourhood
        // because the routing table is thin. One retry against a freshly
        // re-walked close group is the cheapest defence against both.
        info!(
            "chunk_get retrying {addr_hex} after reachability failure: \
             queried={} not_found={} timeout={} network_err={} protocol_err={}",
            first.queried, first.not_found, first.timeout, first.network_err, first.protocol_err,
        );

        // Brief settle so any in-flight transport state can quiesce before
        // we re-walk the DHT. Keep this small so we don't add meaningful
        // latency to the genuinely-lost case (we already paid for one full
        // close-group sweep before getting here).
        tokio::time::sleep(Duration::from_secs(1)).await;

        // If the retry's DHT lookup itself fails, treat that as "still
        // couldn't find" rather than escalating the error — matches the
        // semantics of the first attempt when peers are unreachable.
        let retry = match self.chunk_get_try_closest_peers(address, peer_count).await {
            Ok(o) => o,
            Err(e) => {
                info!(
                    "chunk_get retry close-group lookup failed for {addr_hex}: {e}; \
                     first(queried={} not_found={} timeout={} network_err={} protocol_err={})",
                    first.queried,
                    first.not_found,
                    first.timeout,
                    first.network_err,
                    first.protocol_err,
                );
                return Ok(None);
            }
        };
        if let Some(chunk) = retry.chunk {
            info!("chunk_get retry succeeded for {addr_hex}");
            self.chunk_cache().put(chunk.address, chunk.content.clone());
            return Ok(Some(chunk));
        }

        info!(
            "chunk_get exhausted close group after retry for {addr_hex}: \
             first(queried={} not_found={} timeout={} network_err={} protocol_err={}) \
             retry(queried={} not_found={} timeout={} network_err={} protocol_err={})",
            first.queried,
            first.not_found,
            first.timeout,
            first.network_err,
            first.protocol_err,
            retry.queried,
            retry.not_found,
            retry.timeout,
            retry.network_err,
            retry.protocol_err,
        );
        Ok(None)
    }

    /// One sweep of the requested closest peers: fetch the closest peers
    /// for `address` from the DHT and ask each for the chunk in turn,
    /// returning on the first success.
    async fn chunk_get_try_closest_peers(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<CloseGroupOutcome> {
        let peers = self.closest_peers(address, peer_count).await?;
        let addr_hex = hex::encode(address);
        let queried = peers.len();
        let mut not_found = 0usize;
        let mut timeout = 0usize;
        let mut network_err = 0usize;
        let mut protocol_err = 0usize;

        for (peer, addrs) in &peers {
            match self.chunk_get_from_peer(address, peer, addrs).await {
                Ok(Some(chunk)) => {
                    return Ok(CloseGroupOutcome {
                        chunk: Some(chunk),
                        queried,
                        not_found,
                        timeout,
                        network_err,
                        protocol_err,
                    });
                }
                Ok(None) => {
                    not_found += 1;
                    debug!("Chunk {addr_hex} not found on peer {peer}, trying next");
                }
                Err(Error::Timeout(_)) => {
                    timeout += 1;
                    debug!("Peer {peer} timed out for chunk {addr_hex}, trying next");
                }
                Err(Error::Network(_)) => {
                    network_err += 1;
                    debug!("Peer {peer} unreachable for chunk {addr_hex}, trying next");
                }
                // A `Protocol` error here is the storer responding with
                // `ChunkGetResponse::Error(...)` — e.g. "Chunk verification
                // failed" from a peer that has a corrupted local copy.
                // That's a per-peer problem, not a per-chunk one: the
                // remaining peers might still have a clean copy, so
                // continue the sweep rather than aborting it. Counted
                // separately from network_err so the summary log still
                // distinguishes "peer corrupted" from "peer unreachable".
                Err(Error::Protocol(ref e)) => {
                    protocol_err += 1;
                    debug!(
                        "Peer {peer} returned protocol error for chunk {addr_hex} ({e}), trying next"
                    );
                }
                Err(e) => return Err(e),
            }
        }

        Ok(CloseGroupOutcome {
            chunk: None,
            queried,
            not_found,
            timeout,
            network_err,
            protocol_err,
        })
    }

    /// Retrieve a chunk from every peer in the close group.
    ///
    /// Unlike [`Client::chunk_get`], this method does not return early
    /// after the first successful response. It returns one result per
    /// close-group peer, sorted from closest XOR distance to furthest.
    ///
    /// # Errors
    ///
    /// Returns an error if the close-group lookup fails.
    pub async fn chunk_get_from_close_group(
        &self,
        address: &XorName,
    ) -> Result<Vec<ChunkPeerGetResult>> {
        self.chunk_get_from_closest_peer_group(address, self.config().close_group_size)
            .await
    }

    /// Retrieve a chunk from the requested number of closest peers.
    ///
    /// Unlike [`Client::chunk_get_from_closest_peers`], this method does
    /// not return early after the first successful response. It returns
    /// one result per queried peer, sorted from closest XOR distance to
    /// furthest.
    ///
    /// # Errors
    ///
    /// Returns an error if the DHT lookup fails.
    pub async fn chunk_get_from_closest_peer_group(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<Vec<ChunkPeerGetResult>> {
        let peers = self.closest_peers(address, peer_count).await?;
        let targets = chunk_peer_get_targets(peers, address);
        let concurrency_limit =
            diagnostic_peer_get_concurrency(peer_count, self.config().close_group_size);
        let per_peer_timeout = Duration::from_secs(self.config().chunk_get_timeout_secs);
        let overall_timeout =
            diagnostic_peer_get_overall_timeout(per_peer_timeout, targets.len(), concurrency_limit);

        let mut completed = vec![false; targets.len()];
        let mut results = Vec::with_capacity(targets.len());
        let mut get_results = stream::iter(targets.iter().cloned())
            .map(|target| async move {
                let chunk_result = self
                    .chunk_get_from_peer(address, &target.peer_id, &target.peer_addrs)
                    .await;

                if let Ok(Some(chunk)) = &chunk_result {
                    self.chunk_cache().put(chunk.address, chunk.content.clone());
                }

                (
                    target.index,
                    ChunkPeerGetResult {
                        peer_id: target.peer_id,
                        peer_addrs: target.peer_addrs,
                        xor_distance: target.xor_distance,
                        chunk_result,
                    },
                )
            })
            .buffer_unordered(concurrency_limit);

        let collect_results = async {
            while let Some((index, result)) = get_results.next().await {
                completed[index] = true;
                results.push(result);
            }
        };

        if tokio::time::timeout(overall_timeout, collect_results)
            .await
            .is_err()
        {
            for target in &targets {
                if !completed[target.index] {
                    results.push(timed_out_chunk_peer_get_result(
                        target,
                        address,
                        overall_timeout,
                    ));
                }
            }
        }

        sort_chunk_peer_get_results(&mut results);
        Ok(results)
    }

    /// Fetch a chunk from a specific peer.
    async fn chunk_get_from_peer(
        &self,
        address: &XorName,
        peer: &PeerId,
        peer_addrs: &[MultiAddr],
    ) -> Result<Option<DataChunk>> {
        let node = self.network().node();
        let request_id = self.next_request_id();
        let request = ChunkGetRequest::new(*address);
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::GetRequest(request),
        };
        let message_bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("Failed to encode GET request: {e}")))?;

        let timeout = Duration::from_secs(self.config().chunk_get_timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().chunk_get_timeout_secs;

        let result = send_and_await_chunk_response(
            node,
            peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            |body| match body {
                ChunkMessageBody::GetResponse(ChunkGetResponse::Success {
                    address: addr,
                    content,
                }) => {
                    if addr != *address {
                        return Some(Err(Error::InvalidData(format!(
                            "Mismatched chunk address: expected {addr_hex}, got {}",
                            hex::encode(addr)
                        ))));
                    }

                    let computed = compute_address(&content);
                    if computed != addr {
                        return Some(Err(Error::InvalidData(format!(
                            "Invalid chunk content: expected hash {addr_hex}, got {}",
                            hex::encode(computed)
                        ))));
                    }

                    debug!(
                        "Retrieved chunk {} ({} bytes) from peer {peer}",
                        hex::encode(addr),
                        content.len()
                    );
                    Some(Ok(Some(DataChunk::new(addr, Bytes::from(content)))))
                }
                ChunkMessageBody::GetResponse(ChunkGetResponse::NotFound { .. }) => Some(Ok(None)),
                ChunkMessageBody::GetResponse(ChunkGetResponse::Error(e)) => Some(Err(
                    Error::Protocol(format!("Remote GET error for {addr_hex}: {e}")),
                )),
                _ => None,
            },
            |e| Error::Network(format!("Failed to send GET to peer {peer}: {e}")),
            || {
                Error::Timeout(format!(
                    "Timeout waiting for chunk {addr_hex} from {peer} after {timeout_secs}s"
                ))
            },
        )
        .await;

        result
    }

    /// Check if a chunk exists on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_exists(&self, address: &XorName) -> Result<bool> {
        self.chunk_get(address).await.map(|opt| opt.is_some())
    }

    /// Finalize a single-chunk publish after an external signer has paid.
    ///
    /// Single-chunk analogue of [`Client::finalize_upload`]. Takes a
    /// [`PreparedChunk`] (from [`Client::prepare_chunk_payment`]) and a
    /// `quote_hash -> tx_hash` map containing receipts for every non-zero
    /// quote in the chunk's payment. Builds the `PaymentProof` and stores
    /// the chunk on `CLOSE_GROUP_MAJORITY` peers, returning its address.
    ///
    /// Wave-batch payment shape only. Single-chunk publishes don't need
    /// Merkle batching: one chunk's worth of quotes is well below the
    /// wave-batch threshold.
    ///
    /// # Errors
    ///
    /// Returns an error if the proof construction fails (e.g. missing
    /// `tx_hash` for a non-zero quote) or if fewer than
    /// `CLOSE_GROUP_MAJORITY` peers accept the chunk.
    pub async fn finalize_chunk(
        &self,
        prepared: PreparedChunk,
        tx_hash_map: &HashMap<QuoteHash, TxHash>,
    ) -> Result<XorName> {
        let mut paid = finalize_batch_payment(vec![prepared], tx_hash_map)?;
        // finalize_batch_payment returns one PaidChunk per PreparedChunk
        // input; we passed exactly one. If that invariant is ever violated
        // it's an upstream bug — fail loudly rather than silently address-0.
        let chunk = paid.pop().ok_or_else(|| {
            Error::Payment(
                "finalize_batch_payment returned no paid chunks for a single \
                 prepared chunk — internal invariant violated"
                    .into(),
            )
        })?;
        self.chunk_put_to_close_group(chunk.content, chunk.proof_bytes, &chunk.quoted_peers)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ant_protocol::{PROOF_TAG_MERKLE, PROOF_TAG_SINGLE_NODE};

    /// Arbitrary configured Merkle store timeout used by the timeout-selection tests.
    const TEST_MERKLE_TIMEOUT_SECS: u64 = 60;
    /// Sentinel byte used to represent an unknown/unrecognized proof tag.
    const UNKNOWN_PROOF_TAG: u8 = 0xff;
    /// XorName byte width used by test peer IDs and distances.
    const TEST_XORNAME_BYTE_LEN: usize = 32;
    /// Last byte position in the test XOR distance arrays.
    const TEST_DISTANCE_TAIL_INDEX: usize = TEST_XORNAME_BYTE_LEN - 1;

    #[test]
    fn classify_put_failure_maps_remote_timeout_and_dial_reasons() {
        let remote = |source| Error::RemotePut {
            address: "test-addr".to_string(),
            source,
        };
        assert!(matches!(
            classify_put_failure(&remote(ProtocolError::StorageFailed("full".to_string()))),
            PutRejection::Full
        ));
        assert!(matches!(
            classify_put_failure(&remote(ProtocolError::PaymentFailed(
                "below floor".to_string()
            ))),
            PutRejection::PriceFloor
        ));
        assert!(matches!(
            classify_put_failure(&remote(ProtocolError::Internal("boom".to_string()))),
            PutRejection::OtherRemote
        ));
        // A `PaymentRequired` PUT response surfaces as `Error::Payment` and is an
        // application-level decline, not a transport shortfall (ADR-0002).
        assert!(matches!(
            classify_put_failure(&Error::Payment("Payment required: more".to_string())),
            PutRejection::PriceFloor
        ));
        // A PUT-response timeout is genuine local backpressure (V2-554).
        assert!(matches!(
            classify_put_failure(&Error::Timeout("no response".to_string())),
            PutRejection::Timeout
        ));
        // A dial/relay failure (dead/stale relayed address) is remote churn.
        assert!(matches!(
            classify_put_failure(&Error::Network("dial failed".to_string())),
            PutRejection::Dial
        ));
    }

    #[test]
    fn put_shortfall_routes_by_failure_mix() {
        let app = || Error::Payment("Payment required: more".to_string());
        let msg = || "shortfall".to_string();

        // Every failure was an application-level decline (no timeout, no dial):
        // surface the app error so the limiter isn't driven down as a false
        // capacity signal (ADR-0002 / V2-468).
        assert!(matches!(
            put_shortfall_error(0, 0, Some(app()), msg()),
            Error::Payment(_)
        ));
        // Any PUT-response timeout in the mix is genuine local backpressure:
        // keep it a capacity signal so the store limiter still backs off (V2-554).
        assert!(matches!(
            put_shortfall_error(1, 0, Some(app()), msg()),
            Error::InsufficientPeers(_)
        ));
        assert!(matches!(
            put_shortfall_error(1, 3, None, msg()),
            Error::InsufficientPeers(_)
        ));
        // No timeouts but dial/relay churn present: remote peer churn, not local
        // capacity — surface a neutral CloseGroupShortfall (V2-554).
        assert!(matches!(
            put_shortfall_error(0, 2, None, msg()),
            Error::CloseGroupShortfall(_)
        ));
        // Dial churn alongside an app rejection, still no timeout: neutral.
        assert!(matches!(
            put_shortfall_error(0, 1, Some(app()), msg()),
            Error::CloseGroupShortfall(_)
        ));
    }

    fn chunk_peer_get_result(peer_seed: u8, distance_tail: u8) -> ChunkPeerGetResult {
        let mut xor_distance = [0; TEST_XORNAME_BYTE_LEN];
        xor_distance[TEST_DISTANCE_TAIL_INDEX] = distance_tail;

        ChunkPeerGetResult {
            peer_id: PeerId::from_bytes([peer_seed; TEST_XORNAME_BYTE_LEN]),
            peer_addrs: Vec::new(),
            xor_distance,
            chunk_result: Ok(None),
        }
    }

    #[test]
    fn authoritative_not_found_requires_unanimous_well_sampled_response() {
        // Unanimous AND well-sampled: every queried peer of a full
        // close group said NotFound. The only safe stop.
        assert!(is_authoritative_not_found(7, 7));
        // Unanimous with exactly a majority-sized sample is also
        // authoritative.
        assert!(is_authoritative_not_found(
            CLOSE_GROUP_MAJORITY,
            CLOSE_GROUP_MAJORITY
        ));

        // Unanimous but UNDER-sampled: a thin DHT walk returning 1 or 3
        // peers, all NotFound, is NOT authoritative — the real replica
        // majority may sit entirely outside that narrow view. Must
        // retry (re-walk the DHT).
        assert!(!is_authoritative_not_found(1, 1));
        assert!(!is_authoritative_not_found(3, 3));
        assert!(!is_authoritative_not_found(
            CLOSE_GROUP_MAJORITY - 1,
            CLOSE_GROUP_MAJORITY - 1
        ));

        // Not unanimous: 4-of-7 / 6-of-7 NotFound leaves storers in the
        // timeout bucket. Must retry.
        assert!(!is_authoritative_not_found(4, 7));
        assert!(!is_authoritative_not_found(6, 7));

        // Pure-reachability failure — must retry.
        assert!(!is_authoritative_not_found(0, 7));

        // Defensive: a zeroed outcome (e.g. the first attempt's
        // close-group lookup errored) is never authoritative.
        assert!(!is_authoritative_not_found(0, 0));
    }

    #[test]
    fn chunk_get_outcome_classifies_each_result_kind() {
        // Success: chunk_get returned a chunk, regardless of how many
        // internal peer attempts it took.
        let chunk = DataChunk::new([0u8; 32], Bytes::from_static(b"x"));
        assert_eq!(
            chunk_get_outcome(&Ok(Some(chunk))),
            Outcome::Success,
            "found-chunk must be Success",
        );

        // Ok(None): chunk_get exhausted the close group across first
        // attempt + retry. This is the load-shedding signal — count it
        // as Timeout so a sustained run of them on a saturated link
        // shrinks the cap.
        assert_eq!(
            chunk_get_outcome(&Ok(None)),
            Outcome::Timeout,
            "Ok(None) must be Timeout — that's the controller's load-shedding signal",
        );

        // Capacity signals from explicit error variants.
        assert_eq!(
            chunk_get_outcome(&Err(Error::Timeout("t".into()))),
            Outcome::Timeout,
        );
        assert_eq!(
            chunk_get_outcome(&Err(Error::Network("n".into()))),
            Outcome::NetworkError,
        );

        // Unexpected error variant (e.g. Protocol) — propagates out of
        // chunk_get to the caller and is not a capacity signal.
        assert_eq!(
            chunk_get_outcome(&Err(Error::Protocol("p".into()))),
            Outcome::ApplicationError,
        );
    }

    #[test]
    fn single_node_proof_uses_store_response_timeout() {
        let timeout =
            store_response_timeout_for_proof(&[PROOF_TAG_SINGLE_NODE], TEST_MERKLE_TIMEOUT_SECS);

        assert_eq!(timeout, STORE_RESPONSE_TIMEOUT);
    }

    #[test]
    fn unknown_proof_uses_store_response_timeout() {
        let timeout =
            store_response_timeout_for_proof(&[UNKNOWN_PROOF_TAG], TEST_MERKLE_TIMEOUT_SECS);

        assert_eq!(timeout, STORE_RESPONSE_TIMEOUT);
    }

    #[test]
    fn merkle_proof_uses_configured_store_timeout() {
        let timeout =
            store_response_timeout_for_proof(&[PROOF_TAG_MERKLE], TEST_MERKLE_TIMEOUT_SECS);

        assert_eq!(timeout, Duration::from_secs(TEST_MERKLE_TIMEOUT_SECS));
    }

    #[test]
    fn chunk_peer_get_results_sort_by_xor_distance() {
        let mut results = vec![
            chunk_peer_get_result(3, 3),
            chunk_peer_get_result(1, 1),
            chunk_peer_get_result(2, 2),
        ];

        sort_chunk_peer_get_results(&mut results);

        let ordered_distances = results
            .iter()
            .map(|result| result.xor_distance[TEST_DISTANCE_TAIL_INDEX])
            .collect::<Vec<_>>();
        assert_eq!(ordered_distances, vec![1, 2, 3]);
    }

    #[test]
    fn diagnostic_peer_get_overall_timeout_allows_one_wave_plus_padding() {
        const PER_PEER_TIMEOUT_SECS: u64 = 10;
        const EXPECTED_WAVES_WITH_PADDING: u64 = 2;
        const TARGET_COUNT: usize = 7;
        const CONCURRENCY_LIMIT: usize = 7;

        let timeout = diagnostic_peer_get_overall_timeout(
            Duration::from_secs(PER_PEER_TIMEOUT_SECS),
            TARGET_COUNT,
            CONCURRENCY_LIMIT,
        );

        assert_eq!(
            timeout,
            Duration::from_secs(PER_PEER_TIMEOUT_SECS * EXPECTED_WAVES_WITH_PADDING)
        );
    }

    #[test]
    fn diagnostic_peer_get_overall_timeout_scales_with_peer_count() {
        const PER_PEER_TIMEOUT_SECS: u64 = 10;
        const TARGET_COUNT: usize = 20;
        const CLOSE_GROUP_SIZE: usize = 7;
        const EXPECTED_WAVES_WITH_PADDING: u64 = 4;

        let concurrency_limit = diagnostic_peer_get_concurrency(TARGET_COUNT, CLOSE_GROUP_SIZE);
        let timeout = diagnostic_peer_get_overall_timeout(
            Duration::from_secs(PER_PEER_TIMEOUT_SECS),
            TARGET_COUNT,
            concurrency_limit,
        );

        assert_eq!(
            timeout,
            Duration::from_secs(PER_PEER_TIMEOUT_SECS * EXPECTED_WAVES_WITH_PADDING)
        );
    }

    /// Regression: the default `merkle_store_timeout_secs` must be at
    /// least the storer-side `CLOSENESS_LOOKUP_TIMEOUT` (240 s) plus
    /// padding. If either side moves and this invariant breaks, the
    /// client will give up on chunks the storer is still verifying.
    /// See `DEFAULT_MERKLE_STORE_TIMEOUT_SECS` doc comment for the
    /// derivation.
    #[test]
    fn default_merkle_store_timeout_satisfies_storer_invariant() {
        use crate::data::client::ClientConfig;
        const STORER_CLOSENESS_LOOKUP_TIMEOUT_SECS: u64 = 240;
        const MIN_PADDING_SECS: u64 = 30;
        let config = ClientConfig::default();
        assert!(
            config.merkle_store_timeout_secs
                >= STORER_CLOSENESS_LOOKUP_TIMEOUT_SECS + MIN_PADDING_SECS,
            "merkle_store_timeout_secs ({}) must be >= storer CLOSENESS_LOOKUP_TIMEOUT ({}) + padding ({})",
            config.merkle_store_timeout_secs,
            STORER_CLOSENESS_LOOKUP_TIMEOUT_SECS,
            MIN_PADDING_SECS,
        );
    }

    /// Regression: the non-merkle PUT path uses the hardcoded
    /// `STORE_RESPONSE_TIMEOUT` constant, not the per-config
    /// `merkle_store_timeout_secs`. If a future refactor accidentally
    /// routes non-merkle PUTs through the merkle field they'd inherit
    /// the 270 s value and silently regress non-merkle latency.
    /// `store_response_timeout_for_proof` with a non-merkle proof tag
    /// must return the const regardless of what merkle timeout is
    /// passed.
    #[test]
    fn non_merkle_put_ignores_merkle_timeout_value() {
        let absurd_merkle_timeout = 9_999;
        for tag in [PROOF_TAG_SINGLE_NODE, UNKNOWN_PROOF_TAG] {
            let timeout = store_response_timeout_for_proof(&[tag], absurd_merkle_timeout);
            assert_eq!(
                timeout, STORE_RESPONSE_TIMEOUT,
                "non-merkle proof tag {tag:#x} should ignore merkle timeout {absurd_merkle_timeout}",
            );
        }
    }
}
