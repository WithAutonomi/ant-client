//! Chunk storage operations.
//!
//! Chunks are immutable, content-addressed data blocks where the address
//! is the BLAKE3 hash of the content.

#[cfg(feature = "native")]
use crate::data::client::diagnostics::{
    bounded_error, unix_now_ms, DownloadDiagnosticsOutcome, DownloadDiagnosticsRecord,
    DownloadDiagnosticsSender, DownloadRequestCorrelation,
};
#[cfg(feature = "native")]
use crate::data::network::ClosestPeerDiagnostics;
#[cfg(feature = "native")]
use ant_protocol::{
    send_and_await_chunk_response_with_metadata, transport::PeerRouteKind, ChunkProtocolResponse,
};
use std::sync::atomic::AtomicBool;
#[cfg(feature = "native")]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering as AtomicOrdering;
use std::sync::{Mutex, PoisonError};
#[cfg(feature = "native")]
static ACTIVE_DIAGNOSTIC_REQUESTS: AtomicUsize = AtomicUsize::new(0);

#[cfg(feature = "native")]
static NEXT_DIAGNOSTIC_LOOKUP_ID: AtomicUsize = AtomicUsize::new(1);

#[cfg(feature = "native")]
struct ActiveDiagnosticRequestGuard;

#[cfg(feature = "native")]
impl ActiveDiagnosticRequestGuard {
    fn enter() -> (Self, usize) {
        let active = ACTIVE_DIAGNOSTIC_REQUESTS.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        (Self, active)
    }
}

#[cfg(feature = "native")]
impl Drop for ActiveDiagnosticRequestGuard {
    fn drop(&mut self) {
        ACTIVE_DIAGNOSTIC_REQUESTS.fetch_sub(1, AtomicOrdering::Relaxed);
    }
}

#[cfg(feature = "native")]
fn encode_diagnostic_chunk_get_request(
    address: &XorName,
    correlation: &DownloadRequestCorrelation,
) -> Result<Vec<u8>> {
    ChunkMessage {
        request_id: correlation.request_id,
        body: ChunkMessageBody::GetRequest(ChunkGetRequest::new(*address)),
    }
    .encode()
    .map_err(|e| Error::Protocol(format!("Failed to encode GET request: {e}")))
}

#[cfg(feature = "native")]
pub(crate) struct ChunkFetchDiagnostics<'a> {
    sender: &'a DownloadDiagnosticsSender,
    file_attempt: usize,
    chunk_index: usize,
    chunk_address: [u8; 32],
    fetch_cap: usize,
}

#[cfg(feature = "native")]
impl<'a> ChunkFetchDiagnostics<'a> {
    pub(crate) fn new(
        sender: &'a DownloadDiagnosticsSender,
        file_attempt: usize,
        chunk_index: usize,
        chunk_address: [u8; 32],
        fetch_cap: usize,
    ) -> Self {
        Self {
            sender,
            file_attempt,
            chunk_index,
            chunk_address,
            fetch_cap,
        }
    }

    /// Emit a per-peer-attempt record. `lookup_duration_ms` is attached only
    /// for the first peer attempt of the sweep.
    #[allow(clippy::too_many_arguments)]
    fn emit_peer_attempt(
        &self,
        sweep: &'static str,
        peer_attempt: usize,
        lookup_duration_ms: Option<u64>,
        lookup_correlation_id: &str,
        peer_context: &ClosestPeerDiagnostics,
        expected_peer: &PeerId,
        source_peer: Option<&PeerId>,
        transport_source: Option<&MultiAddr>,
        route: PeerRouteKind,
        peer_connected_before_request: bool,
        active_requests_at_start: usize,
        request_started_unix_ms: u64,
        request_completed_unix_ms: u64,
        correlation: &DownloadRequestCorrelation,
        response_elapsed_ms: u64,
        bytes: u64,
        outcome: DownloadDiagnosticsOutcome,
        error: Option<String>,
    ) {
        self.sender
            .try_emit(DownloadDiagnosticsRecord::peer_attempt(
                self.file_attempt,
                self.chunk_index,
                &self.chunk_address,
                sweep,
                peer_attempt,
                lookup_duration_ms,
                lookup_correlation_id,
                &expected_peer.to_string(),
                peer_context
                    .addresses
                    .iter()
                    .map(ToString::to_string)
                    .collect(),
                peer_context.address_types.clone(),
                peer_context.local_last_seen_age_ms,
                peer_context.publisher_address_set_age_ms,
                peer_context.publisher_address_set_unix_ns,
                source_peer.map(ToString::to_string).as_deref(),
                transport_source.map(ToString::to_string).as_deref(),
                route.as_str(),
                (route == PeerRouteKind::Unknown)
                    .then_some(DownloadDiagnosticsRecord::ROUTE_UNKNOWN_NOTE),
                Some(peer_connected_before_request),
                Some(active_requests_at_start),
                Some(self.fetch_cap),
                request_started_unix_ms,
                request_completed_unix_ms,
                correlation,
                response_elapsed_ms,
                bytes,
                outcome,
                error,
            ));
    }

    /// Emit a chunk-level record (cache hit, lookup error, or exhausted).
    fn emit_chunk_level(
        &self,
        sweep: &'static str,
        bytes: u64,
        outcome: DownloadDiagnosticsOutcome,
        error: Option<String>,
    ) {
        self.sender.try_emit(DownloadDiagnosticsRecord::chunk_level(
            self.file_attempt,
            self.chunk_index,
            &self.chunk_address,
            sweep,
            Some(self.fetch_cap),
            bytes,
            outcome,
            error,
        ));
    }
}

#[cfg(feature = "native")]
fn classify_peer_attempt(
    result: &Result<Option<DataChunk>>,
) -> (DownloadDiagnosticsOutcome, u64, bool, Option<String>) {
    match result {
        Ok(Some(chunk)) => (
            DownloadDiagnosticsOutcome::Found,
            chunk.content.len() as u64,
            true,
            None,
        ),
        Ok(None) => (DownloadDiagnosticsOutcome::NotFound, 0, true, None),
        Err(Error::Timeout(msg)) => (
            DownloadDiagnosticsOutcome::Timeout,
            0,
            false,
            Some(bounded_error("timeout", msg)),
        ),
        Err(Error::Network(msg)) => (
            DownloadDiagnosticsOutcome::NetworkError,
            0,
            false,
            Some(bounded_error("network", msg)),
        ),
        // Invalid data can only be constructed after a response body was
        // received and validated, so attributing the matched peer is sound.
        Err(Error::InvalidData(msg)) => (
            DownloadDiagnosticsOutcome::ProtocolError,
            0,
            true,
            Some(bounded_error("protocol", msg)),
        ),
        // `Protocol` includes both a remote GET error and a local request
        // encoding failure. Without a distinct provenance bit, conservatively
        // avoid claiming a response peer for either case.
        Err(Error::Protocol(msg)) => (
            DownloadDiagnosticsOutcome::ProtocolError,
            0,
            false,
            Some(bounded_error("protocol", msg)),
        ),
        Err(e) => (
            DownloadDiagnosticsOutcome::ProtocolError,
            0,
            false,
            Some(bounded_error("protocol", &e.to_string())),
        ),
    }
}
use crate::client_engine::single_flight::Flown;
use crate::data::client::adaptive::Outcome;
use crate::data::client::batch::{finalize_batch_payment, PreparedChunk};
use crate::data::client::peer_xor_distance;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use crate::data::network::{send_and_await_chunk_response, ChunkRequest, RequestDispatched};
use ant_protocol::evm::{QuoteHash, TxHash};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{
    compute_address, detect_proof_type, ChunkGetRequest, ChunkGetResponse, ChunkMessage,
    ChunkMessageBody, ChunkPutRequest, ChunkPutResponse, DataChunk, ProofType, ProtocolError,
    XorName, CLOSE_GROUP_MAJORITY,
};
use bytes::Bytes;
use futures::future::{select, Either};
use futures::stream::{self, StreamExt};
use std::collections::HashMap;
use tracing::{debug, info, warn};
use web_time::{Duration, Instant};

/// Data type identifier for chunks (used in quote requests).
const CHUNK_DATA_TYPE: u32 = 0;

use crate::transfer_policy::{PutRejection, PutShortfall};

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
    match crate::transfer_policy::put_shortfall(timeout, dial, first_app_rejection.is_some()) {
        PutShortfall::ResponseTimeout => Error::InsufficientPeers(shortfall_message),
        PutShortfall::RemoteRejection => {
            first_app_rejection.unwrap_or(Error::CloseGroupShortfall(shortfall_message))
        }
        PutShortfall::PeerChurn => Error::CloseGroupShortfall(shortfall_message),
    }
}

#[cfg(test)]
use crate::client_engine::read::is_authoritative_not_found;

/// Store-response timeout for a PUT carrying a single-node payment proof.
///
/// The merkle timeout is far longer only because a merkle pool makes the
/// storer do an authoritative network closeness lookup first. A PUT that
/// carries a single-node proof does no such lookup, so it is held to this.
pub(crate) const STORE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

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

/// What one read did with the swarm.
#[derive(Default)]
struct SwarmRead {
    /// Another client sent the chunk.
    from_peer: AtomicBool,
    /// When the read turned to nodes, after asking peers.
    nodes_from: Mutex<Option<Instant>>,
    /// A GET of this read has left for a node: from then on its record is
    /// the node's to send, and no peer is asked for it.
    dispatched: AtomicBool,
    /// How the node read went and how long it took, when it ended without
    /// the record while a linked reader was still sending it: the nodes'
    /// miss is theirs, for the node limiter, whatever the reader then did.
    nodes_missed: Mutex<Option<(Outcome, Duration)>>,
}

/// What the nodes did for a read, for the node limiter: how long they took,
/// and, when they ended without the record while a linked reader was still
/// sending it, how their read went.
#[derive(Clone, Copy, Debug)]
// Read by the browser's read-ahead only; a native read observes in place.
#[cfg_attr(feature = "native", allow(dead_code))]
pub(crate) struct NodeTime {
    pub(crate) latency: Duration,
    pub(crate) missed: Option<Outcome>,
}

/// How often a read still waiting for its turn at the nodes looks whether a
/// linked reader has its record.
#[cfg(not(feature = "native"))]
const QUEUED_PEER_POLL: Duration = Duration::from_millis(250);

/// Where a read's record came from.
#[derive(Debug, PartialEq)]
enum Source<T> {
    /// A linked reader, checked.
    Peer(Bytes),
    /// The nodes, as their read returned it.
    Nodes(T),
}

/// How often a read whose reader is past its head start looks whether the
/// reader keeps on pace.
const PACE_POLL: Duration = Duration::from_millis(100);

/// Reads a record from linked readers beside the nodes, never instead of
/// them. `ask` asks the readers that offer it (`None` at once if none does),
/// telling `pace` how far the record has come; `node_read` reads it from the
/// nodes. A reader that has the record when the read starts gets
/// `head_start` (the read's hedge delay, the time a node gets before another
/// is asked) to send it. Past that, it keeps the read while, at its average
/// rate so far, the whole record comes within `patience` of the ask (about
/// as long as the nodes take to send a record now), and for `patience` at
/// most; then, or as soon as it falls off that pace or fails, the node read
/// starts beside it, and the first to bring the record wins. A node read
/// already waiting for its turn keeps its place while a reader that offers
/// the record meanwhile (`offered`) sends it. A slow or failing reader so
/// delays a read by `patience` at most, and by `head_start` unless it was on
/// pace to beat a node read started then; it never costs a node read its
/// place. A node read that ends without the record (`has_record` says
/// whether its answer is one, and may note one that is not) leaves a reader
/// still sending to finish, as its own deadline allows. `to_nodes` marks
/// when the read turned to the nodes.
async fn beside_peers<T, A, AF, N, NF>(
    ask: A,
    pace: &crate::swarm::Pace,
    node_read: N,
    (head_start, patience): (Duration, Duration),
    offered: impl std::future::Future<Output = ()>,
    to_nodes: impl Fn(),
    has_record: impl Fn(&Result<T>) -> bool,
) -> Result<Source<T>>
where
    A: Fn() -> AF,
    AF: std::future::Future<Output = Option<Bytes>>,
    N: Fn() -> NF,
    NF: std::future::Future<Output = Result<T>>,
{
    let longest = patience.max(head_start);
    // The reader's whole window runs from the ask on the timers' own clock,
    // so a timer that fires late on a busy or throttled page cannot stretch
    // it, and the pace is judged on the time that really passed.
    let asked_at = crate::runtime::Clock::now();
    let deadline = asked_at + longest;
    let asked = Box::pin(ask());
    let head = Box::pin(crate::runtime::sleep(head_start));
    match select(asked, head).await {
        Either::Left((Some(record), _)) => return Ok(Source::Peer(record)),
        // No reader had it, or the one asked failed: the nodes, while a
        // reader that offers it later may still send it.
        Either::Left((None, _)) => to_nodes(),
        Either::Right(((), asked)) => {
            let paced = Box::pin(async {
                loop {
                    let now = crate::runtime::Clock::now();
                    let left = deadline.saturating_duration_since(now);
                    if left.is_zero() || !pace.finishes_by(now.duration_since(asked_at), longest) {
                        return;
                    }
                    crate::runtime::sleep(PACE_POLL.min(left)).await;
                }
            });
            let asked = match select(asked, paced).await {
                Either::Left((Some(record), _)) => return Ok(Source::Peer(record)),
                Either::Left((None, _)) => {
                    to_nodes();
                    return Box::pin(node_read()).await.map(Source::Nodes);
                }
                Either::Right(((), asked)) => asked,
            };
            to_nodes();
            return match select(asked, Box::pin(node_read())).await {
                Either::Left((Some(record), _)) => Ok(Source::Peer(record)),
                Either::Left((None, nodes)) => nodes.await.map(Source::Nodes),
                Either::Right((result, asked)) => after_nodes(result, asked, &has_record).await,
            };
        }
    }
    let nodes = Box::pin(node_read());
    match select(nodes, Box::pin(offered)).await {
        Either::Left((result, _)) => result.map(Source::Nodes),
        Either::Right(((), nodes)) => match select(Box::pin(ask()), nodes).await {
            Either::Left((Some(record), _)) => Ok(Source::Peer(record)),
            Either::Left((None, nodes)) => nodes.await.map(Source::Nodes),
            Either::Right((result, asked)) => after_nodes(result, asked, &has_record).await,
        },
    }
}

/// The node read ended before the reader sending the record: its record if
/// it brought one; otherwise (it failed, or found none) the reader's, if it
/// still sends one within its own deadline, and only then the node read's
/// answer. Busy or refusing nodes so never cancel a reader that has most of
/// the record.
async fn after_nodes<T>(
    result: Result<T>,
    asked: impl std::future::Future<Output = Option<Bytes>>,
    has_record: impl Fn(&Result<T>) -> bool,
) -> Result<Source<T>> {
    if has_record(&result) {
        return result.map(Source::Nodes);
    }
    match asked.await {
        Some(record) => Ok(Source::Peer(record)),
        None => result.map(Source::Nodes),
    }
}

/// `dispatched`, for a GET whose dispatch `read`, if any, learns of: the
/// notice the GET gets, and the future that passes it on, which the GET's
/// own future runs beside.
fn watch_dispatch<'a>(
    dispatched: RequestDispatched,
    read: Option<&'a SwarmRead>,
) -> (
    RequestDispatched,
    impl std::future::Future<Output = ()> + 'a,
) {
    let (notice, sent) = RequestDispatched::channel();
    let forward = async move {
        if sent.await.is_ok() {
            if let Some(read) = read {
                read.dispatched.store(true, AtomicOrdering::Release);
            }
            dispatched.notify();
        }
    };
    (notice, forward)
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
        self.chunk_get_observed_from_closest_peers(
            address,
            self.config().close_group_size,
            true,
            #[cfg(feature = "native")]
            None,
        )
        .await
    }

    /// As [`Self::chunk_get_observed`], for a record that is not one of a
    /// file's data records (a DataMap wrapper, say): only nodes are asked,
    /// and the record is not offered to any room. A room lists its file's
    /// data records alone, and a map record shared through one would tell
    /// that room's members which other file this client is opening.
    pub(crate) async fn chunk_get_observed_from_nodes(
        &self,
        address: &XorName,
    ) -> Result<Option<DataChunk>> {
        self.chunk_get_observed_from_closest_peers(
            address,
            self.config().close_group_size,
            false,
            #[cfg(feature = "native")]
            None,
        )
        .await
    }

    /// With `swarm`, a read of a file's data record: other clients reading the
    /// file are asked first, and what this fetch gets is offered to them.
    pub(crate) async fn chunk_get_observed_from_closest_peers(
        &self,
        address: &XorName,
        peer_count: usize,
        swarm: bool,
        #[cfg(feature = "native")] diag: Option<&ChunkFetchDiagnostics<'_>>,
    ) -> Result<Option<DataChunk>> {
        // Diagnostics record the GETs their own caller sent, so a diagnosed
        // fetch neither shares another caller's GET nor lends its own.
        #[cfg(feature = "native")]
        if let Some(diag) = diag {
            let epoch = self.controller().fetch.observation_epoch();
            let started = Instant::now();
            let result = self
                .chunk_get_from_closest_peers_with_diagnostics(
                    address,
                    peer_count,
                    None,
                    swarm,
                    Some(diag),
                )
                .await;
            self.observe_chunk_get(&result, started.elapsed(), epoch);
            return result;
        }
        self.chunk_get_shared(address, peer_count, true, swarm)
            .await
    }

    /// Fetch a chunk, or wait for this client's fetch of the same address and
    /// peer count if one is already in flight, so concurrent reads of a chunk
    /// send its GETs once. A shared fetch feeds the adaptive fetch limiter
    /// once, if any caller sharing it observes. With `swarm`, other clients
    /// reading the same file are asked before nodes.
    async fn chunk_get_shared(
        &self,
        address: &XorName,
        peer_count: usize,
        observe: bool,
        swarm: bool,
    ) -> Result<Option<DataChunk>> {
        self.chunk_get_sourced(address, peer_count, observe, swarm)
            .await
            .0
    }

    /// Fetch a chunk browser read-ahead holds for a read: as `chunk_get`
    /// does, with how long nodes took if this call asked them. Read-ahead
    /// tells the node limiter only about those.
    #[cfg(not(feature = "native"))]
    pub(crate) async fn chunk_get_for_read_ahead(
        &self,
        address: &XorName,
    ) -> (Result<Option<DataChunk>>, Option<NodeTime>) {
        self.chunk_get_sourced(address, self.config().close_group_size, false, true)
            .await
    }

    /// [`Self::chunk_get_shared`], with what this call's fetch from nodes (or
    /// its cache) did: `None` if another caller fetched it, or another client
    /// sent the chunk before the nodes had missed it. Time spent asking peers
    /// first is not the nodes'.
    async fn chunk_get_sourced(
        &self,
        address: &XorName,
        peer_count: usize,
        observe: bool,
        swarm: bool,
    ) -> (Result<Option<DataChunk>>, Option<NodeTime>) {
        let mut started = None;
        let read = SwarmRead::default();
        let flown = self
            .chunk_gets
            .run(
                (*address, peer_count, swarm),
                observe,
                || {
                    started = Some((self.controller().fetch.observation_epoch(), Instant::now()));
                    self.chunk_get_from_closest_peers_with_diagnostics(
                        address,
                        peer_count,
                        swarm.then_some(&read),
                        swarm,
                        #[cfg(feature = "native")]
                        None,
                    )
                },
                |result| match result {
                    Ok(chunk) => Ok(chunk.clone()),
                    Err(error) => Err(error.duplicate()),
                },
            )
            .await;
        match flown {
            Flown::Led { value, observe } => {
                // The nodes ended without the record while a linked reader
                // was still sending it: their miss, and their time to it, are
                // theirs, whether the reader then sent it or failed too.
                let missed = *read
                    .nodes_missed
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                if let Some((outcome, latency)) = missed {
                    if let (true, Some((epoch, _))) = (observe, started) {
                        self.controller()
                            .fetch
                            .observe_fetch_in_epoch(outcome, latency, 0, epoch);
                    }
                    return (
                        value,
                        Some(NodeTime {
                            latency,
                            missed: Some(outcome),
                        }),
                    );
                }
                // A record another client sent says nothing about how the
                // nodes are coping, so it does not move the node limiter.
                if read.from_peer.load(AtomicOrdering::Relaxed) {
                    return (value, None);
                }
                let Some((epoch, started)) = started else {
                    return (value, None);
                };
                let nodes_from = *read
                    .nodes_from
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner);
                let latency = nodes_from.unwrap_or(started).elapsed();
                if observe {
                    self.observe_chunk_get(&value, latency, epoch);
                }
                // How long the nodes take now sets how long a peer on pace
                // may keep a read: reads someone waits for that turned to the
                // nodes count, as for the limiter. Read-ahead nobody waits on
                // queues behind a speculative share, which says nothing of
                // the nodes.
                if observe && nodes_from.is_some() && matches!(value, Ok(Some(_))) {
                    self.swarm().note_node_read(latency);
                }
                (
                    value,
                    Some(NodeTime {
                        latency,
                        missed: None,
                    }),
                )
            }
            Flown::Joined(value) => (value, None),
        }
    }

    /// Feed one `chunk_get` outcome to the adaptive fetch limiter, including
    /// one another client fetched on this client's behalf. `epoch` is the
    /// limiter's observation epoch when the fetch started.
    pub(crate) fn observe_chunk_get(
        &self,
        result: &Result<Option<DataChunk>>,
        latency: Duration,
        epoch: u64,
    ) {
        let bytes = result
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .map_or(0, |chunk| chunk.content.len() as u64);
        self.controller().fetch.observe_fetch_in_epoch(
            chunk_get_outcome(result),
            latency,
            bytes,
            epoch,
        );
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
        let success_count = outcome.successful_targets.len();
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
        let node = self.network();
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
            ChunkRequest {
                target_peer,
                peer_addrs,
                message_bytes,
                request_id,
                timeout,
                dispatched: RequestDispatched::unobserved(),
            },
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
    /// DHT routing. Concurrent calls for one address share a single fetch.
    ///
    /// ## Adaptive controller feedback
    ///
    /// Download workflows use `chunk_get_observed` to feed the adaptive fetch
    /// limiter once per completed retrieval, including exhausted close groups.
    /// Internal peer failures are handled by the shared read policy.
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
    /// Concurrent calls for the same address and peer count share one
    /// fetch: later callers wait for the first caller's result, including
    /// its error, instead of sending their own GETs.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_get_from_closest_peers(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<Option<DataChunk>> {
        self.chunk_get_shared(address, peer_count, false, false)
            .await
    }

    /// Fetch a chunk from nodes, or this client's cache, never from another
    /// client. With `swarm`, other clients reading the same file are asked
    /// first, and `swarm` records whether one of them sent the chunk or when
    /// the read turned to nodes. With `shares`, what the fetch gets is
    /// offered to the rooms whose file lists it.
    async fn chunk_get_from_closest_peers_with_diagnostics(
        &self,
        address: &XorName,
        peer_count: usize,
        swarm: Option<&SwarmRead>,
        // Only a read of a file's data record shares what it gets with the
        // file's room: one asking peers, or a diagnosed one (which asks only
        // nodes, to measure them). A raw read, an existence check or a map
        // record keeps it to itself.
        shares: bool,
        #[cfg(feature = "native")] diag: Option<&ChunkFetchDiagnostics<'_>>,
    ) -> Result<Option<DataChunk>> {
        // Check cache first, with integrity verification.
        if let Some(cached) = self.chunk_cache().get(address) {
            if crate::record::verify(address, &cached).is_ok() {
                debug!("Cache hit for chunk {}", hex::encode(address));
                #[cfg(feature = "native")]
                if let Some(diag) = diag {
                    diag.emit_chunk_level(
                        "initial",
                        cached.len() as u64,
                        DownloadDiagnosticsOutcome::CacheHit,
                        None,
                    );
                }
                // Read before its room was joined, a cached record is still
                // offered to the other readers.
                if shares {
                    self.swarm().seed(*address, cached.clone());
                }
                return Ok(Some(DataChunk::new(*address, cached)));
            }
            // Cache entry corrupted — evict and fall through to network fetch.
            debug!(
                "Cache corruption detected for {}: evicting",
                hex::encode(address)
            );
            self.chunk_cache().remove(address);
        }

        // A file's read, whether it asks peers or only nodes, counts while it
        // runs: the client then sends other readers less, and the file's
        // room stays joined however long the nodes take.
        let _reading = shares.then(|| self.swarm().reading(address));
        #[cfg(feature = "native")]
        let observation = diag.map(|_| std::sync::Mutex::new(ReadObservation::default()));
        let node_read = || {
            crate::client_engine::read::retrieve_progressive(
                *address,
                // Use the same bounded fallback allowance while discovery is in
                // flight. Stopping after just the close-group count can exhaust
                // speculation on stale hints before a live holder is discovered.
                // The final close-group size and absence rules still use peer_count.
                crate::client_engine::read::EarlyReads {
                    limit: crate::client_engine::read::MAX_GET_FALLBACK_PEERS,
                    hedge_delay: self.network().read_hedge_delay(),
                },
                crate::client_engine::read::uniform_choice,
                |sender| {
                    #[cfg(feature = "native")]
                    let observation = &observation;
                    async move {
                        let progress = crate::data::network::ReadProgress::new(
                            *address,
                            *self.network().peer_id(),
                            sender,
                        );
                        self.network().seed_read_candidates(&progress).await;
                        #[cfg(feature = "native")]
                        let lookup_started = Instant::now();
                        #[cfg(feature = "native")]
                        let mut contexts = Vec::new();
                        #[cfg(feature = "native")]
                        let closest_result = if diag.is_some() {
                            self.network()
                                .find_closest_peers_with_diagnostics(address, peer_count)
                                .await
                                .map(|found| {
                                    let peers = found
                                        .iter()
                                        .map(|c| (c.peer_id, c.addresses.clone()))
                                        .collect();
                                    contexts = found;
                                    peers
                                })
                        } else {
                            self.closest_peers(address, peer_count).await
                        };
                        #[cfg(not(feature = "native"))]
                        let closest_result = self
                            .network()
                            .find_read_peers(address, peer_count, progress)
                            .await;
                        let closest = closest_result.unwrap_or_else(|e| {
                            #[cfg(feature = "native")]
                            if let (Some(diag), Some(observation)) = (diag, &observation) {
                                let round =
                                    observation.lock().unwrap_or_else(|e| e.into_inner()).round;
                                diag.emit_chunk_level(
                                    if round == 0 { "initial" } else { "retry" },
                                    0,
                                    DownloadDiagnosticsOutcome::LookupError,
                                    Some(bounded_error("lookup", &e.to_string())),
                                );
                            }
                            info!(
                                "Chunk discovery failed for {}: {e}; trying known peers",
                                hex::encode(address)
                            );
                            Vec::new()
                        });
                        let known = self
                            .network()
                            .known_peers()
                            .await
                            .into_iter()
                            .filter(|node| node.peer_id != *self.network().peer_id())
                            .map(|node| {
                                let addrs = node.addresses_by_priority();
                                (node.peer_id, addrs)
                            })
                            .collect();
                        #[cfg(feature = "native")]
                        if let (Some(diag), Some(observation)) = (diag, &observation) {
                            let mut state = observation.lock().unwrap_or_else(|e| e.into_inner());
                            state.round += 1;
                            state.peer_attempt = 0;
                            state.lookup_ms = u64::try_from(lookup_started.elapsed().as_millis())
                                .unwrap_or(u64::MAX);
                            state.lookup_id = format!(
                                "{}-{}-{}-{}",
                                diag.file_attempt,
                                diag.chunk_index,
                                hex::encode(address),
                                NEXT_DIAGNOSTIC_LOOKUP_ID.fetch_add(1, AtomicOrdering::Relaxed)
                            );
                            state.contexts = contexts.into_iter().map(|c| (c.peer_id, c)).collect();
                        }
                        crate::client_engine::read::ReadCandidates { closest, known }
                    }
                },
                |(peer, _)| *peer.as_bytes(),
                |(peer, addrs), early, dispatched| {
                    #[cfg(feature = "native")]
                    let observation = &observation;
                    let (dispatched, forward) = watch_dispatch(dispatched, swarm);
                    let get = async move {
                        if early {
                            #[cfg(feature = "native")]
                            if let Some(diag) = diag {
                                let early_observation = std::sync::Mutex::new(ReadObservation {
                                    lookup_id: format!(
                                        "{}-{}-early-{}",
                                        diag.file_attempt,
                                        diag.chunk_index,
                                        NEXT_DIAGNOSTIC_LOOKUP_ID
                                            .fetch_add(1, AtomicOrdering::Relaxed)
                                    ),
                                    ..ReadObservation::default()
                                });
                                // A native GET is dispatched at once.
                                dispatched.notify();
                                return self
                                    .chunk_get_diagnostic_attempt(
                                        address,
                                        &peer,
                                        &addrs,
                                        diag,
                                        &early_observation,
                                    )
                                    .await;
                            }
                        }
                        #[cfg(feature = "native")]
                        if let (Some(diag), Some(observation)) = (diag, observation) {
                            dispatched.notify();
                            return self
                                .chunk_get_diagnostic_attempt(
                                    address,
                                    &peer,
                                    &addrs,
                                    diag,
                                    observation,
                                )
                                .await;
                        }
                        self.chunk_get_from_peer(address, &peer, &addrs, dispatched)
                            .await
                    };
                    async move { futures::future::join(get, forward).await.0 }
                },
                |error| {
                    matches!(
                        error,
                        Error::Timeout(_) | Error::Network(_) | Error::Protocol(_)
                    )
                },
                || {
                    self.network()
                        .refused_peers()
                        .into_iter()
                        .map(|(peer, remaining)| (*peer.as_bytes(), remaining))
                        .collect()
                },
                || {
                    Error::Network(format!(
                        "the nodes that may hold chunk {} are busy; try again shortly",
                        hex::encode(address)
                    ))
                },
                crate::runtime::sleep,
            )
        };
        // Boxed, so the read's future stays small for the many callers that
        // nest it.
        let result = match swarm {
            Some(read) => {
                match Box::pin(self.chunk_get_beside_peers(address, read, node_read)).await? {
                    // Checked, and kept for the room by the swarm itself; it
                    // stays out of the chunk cache, which answers existence
                    // checks: another client holding a record says nothing
                    // about whether the network stores it.
                    Source::Peer(content) => return Ok(Some(DataChunk::new(*address, content))),
                    Source::Nodes(result) => result,
                }
            }
            None => Box::pin(node_read()).await?,
        };
        #[cfg(feature = "native")]
        if result.is_none() {
            if let (Some(diag), Some(observation)) = (diag, &observation) {
                let round = observation.lock().unwrap_or_else(|e| e.into_inner()).round;
                diag.emit_chunk_level(
                    if round == 1 { "initial" } else { "retry" },
                    0,
                    DownloadDiagnosticsOutcome::Exhausted,
                    None,
                );
            }
        }
        if let Some(chunk) = &result {
            self.chunk_cache().put(chunk.address, chunk.content.clone());
            if shares {
                self.swarm().seed(chunk.address, chunk.content.clone());
            }
        }
        Ok(result)
    }

    /// Reads `address` from the linked readers that offer it beside the
    /// nodes, never instead of them (see [`beside_peers`]), and returns the
    /// first record: a reader's checked against its address, or the nodes'
    /// read as it returned.
    async fn chunk_get_beside_peers<F, R>(
        &self,
        address: &XorName,
        read: &SwarmRead,
        node_read: F,
    ) -> Result<Source<Option<DataChunk>>>
    where
        F: Fn() -> R,
        R: std::future::Future<Output = Result<Option<DataChunk>>>,
    {
        let to_nodes = || {
            *read
                .nodes_from
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
        };
        // Most of a browser download's reads wait for their turn at the
        // nodes, and its links open only after they all began: a read with
        // no GET sent yet looks whether a linked reader has its record
        // meanwhile. A native read sends at once.
        #[cfg(feature = "native")]
        let offered = futures::future::pending::<()>();
        #[cfg(not(feature = "native"))]
        let offered = async {
            loop {
                crate::runtime::sleep(QUEUED_PEER_POLL).await;
                if read.dispatched.load(AtomicOrdering::Acquire) {
                    return futures::future::pending::<()>().await;
                }
                if self.swarm().could_serve(address) {
                    return;
                }
            }
        };
        let pace = crate::swarm::Pace::default();
        let hedge = self.network().read_hedge_delay();
        let source = beside_peers(
            || self.swarm().fetch_paced(address, &pace),
            &pace,
            node_read,
            (hedge, self.swarm().patience(hedge.saturating_mul(2))),
            offered,
            to_nodes,
            |result: &Result<Option<DataChunk>>| {
                let found = matches!(result, Ok(Some(_)));
                // A reader still sending may bring the record yet; how the
                // nodes did is kept for the limiter all the same.
                if !found {
                    let since = *read
                        .nodes_from
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner);
                    if let Some(since) = since {
                        *read
                            .nodes_missed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) =
                            Some((chunk_get_outcome(result), since.elapsed()));
                    }
                }
                found
            },
        )
        .await?;
        if matches!(source, Source::Peer(_)) {
            debug!("Chunk {} came from another client", hex::encode(address));
            read.from_peer.store(true, AtomicOrdering::Relaxed);
        }
        Ok(source)
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
                    .chunk_get_from_peer(
                        address,
                        &target.peer_id,
                        &target.peer_addrs,
                        RequestDispatched::unobserved(),
                    )
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

        if crate::runtime::timeout(overall_timeout, collect_results)
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

    /// Fetch a chunk from a specific peer, notifying `dispatched` once the GET
    /// waits only on that peer.
    async fn chunk_get_from_peer(
        &self,
        address: &XorName,
        peer: &PeerId,
        peer_addrs: &[MultiAddr],
        dispatched: RequestDispatched,
    ) -> Result<Option<DataChunk>> {
        let node = self.network();
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
            ChunkRequest {
                target_peer: peer,
                peer_addrs,
                message_bytes,
                request_id,
                timeout,
                dispatched,
            },
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

                    if let Err(error) = crate::record::verify(&addr, &content) {
                        return Some(Err(Error::InvalidData(error)));
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
    /// Only nodes answer: another client holding a record says nothing about
    /// whether the network stores it.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_exists(&self, address: &XorName) -> Result<bool> {
        self.chunk_get_shared(address, self.config().close_group_size, false, false)
            .await
            .map(|opt| opt.is_some())
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

    /// What `beside_peers` did: where the record came from, how long it took,
    /// how many node reads it started and when the read turned to nodes.
    struct Raced {
        source: Source<&'static str>,
        took: Duration,
        node_reads: usize,
        turned_at: Option<Duration>,
        /// Whether the race was told of a node read that ended without the
        /// record.
        node_missed: bool,
    }

    /// A reader in a race: its pieces arrive evenly, so that the record would
    /// be whole at `whole`; it stops sending at `stops` if that comes first
    /// and then never answers; otherwise it answers at `whole` with `record`
    /// (`None`: it fails then, having sent nothing).
    #[derive(Clone, Copy)]
    struct Peer {
        whole: Duration,
        stops: Duration,
        record: Option<&'static str>,
    }

    impl Peer {
        fn sends(whole: Duration, record: &'static str) -> Self {
            Self {
                whole,
                stops: NEVER,
                record: Some(record),
            }
        }

        fn fails(after: Duration) -> Self {
            Self {
                whole: after,
                stops: Duration::ZERO,
                record: None,
            }
        }

        fn stalls() -> Self {
            Self {
                whole: NEVER,
                stops: Duration::ZERO,
                record: None,
            }
        }
    }

    const RECORD_BYTES: u64 = 4_000_000;

    /// Races `peer` (`None`: no reader has the record) against node reads
    /// that take `node` each, with a reader offering the record to a waiting
    /// read after `offer`, if given. The reader asked a second time is
    /// `second`.
    async fn race(
        peer: Option<Peer>,
        second: Option<Peer>,
        node: Duration,
        offer: Option<Duration>,
    ) -> Raced {
        race_with_patience(peer, second, node, offer, HEAD_START * 2).await
    }

    /// [`race`], with a reader on pace kept for `patience` at most.
    async fn race_with_patience(
        peer: Option<Peer>,
        second: Option<Peer>,
        node: Duration,
        offer: Option<Duration>,
        patience: Duration,
    ) -> Raced {
        race_against_nodes(peer, second, (node, true), offer, patience).await
    }

    /// [`race_with_patience`], with node reads that each end after `node`
    /// with the record, or failing if `nodes_have` is false.
    async fn race_against_nodes(
        peer: Option<Peer>,
        second: Option<Peer>,
        (node, nodes_have): (Duration, bool),
        offer: Option<Duration>,
        patience: Duration,
    ) -> Raced {
        let started = tokio::time::Instant::now();
        let asks = std::cell::Cell::new(0usize);
        let node_reads = std::cell::Cell::new(0usize);
        let turned_at = std::cell::Cell::new(None);
        let node_missed = std::cell::Cell::new(false);
        let pace = crate::swarm::Pace::default();
        let source = beside_peers(
            || {
                let answer = if asks.get() == 0 { peer } else { second };
                asks.set(asks.get() + 1);
                let pace = &pace;
                async move {
                    let peer = answer?;
                    pace.note(0, 0);
                    let step = Duration::from_millis(50);
                    let mut at = Duration::ZERO;
                    while at < peer.whole {
                        if at >= peer.stops {
                            if peer.record.is_some() {
                                futures::future::pending::<()>().await;
                            }
                            tokio::time::sleep(peer.whole - at).await;
                            return None;
                        }
                        let next = (at + step).min(peer.whole).min(peer.stops.max(at + step));
                        tokio::time::sleep(next - at).await;
                        at = next;
                        let sent = u128::from(RECORD_BYTES) * at.as_millis()
                            / peer.whole.as_millis().max(1);
                        pace.note(
                            u64::try_from(sent)
                                .unwrap_or(RECORD_BYTES)
                                .min(RECORD_BYTES),
                            RECORD_BYTES,
                        );
                    }
                    peer.record
                        .map(|record| Bytes::from_static(record.as_bytes()))
                }
            },
            &pace,
            || {
                node_reads.set(node_reads.get() + 1);
                async move {
                    tokio::time::sleep(node).await;
                    if nodes_have {
                        Ok("from nodes")
                    } else {
                        Err(Error::Network("the nodes are busy".into()))
                    }
                }
            },
            (HEAD_START, patience),
            async move {
                match offer {
                    Some(after) => tokio::time::sleep(after).await,
                    None => futures::future::pending::<()>().await,
                }
            },
            || turned_at.set(Some(started.elapsed())),
            |result| {
                let found = result.is_ok();
                if !found {
                    node_missed.set(true);
                }
                found
            },
        )
        .await
        .unwrap_or_else(|error| panic!("the race failed: {error}"));
        Raced {
            source,
            took: started.elapsed(),
            node_reads: node_reads.get(),
            turned_at: turned_at.get(),
            node_missed: node_missed.get(),
        }
    }

    const HEAD_START: Duration = Duration::from_secs(3);
    const NODE_READ: Duration = Duration::from_secs(1);
    /// Longer than any read here: a reader that never finishes in time.
    const NEVER: Duration = Duration::from_secs(3600);

    fn from_a_reader() -> Source<&'static str> {
        Source::Peer(Bytes::from_static(b"from a reader"))
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_has_the_record_spares_the_nodes() {
        let quick = Duration::from_millis(400);
        let raced = race(
            Some(Peer::sends(quick, "from a reader")),
            None,
            NODE_READ,
            None,
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, quick);
        assert_eq!(raced.node_reads, 0);
        assert_eq!(raced.turned_at, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_that_sends_nothing_delays_a_read_by_the_head_start_at_most() {
        let raced = race(Some(Peer::stalls()), None, NODE_READ, None).await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        assert_eq!(raced.took, HEAD_START + NODE_READ);
        assert_eq!(raced.node_reads, 1);
        assert_eq!(raced.turned_at, Some(HEAD_START));
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_reader_turns_the_read_to_the_nodes_at_once() {
        let failed = Duration::from_millis(250);
        let raced = race(Some(Peer::fails(failed)), None, NODE_READ, None).await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        assert_eq!(raced.took, failed + NODE_READ);
        assert_eq!(raced.turned_at, Some(failed));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_too_slow_at_the_head_start_loses_the_race() {
        // A third of the record by the head start: the rest would take
        // twice as long again, so the nodes are asked then, and answer first.
        let raced = race(
            Some(Peer::sends(HEAD_START * 3, "late")),
            None,
            NODE_READ,
            None,
        )
        .await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        assert_eq!(raced.took, HEAD_START + NODE_READ);
        assert_eq!(raced.turned_at, Some(HEAD_START));
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_on_pace_at_the_head_start_keeps_the_read() {
        // Five sixths of the record by the head start: the rest comes in
        // well under another, and no node is asked.
        let whole = HEAD_START + Duration::from_millis(600);
        let raced = race(
            Some(Peer::sends(whole, "from a reader")),
            None,
            NODE_READ,
            None,
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, whole);
        assert_eq!(raced.node_reads, 0);
        assert_eq!(raced.turned_at, None);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_on_pace_keeps_the_read_for_the_patience_at_most() {
        // On pace to finish just inside the patience, then nothing for the
        // last few pieces: the nodes are asked when the patience is up.
        let patience = HEAD_START * 2;
        let whole = patience - Duration::from_millis(100);
        let stuck = Peer {
            whole,
            stops: whole - Duration::from_millis(50),
            record: Some("never sent"),
        };
        let raced = race(Some(stuck), None, NODE_READ, None).await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        assert_eq!(raced.turned_at, Some(patience));
        assert_eq!(raced.took, patience + NODE_READ);
        assert_eq!(raced.node_reads, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_falling_behind_the_nodes_gives_up_the_read_early() {
        // Nine tenths of a record that would be whole at 5 s, then nothing:
        // by 5.6 s its average rate can no longer finish within the patience.
        let whole = Duration::from_secs(5);
        let behind = Peer {
            whole,
            stops: whole * 9 / 10,
            record: Some("never sent"),
        };
        let raced = race(Some(behind), None, NODE_READ, None).await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        let turned = raced.turned_at.unwrap_or(Duration::MAX);
        assert!(turned > whole && turned < HEAD_START * 2, "{turned:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_reader_beating_busy_nodes_keeps_the_read() {
        // The nodes take 10 s a record now: a reader that sends one in 8 s
        // keeps the read, and no node is asked.
        let busy = Duration::from_secs(10);
        let whole = Duration::from_secs(8);
        let raced = race_with_patience(
            Some(Peer::sends(whole, "from a reader")),
            None,
            busy,
            None,
            busy,
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, whole);
        assert_eq!(raced.node_reads, 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_page_frozen_past_the_head_start_does_not_stretch_the_window() {
        // Right after the ask, the page stalls for 5 s (every timer fires
        // late), and the reader looks on pace but sends nothing more: the
        // nodes are asked when the patience from the ask is up, at 6 s, not a
        // fresh window after the late head start.
        let started = tokio::time::Instant::now();
        let pace = crate::swarm::Pace::default();
        let turned_at = std::cell::Cell::new(None);
        let source = beside_peers(
            || {
                let pace = &pace;
                async move {
                    pace.note(RECORD_BYTES * 95 / 100, RECORD_BYTES);
                    tokio::time::advance(HEAD_START + Duration::from_secs(2)).await;
                    futures::future::pending::<Option<Bytes>>().await
                }
            },
            &pace,
            || async move {
                tokio::time::sleep(NODE_READ).await;
                Ok("from nodes")
            },
            (HEAD_START, HEAD_START * 2),
            futures::future::pending::<()>(),
            || turned_at.set(Some(started.elapsed())),
            Result::is_ok,
        )
        .await
        .unwrap_or_else(|error| panic!("the race failed: {error}"));
        assert_eq!(source, Source::Nodes("from nodes"));
        let turned = turned_at.get().unwrap_or(Duration::MAX);
        assert!(turned <= HEAD_START * 2 + PACE_POLL, "{turned:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn nodes_that_fail_leave_a_reader_still_sending_to_finish() {
        // A reader too slow to keep the read (whole at 8 s, past the 6 s
        // patience) has the nodes asked at the head start; they refuse at
        // 4 s, and the reader's record, still coming, is the read's.
        let whole = Duration::from_secs(8);
        let raced = race_against_nodes(
            Some(Peer::sends(whole, "from a reader")),
            None,
            (NODE_READ, false),
            None,
            HEAD_START * 2,
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, whole);
        assert_eq!(raced.node_reads, 1);
        assert_eq!(raced.turned_at, Some(HEAD_START));
        // The nodes' miss is told, for the node limiter.
        assert!(raced.node_missed);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_read_whose_nodes_fail_takes_the_record_still_coming() {
        // The node read waits its turn and then fails at 8 s; a reader that
        // offered at 2 s sends the record by 12 s.
        let offered = Duration::from_secs(2);
        let slow = Duration::from_secs(10);
        let raced = race_against_nodes(
            None,
            Some(Peer::sends(slow, "from a reader")),
            (Duration::from_secs(8), false),
            Some(offered),
            HEAD_START * 2,
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, offered + slow);
        assert_eq!(raced.node_reads, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_nodes_answer_stands_when_the_reader_fails_too() {
        // The nodes refuse at 4 s and the reader fails at 5 s: the read
        // fails with the nodes' answer once the reader has.
        let started = tokio::time::Instant::now();
        let result = beside_peers(
            || async {
                tokio::time::sleep(Duration::from_secs(5)).await;
                None
            },
            &crate::swarm::Pace::default(),
            || async {
                tokio::time::sleep(NODE_READ).await;
                Err::<&'static str, _>(Error::Network("the nodes are busy".into()))
            },
            (HEAD_START, HEAD_START * 2),
            futures::future::pending::<()>(),
            || {},
            Result::is_ok,
        )
        .await;
        assert!(
            matches!(&result, Err(Error::Network(message)) if message == "the nodes are busy"),
            "{result:?}"
        );
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_read_takes_a_record_a_reader_offers_meanwhile() {
        // Nobody has it at first; the node read waits its turn (8 s here); a
        // reader offers it after 2 s and sends it in half a second.
        let offered = Duration::from_secs(2);
        let quick = Duration::from_millis(500);
        let raced = race(
            None,
            Some(Peer::sends(quick, "from a reader")),
            Duration::from_secs(8),
            Some(offered),
        )
        .await;
        assert_eq!(raced.source, from_a_reader());
        assert_eq!(raced.took, offered + quick);
        assert_eq!(raced.node_reads, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_read_keeps_its_place_while_a_reader_fails() {
        // The reader that offered fails after 5 s; the node read, which never
        // gave up its turn, answers at 4 s as it would have anyway.
        let node = Duration::from_secs(4);
        let raced = race(
            None,
            Some(Peer::fails(Duration::from_secs(5))),
            node,
            Some(Duration::from_secs(1)),
        )
        .await;
        assert_eq!(raced.source, Source::Nodes("from nodes"));
        assert_eq!(raced.took, node);
        assert_eq!(raced.node_reads, 1);
    }

    #[cfg(feature = "native")]
    #[test]
    fn diagnostic_correlation_is_identical_on_wire_and_in_record() {
        let address = [7u8; 32];
        let correlation = DownloadRequestCorrelation::new(
            9_903,
            &PeerId::from_bytes([42; TEST_XORNAME_BYTE_LEN]),
        );
        let encoded = encode_diagnostic_chunk_get_request(&address, &correlation).unwrap();
        let wire = ChunkMessage::decode(&encoded).unwrap();
        assert_eq!(wire.request_id, correlation.request_id);
        assert!(matches!(wire.body, ChunkMessageBody::GetRequest(_)));

        let record = DownloadDiagnosticsRecord::peer_attempt(
            1,
            1,
            &address,
            "initial",
            1,
            None,
            "lookup-1",
            "expected-peer",
            Vec::new(),
            Vec::new(),
            None,
            None,
            None,
            None,
            None,
            "unknown",
            None,
            Some(false),
            Some(1),
            Some(8),
            100,
            200,
            &correlation,
            100,
            0,
            DownloadDiagnosticsOutcome::Timeout,
            Some("timeout".to_string()),
        );
        assert_eq!(record.request_id, Some(wire.request_id));
        assert_eq!(record.local_peer_id, Some(correlation.local_peer_id));
    }
    #[cfg(feature = "native")]
    #[test]
    fn classify_peer_attempt_pins_outcomes_and_response_attribution() {
        let chunk = DataChunk::new([0u8; 32], Bytes::from_static(b"payload"));
        let cases = [
            (
                Ok(Some(chunk)),
                DownloadDiagnosticsOutcome::Found,
                7,
                true,
                None,
            ),
            (
                Ok(None),
                DownloadDiagnosticsOutcome::NotFound,
                0,
                true,
                None,
            ),
            (
                Err(Error::Timeout("late".to_string())),
                DownloadDiagnosticsOutcome::Timeout,
                0,
                false,
                Some("timeout: late"),
            ),
            (
                Err(Error::Network("dial".to_string())),
                DownloadDiagnosticsOutcome::NetworkError,
                0,
                false,
                Some("network: dial"),
            ),
            (
                Err(Error::InvalidData("hash".to_string())),
                DownloadDiagnosticsOutcome::ProtocolError,
                0,
                true,
                Some("protocol: hash"),
            ),
            (
                Err(Error::Protocol("remote".to_string())),
                DownloadDiagnosticsOutcome::ProtocolError,
                0,
                false,
                Some("protocol: remote"),
            ),
        ];

        for (result, expected_outcome, expected_bytes, expected_response, expected_error) in cases {
            let (outcome, bytes, got_response, error) = classify_peer_attempt(&result);
            assert_eq!(outcome, expected_outcome);
            assert_eq!(bytes, expected_bytes);
            assert_eq!(got_response, expected_response);
            assert_eq!(error.as_deref(), expected_error);
        }
    }
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

#[cfg(feature = "native")]
impl Client {
    async fn chunk_get_from_peer_with_metadata(
        &self,
        address: &XorName,
        peer: &PeerId,
        peer_addrs: &[MultiAddr],
        correlation: &DownloadRequestCorrelation,
    ) -> Result<ChunkProtocolResponse<Option<DataChunk>, Error>> {
        let node = self.network().node();
        let message_bytes = encode_diagnostic_chunk_get_request(address, correlation)?;

        let timeout = Duration::from_secs(self.config().chunk_get_timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().chunk_get_timeout_secs;

        send_and_await_chunk_response_with_metadata(
            node,
            peer,
            message_bytes,
            correlation.request_id,
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
        .await
    }
}

#[cfg(feature = "native")]
#[derive(Default)]
struct ReadObservation {
    round: usize,
    peer_attempt: usize,
    lookup_ms: u64,
    lookup_id: String,
    contexts: std::collections::HashMap<PeerId, ClosestPeerDiagnostics>,
}
#[cfg(feature = "native")]
impl Client {
    async fn chunk_get_diagnostic_attempt(
        &self,
        address: &XorName,
        peer: &PeerId,
        addrs: &[MultiAddr],
        diag: &ChunkFetchDiagnostics<'_>,
        observation: &std::sync::Mutex<ReadObservation>,
    ) -> Result<Option<DataChunk>> {
        let (sweep, peer_attempt_no, lookup_duration_opt, lookup_correlation_id, peer_context) = {
            let mut state = observation.lock().unwrap_or_else(|e| e.into_inner());
            state.peer_attempt += 1;
            let context = state
                .contexts
                .remove(peer)
                .unwrap_or_else(|| ClosestPeerDiagnostics {
                    peer_id: *peer,
                    addresses: addrs.to_vec(),
                    address_types: Vec::new(),
                    local_last_seen_age_ms: None,
                    publisher_address_set_age_ms: None,
                    publisher_address_set_unix_ns: None,
                });
            (
                match state.round {
                    0 => "early",
                    1 => "initial",
                    _ => "retry",
                },
                state.peer_attempt,
                (state.round != 0).then_some(state.lookup_ms),
                state.lookup_id.clone(),
                context,
            )
        };
        let node = self.network().node();
        let peer_connected_before_request = node.is_peer_connected(peer).await;
        let (active_guard, active_requests_at_start) = ActiveDiagnosticRequestGuard::enter();
        let request_started_unix_ms = unix_now_ms();
        let resp_start = Instant::now();
        let correlation = DownloadRequestCorrelation::new(self.next_request_id(), node.peer_id());
        let observed = self
            .chunk_get_from_peer_with_metadata(address, peer, addrs, &correlation)
            .await;
        let response_elapsed_ms =
            u64::try_from(resp_start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let request_completed_unix_ms = unix_now_ms();
        // Count only the network request itself; route classification
        // and sidecar emission are diagnostic bookkeeping.
        drop(active_guard);

        let (result, source_peer, transport_source, route) = match observed {
            Ok(response) => {
                let route = node
                    .classify_peer_transport_route(
                        &response.source_peer,
                        response.transport_source.as_ref(),
                    )
                    .await;
                (
                    response.result,
                    Some(response.source_peer),
                    response.transport_source,
                    route,
                )
            }
            Err(error) => (Err(error), None, None, PeerRouteKind::Unknown),
        };
        let (outcome, bytes, _got_response, error) = classify_peer_attempt(&result);
        let lookup = if peer_attempt_no == 1 {
            lookup_duration_opt
        } else {
            None
        };
        diag.emit_peer_attempt(
            sweep,
            peer_attempt_no,
            lookup,
            &lookup_correlation_id,
            &peer_context,
            peer,
            source_peer.as_ref(),
            transport_source.as_ref(),
            route,
            peer_connected_before_request,
            active_requests_at_start,
            request_started_unix_ms,
            request_completed_unix_ms,
            &correlation,
            response_elapsed_ms,
            bytes,
            outcome,
            error,
        );
        result
    }
}
