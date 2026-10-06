//! V2-1358 read-strategy prototype bench. Not a production path.
//!
//! Ways to read one chunk on the same live client, each recording every
//! request it sends:
//!
//! - [`ReadStrategy::Baseline`]: today's `chunk_get` policy.
//! - [`ReadStrategy::Progress`]: native only. The same policy, but each
//!   FIND_NODE responder, and each connected peer it returns, becomes an
//!   early-GET hint while the lookup runs. This is what the browser already
//!   does, so on the browser it is the baseline.
//! - [`ReadStrategy::Eager`]: like `Progress`, but each peer is offered as a
//!   hint when the lookup queries it, not when it answers. Still at most two
//!   GETs at once.
//!
//! The remaining strategies act on each peer the lookup queries, at the
//! moment it is queried, and stop at the first verified chunk:
//!
//! - [`ReadStrategy::Combined`]: GET every queried peer. Emulates a request
//!   that returns the chunk or closer peers, with no node change.
//! - [`ReadStrategy::Capped`]: GET queried peers closest-first, at most
//!   [`CAPPED_GETS`] at once. No protocol change.
//! - [`ReadStrategy::Have`]: ask every queried peer whether it holds the
//!   chunk, and GET from holders one at a time, hedged. Emulates a request
//!   that answers "have it" or closer peers; a storage quote's
//!   `already_stored` stands in for the "have it" answer.
//! - [`ReadStrategy::HaveInline`]: like `Have`, but the closest queried peer
//!   of each lookup round is sent a GET, as if asked to inline the chunk.
//!
//! The platform supplies the lookup and a stream of [`LookupEvent`]s; the
//! read policy and the recording are shared.

use crate::client_engine::read::{
    read_targets, retrieve_progressive, ReadCandidates, MAX_GET_FALLBACK_PEERS,
};
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use crate::data::network::ReadProgress;
#[cfg(feature = "native")]
use ant_protocol::transport::DHTNode;
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{
    ChunkMessage, ChunkMessageBody, ChunkQuoteRequestV2, ChunkQuoteResponse, DataChunk, XorName,
    MAX_CHUNK_SIZE,
};
use futures::stream::{FuturesUnordered, StreamExt};
#[cfg(feature = "native")]
use saorsa_core::LookupObserver;
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
#[cfg(feature = "native")]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use web_time::Instant;

/// How long to keep collecting in-flight GETs after a lookup-driven read has
/// its chunk, so the duplicate bytes they carry are counted.
pub const DUPLICATE_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);
/// GETs a [`ReadStrategy::Capped`] read keeps in flight.
const CAPPED_GETS: usize = 2;
/// GETs to known holders a [`ReadStrategy::Have`] read keeps in flight.
const HOLDER_GETS: usize = 2;
/// How long a holder GET runs before a second holder is asked too.
const HOLDER_HEDGE_DELAY: Duration = Duration::from_millis(1500);
/// Size declared by a have-probe quote. Any valid size works; the node only
/// reports whether it already stores the address.
const HAVE_PROBE_SIZE: u64 = MAX_CHUNK_SIZE as u64;

/// A peer and the addresses to reach it by.
pub(crate) type PeerAddrs = (PeerId, Vec<MultiAddr>);

/// A read strategy under comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadStrategy {
    /// Today's policy.
    Baseline,
    /// Lookup progress feeds early GETs (native only).
    Progress,
    /// Queried peers feed early GETs as they are queried (native only).
    Eager,
    /// Emulated chunk-or-closer-peers request.
    Combined,
    /// GET queried peers closest-first, a few at a time.
    Capped,
    /// Emulated have-or-closer-peers request, then a hedged fetch.
    Have,
    /// `Have`, with the closest peer of each round asked for the chunk.
    HaveInline,
}

/// How a lookup-driven read turns queried peers into requests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbePolicy {
    /// GET queried peers closest-first, at most `limit` at once.
    Get { limit: usize },
    /// Ask queried peers whether they hold the chunk; GET holders.
    Have { inline_closest: bool },
}

impl ReadStrategy {
    fn probe_policy(self) -> Option<ProbePolicy> {
        match self {
            Self::Combined => Some(ProbePolicy::Get { limit: usize::MAX }),
            Self::Capped => Some(ProbePolicy::Get { limit: CAPPED_GETS }),
            Self::Have => Some(ProbePolicy::Have {
                inline_closest: false,
            }),
            Self::HaveInline => Some(ProbePolicy::Have {
                inline_closest: true,
            }),
            Self::Baseline | Self::Progress | Self::Eager => None,
        }
    }
}

impl ReadStrategy {
    /// Parse a strategy name.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "baseline" => Some(Self::Baseline),
            "progress" => Some(Self::Progress),
            "eager" => Some(Self::Eager),
            "combined" => Some(Self::Combined),
            "capped" => Some(Self::Capped),
            "have" => Some(Self::Have),
            "have_inline" => Some(Self::HaveInline),
            _ => None,
        }
    }
}

/// One GET sent while reading a chunk.
#[derive(Clone, Debug, Serialize)]
pub struct ReadAttempt {
    /// Peer asked.
    pub peer: String,
    /// Sent before the lookup completed.
    pub early: bool,
    /// Lookup round that queried the peer (Combined only).
    pub round: Option<usize>,
    /// Milliseconds from the start of the read.
    pub started_ms: u64,
    /// Milliseconds from the start of the read.
    pub completed_ms: u64,
    /// `found`, `not_found`, `timeout`, `network`, `protocol` or `invalid`.
    pub outcome: &'static str,
    /// Verified chunk bytes received.
    pub bytes: u64,
}

/// One "do you hold it?" probe sent while reading a chunk.
#[derive(Clone, Debug, Serialize)]
pub struct HaveProbe {
    /// Peer asked.
    pub peer: String,
    /// Lookup round that queried the peer.
    pub round: usize,
    /// Milliseconds from the start of the read.
    pub started_ms: u64,
    /// Milliseconds from the start of the read.
    pub completed_ms: u64,
    /// `have`, `not_have`, `timeout`, `network`, `protocol` or `invalid`.
    pub outcome: &'static str,
}

/// A FIND_NODE answer seen during the lookup.
#[derive(Clone, Debug, Serialize)]
pub struct LookupAnswer {
    /// Responder.
    pub peer: String,
    /// Lookup round.
    pub round: usize,
    /// Milliseconds from the start of the read when the lookup queried it.
    pub queried_ms: u64,
    /// Milliseconds from the start of the read when its answer was processed.
    pub answered_ms: u64,
}

/// Everything recorded for one chunk read.
#[derive(Clone, Debug, Serialize)]
pub struct ReadTrace {
    /// Strategy used.
    pub strategy: ReadStrategy,
    /// Chunk address.
    pub address: String,
    /// A verified chunk was returned.
    pub found: bool,
    /// Milliseconds from the start of the read to the verified chunk, or to
    /// giving up.
    pub total_ms: u64,
    /// Milliseconds the lookup took, when it ran to completion.
    pub lookup_ms: Option<u64>,
    /// Lookup round of the GET that returned the chunk (Combined only).
    pub found_round: Option<usize>,
    /// FIND_NODE answers, when the strategy observes the lookup.
    pub answers: Vec<LookupAnswer>,
    /// Every GET sent, including those still in flight when the chunk arrived.
    pub attempts: Vec<ReadAttempt>,
    /// Every have-probe sent.
    pub have_probes: Vec<HaveProbe>,
    /// Error that ended the read.
    pub error: Option<String>,
}

/// What a platform's lookup reports while it runs.
pub(crate) enum LookupEvent {
    /// FIND_NODE is about to be sent to `peer` in `round`.
    Query {
        round: usize,
        peer: PeerAddrs,
        at_ms: u64,
    },
    /// `responder` answered in `round` with `candidates`.
    Response {
        round: usize,
        responder: PeerAddrs,
        /// Read only by the native progress strategy.
        #[cfg_attr(not(feature = "native"), allow(dead_code))]
        candidates: Vec<PeerAddrs>,
        at_ms: u64,
    },
}

pub(crate) fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn outcome_of(result: &Result<Option<DataChunk>>) -> (&'static str, u64) {
    match result {
        Ok(Some(chunk)) => ("found", chunk.content.len() as u64),
        Ok(None) => ("not_found", 0),
        Err(Error::Timeout(_)) => ("timeout", 0),
        Err(Error::Network(_)) => ("network", 0),
        Err(Error::Protocol(_)) => ("protocol", 0),
        Err(_) => ("invalid", 0),
    }
}

fn have_outcome_of(result: &Result<bool>) -> &'static str {
    match result {
        Ok(true) => "have",
        Ok(false) => "not_have",
        Err(Error::Timeout(_)) => "timeout",
        Err(Error::Network(_)) => "network",
        Err(Error::Protocol(_)) => "protocol",
        Err(_) => "invalid",
    }
}

fn is_retryable(error: &Error) -> bool {
    matches!(
        error,
        Error::Timeout(_) | Error::Network(_) | Error::Protocol(_)
    )
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Shared per-read recorder.
pub(crate) struct Recorder {
    pub(crate) started: Instant,
    attempts: Mutex<Vec<ReadAttempt>>,
    have_probes: Mutex<Vec<HaveProbe>>,
    answers: Mutex<Vec<LookupAnswer>>,
    queried_at: Mutex<HashMap<PeerId, u64>>,
    lookup_ms: Mutex<Option<u64>>,
}

impl Recorder {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            attempts: Mutex::new(Vec::new()),
            have_probes: Mutex::new(Vec::new()),
            answers: Mutex::new(Vec::new()),
            queried_at: Mutex::new(HashMap::new()),
            lookup_ms: Mutex::new(None),
        }
    }

    pub(crate) fn now_ms(&self) -> u64 {
        elapsed_ms(self.started)
    }

    fn record_attempt(
        &self,
        peer: &PeerId,
        early: bool,
        round: Option<usize>,
        started_ms: u64,
        result: &Result<Option<DataChunk>>,
    ) {
        let (outcome, bytes) = outcome_of(result);
        lock(&self.attempts).push(ReadAttempt {
            peer: peer.to_hex(),
            early,
            round,
            started_ms,
            completed_ms: self.now_ms(),
            outcome,
            bytes,
        });
    }

    fn record_have_probe(
        &self,
        peer: &PeerId,
        round: usize,
        started_ms: u64,
        result: &Result<bool>,
    ) {
        lock(&self.have_probes).push(HaveProbe {
            peer: peer.to_hex(),
            round,
            started_ms,
            completed_ms: self.now_ms(),
            outcome: have_outcome_of(result),
        });
    }

    fn record_query(&self, peer: PeerId, at_ms: u64) {
        lock(&self.queried_at).insert(peer, at_ms);
    }

    fn record_answer(&self, round: usize, peer: &PeerId, at_ms: u64) {
        let queried_ms = lock(&self.queried_at).get(peer).copied().unwrap_or(at_ms);
        lock(&self.answers).push(LookupAnswer {
            peer: peer.to_hex(),
            round,
            queried_ms,
            answered_ms: at_ms,
        });
    }

    fn record_lookup_done(&self) {
        *lock(&self.lookup_ms) = Some(self.now_ms());
    }

    pub(crate) fn finish(
        &self,
        strategy: ReadStrategy,
        address: &XorName,
        result: &Result<Option<DataChunk>>,
        total_ms: u64,
        found_round: Option<usize>,
    ) -> ReadTrace {
        ReadTrace {
            strategy,
            address: hex::encode(address),
            found: matches!(result, Ok(Some(_))),
            total_ms,
            lookup_ms: *lock(&self.lookup_ms),
            found_round,
            answers: lock(&self.answers).clone(),
            attempts: lock(&self.attempts).clone(),
            have_probes: lock(&self.have_probes).clone(),
            error: result.as_ref().err().map(ToString::to_string),
        }
    }
}

impl Client {
    /// `retrieve_progressive`, exactly as `chunk_get` drives it, with the
    /// discovery `discover` supplies. Bypasses the chunk cache.
    pub(crate) async fn bench_progressive_read<D, DF>(
        &self,
        address: &XorName,
        recorder: &Recorder,
        discover: D,
    ) -> Result<Option<DataChunk>>
    where
        D: Fn(ReadProgress) -> DF,
        DF: Future<Output = Result<Vec<PeerAddrs>>>,
    {
        let discover = &discover;
        retrieve_progressive(
            *address,
            MAX_GET_FALLBACK_PEERS,
            |sender| async move {
                let progress = ReadProgress::new(*address, *self.network().peer_id(), sender);
                self.network().seed_read_candidates(&progress).await;
                let closest = discover(progress).await.unwrap_or_default();
                recorder.record_lookup_done();
                ReadCandidates {
                    closest,
                    known: self.bench_known_peers().await,
                }
            },
            |(peer, _)| *peer.as_bytes(),
            |(peer, addrs), early| async move {
                let started_ms = recorder.now_ms();
                let result = self.chunk_get_from_peer(address, &peer, &addrs).await;
                recorder.record_attempt(&peer, early, None, started_ms, &result);
                result
            },
            is_retryable,
            crate::runtime::sleep,
        )
        .await
    }

    async fn bench_known_peers(&self) -> Vec<PeerAddrs> {
        let local = *self.network().peer_id();
        self.network()
            .known_peers()
            .await
            .into_iter()
            .filter(|node| node.peer_id != local)
            .map(|node| {
                let addrs = node.addresses_by_priority();
                (node.peer_id, addrs)
            })
            .collect()
    }

    /// Read through `lookup`, acting on each peer as it is queried according
    /// to `strategy`'s probe policy, and stop at the first verified chunk.
    pub(crate) async fn bench_lookup_read<L>(
        &self,
        address: &XorName,
        strategy: ReadStrategy,
        recorder: &Recorder,
        lookup: L,
        mut events: mpsc::UnboundedReceiver<LookupEvent>,
        drain: Duration,
    ) -> ReadTrace
    where
        L: Future<Output = Result<Vec<PeerAddrs>>>,
    {
        let Some(policy) = strategy.probe_policy() else {
            let result = Err(Error::Protocol(format!(
                "{strategy:?} is not lookup-driven"
            )));
            return recorder.finish(strategy, address, &result, 0, None);
        };
        tokio::pin!(lookup);
        let mut state = ProbeState::new(*address, policy);
        let mut gets = FuturesUnordered::new();
        let mut haves = FuturesUnordered::new();
        let mut lookup_result = None;
        let mut found = None;

        // Run the lookup and the requests it triggers until a verified chunk
        // arrives, or until the lookup is done and nothing is left to ask.
        while found.is_none() && (lookup_result.is_none() || state.has_work(&gets, &haves)) {
            for (peer, round) in state.next_gets(gets.len(), recorder.now_ms()) {
                gets.push(self.bench_get(address, peer, round, recorder));
            }
            let hedge = state.hedge_wait(gets.len(), recorder.now_ms());
            // Events first, so a round's queries are all seen before its
            // answers, and before the lookup's completion.
            tokio::select! {
                biased;
                Some(event) = events.recv() => {
                    let mut batch = vec![event];
                    while let Ok(event) = events.try_recv() {
                        batch.push(event);
                    }
                    for (peer, round, have) in state.on_events(batch, recorder) {
                        if have {
                            haves.push(self.bench_have_probe(address, peer, round, recorder));
                        } else {
                            state.queue_get(peer, round);
                        }
                    }
                }
                Some((round, result)) = gets.next(), if !gets.is_empty() => {
                    match result {
                        Ok(Some(chunk)) => found = Some((Some(round), Ok(Some(chunk)))),
                        Err(error) if !is_retryable(&error) => {
                            found = Some((Some(round), Err(error)));
                        }
                        _ => {}
                    }
                }
                Some((peer, round, result)) = haves.next(), if !haves.is_empty() => {
                    if matches!(result, Ok(true)) {
                        state.add_holder(peer, round);
                    }
                }
                () = crate::runtime::sleep(hedge.unwrap_or_default()), if hedge.is_some() => {}
                result = &mut lookup, if lookup_result.is_none() => {
                    recorder.record_lookup_done();
                    lookup_result = Some(result);
                }
            }
        }

        let (found_round, result) = match found {
            Some(found) => found,
            None => {
                let closest = lookup_result.and_then(Result::ok).unwrap_or_default();
                (
                    None,
                    self.bench_combined_fallback(address, closest, &state.asked, recorder)
                        .await,
                )
            }
        };
        let total_ms = recorder.now_ms();

        // A holder that had the request in flight still sends its copy.
        let _ =
            crate::runtime::timeout(drain, async { while gets.next().await.is_some() {} }).await;
        recorder.finish(strategy, address, &result, total_ms, found_round)
    }

    async fn bench_get(
        &self,
        address: &XorName,
        (peer, addrs): PeerAddrs,
        round: usize,
        recorder: &Recorder,
    ) -> (usize, Result<Option<DataChunk>>) {
        let started_ms = recorder.now_ms();
        let result = self.chunk_get_from_peer(address, &peer, &addrs).await;
        recorder.record_attempt(&peer, true, Some(round), started_ms, &result);
        (round, result)
    }

    /// Ask `peer` whether it holds `address`, through a storage quote's
    /// `already_stored`. A measurement stand-in for a "have it" answer.
    async fn bench_have_probe(
        &self,
        address: &XorName,
        peer: PeerAddrs,
        round: usize,
        recorder: &Recorder,
    ) -> (PeerAddrs, usize, Result<bool>) {
        let started_ms = recorder.now_ms();
        let request_id = self.next_request_id();
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::QuoteRequestV2(ChunkQuoteRequestV2::new(
                *address,
                HAVE_PROBE_SIZE,
            )),
        };
        let result = match message.encode() {
            Ok(bytes) => {
                crate::data::network::send_and_await_chunk_response(
                    self.network(),
                    &peer.0,
                    bytes,
                    request_id,
                    Duration::from_secs(self.config().chunk_get_timeout_secs),
                    &peer.1,
                    |body| match body {
                        ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Success {
                            already_stored,
                            ..
                        }) => Some(Ok(already_stored)),
                        ChunkMessageBody::QuoteResponse(_) => Some(Ok(false)),
                        _ => None,
                    },
                    |e| Error::Network(format!("have-probe send failed: {e}")),
                    || Error::Timeout("have-probe timed out".to_string()),
                )
                .await
            }
            Err(e) => Err(Error::Protocol(format!("have-probe encode failed: {e}"))),
        };
        recorder.record_have_probe(&peer.0, round, started_ms, &result);
        (peer, round, result)
    }

    /// The lookup finished without a holder answering: GET its closest
    /// peers, then known peers, one at a time, as today's ordinary path does.
    async fn bench_combined_fallback(
        &self,
        address: &XorName,
        closest: Vec<PeerAddrs>,
        tried: &HashSet<PeerId>,
        recorder: &Recorder,
    ) -> Result<Option<DataChunk>> {
        let targets = read_targets(
            ReadCandidates {
                closest,
                known: self.bench_known_peers().await,
            },
            address,
            |(peer, _)| *peer.as_bytes(),
        );
        for (peer, addrs) in targets
            .into_iter()
            .filter(|(peer, _)| !tried.contains(peer))
        {
            let started_ms = recorder.now_ms();
            let result = self.chunk_get_from_peer(address, &peer, &addrs).await;
            recorder.record_attempt(&peer, false, None, started_ms, &result);
            match result {
                Ok(Some(chunk)) => return Ok(Some(chunk)),
                Err(error) if !is_retryable(&error) => return Err(error),
                _ => {}
            }
        }
        Ok(None)
    }
}

/// Which queried peers a lookup-driven read asks for the chunk, and when.
struct ProbeState {
    target: XorName,
    policy: ProbePolicy,
    /// Peers asked for the chunk or whether they hold it.
    asked: HashSet<PeerId>,
    /// Queried peers waiting for a GET slot (`ProbePolicy::Get`).
    queued: Vec<(PeerAddrs, usize)>,
    /// Peers that hold the chunk, in the order they said so.
    holders: VecDeque<(PeerAddrs, usize)>,
    /// When the newest GET started, in recorder milliseconds.
    newest_get_ms: u64,
}

impl ProbeState {
    fn new(target: XorName, policy: ProbePolicy) -> Self {
        Self {
            target,
            policy,
            asked: HashSet::new(),
            queued: Vec::new(),
            holders: VecDeque::new(),
            newest_get_ms: 0,
        }
    }

    fn distance(&self, peer: &PeerId) -> XorName {
        ant_protocol::transport::xor_distance(peer.as_bytes(), &self.target)
    }

    /// Record lookup events. Returns each newly queried peer, its round, and
    /// whether to have-probe it (`true`) or queue it for a GET (`false`).
    fn on_events(
        &mut self,
        events: Vec<LookupEvent>,
        recorder: &Recorder,
    ) -> Vec<(PeerAddrs, usize, bool)> {
        let mut queried: Vec<(PeerAddrs, usize)> = Vec::new();
        for event in events {
            match event {
                LookupEvent::Query { round, peer, at_ms } => {
                    recorder.record_query(peer.0, at_ms);
                    if self.asked.insert(peer.0) {
                        queried.push((peer, round));
                    }
                }
                LookupEvent::Response {
                    round,
                    responder,
                    at_ms,
                    ..
                } => recorder.record_answer(round, &responder.0, at_ms),
            }
        }
        let inline: HashSet<PeerId> = match self.policy {
            ProbePolicy::Have {
                inline_closest: true,
            } => {
                let mut closest: HashMap<usize, (XorName, PeerId)> = HashMap::new();
                for ((peer, _), round) in &queried {
                    let distance = self.distance(peer);
                    let entry = closest.entry(*round).or_insert((distance, *peer));
                    if distance < entry.0 {
                        *entry = (distance, *peer);
                    }
                }
                closest.into_values().map(|(_, peer)| peer).collect()
            }
            _ => HashSet::new(),
        };
        queried
            .into_iter()
            .map(|(peer, round)| {
                let have =
                    matches!(self.policy, ProbePolicy::Have { .. }) && !inline.contains(&peer.0);
                (peer, round, have)
            })
            .collect()
    }

    fn queue_get(&mut self, peer: PeerAddrs, round: usize) {
        self.queued.push((peer, round));
    }

    fn add_holder(&mut self, peer: PeerAddrs, round: usize) {
        self.holders.push_back((peer, round));
    }

    /// GETs to start now, given `running` GETs in flight.
    fn next_gets(&mut self, running: usize, now_ms: u64) -> Vec<(PeerAddrs, usize)> {
        let mut started = Vec::new();
        // Queued peers (direct GETs and inline asks) go closest-first.
        let limit = match self.policy {
            ProbePolicy::Get { limit } => limit,
            ProbePolicy::Have { .. } => usize::MAX,
        };
        let target = self.target;
        self.queued.sort_by_key(|((peer, _), _)| {
            std::cmp::Reverse(ant_protocol::transport::xor_distance(
                peer.as_bytes(),
                &target,
            ))
        });
        while running + started.len() < limit {
            let Some(next) = self.queued.pop() else { break };
            started.push(next);
        }
        // Holders: one at a time, a second once the newest GET has run for
        // the hedge delay.
        if matches!(self.policy, ProbePolicy::Have { .. }) {
            let busy = running + started.len();
            let hedge_due = now_ms.saturating_sub(self.newest_get_ms) >= hedge_ms();
            if busy == 0 || (busy < HOLDER_GETS && hedge_due) {
                if let Some(holder) = self.holders.pop_front() {
                    started.push(holder);
                }
            }
        }
        if !started.is_empty() {
            self.newest_get_ms = now_ms;
        }
        started
    }

    /// How long until a hedged holder GET may start, if one is waiting.
    fn hedge_wait(&self, running: usize, now_ms: u64) -> Option<Duration> {
        if self.holders.is_empty() || running == 0 || running >= HOLDER_GETS {
            return None;
        }
        let due_ms = self.newest_get_ms.saturating_add(hedge_ms());
        Some(Duration::from_millis(due_ms.saturating_sub(now_ms)))
    }

    fn has_work<G, H>(&self, gets: &FuturesUnordered<G>, haves: &FuturesUnordered<H>) -> bool {
        !gets.is_empty() || !haves.is_empty() || !self.queued.is_empty() || !self.holders.is_empty()
    }
}

fn hedge_ms() -> u64 {
    u64::try_from(HOLDER_HEDGE_DELAY.as_millis()).unwrap_or(u64::MAX)
}

// =============================================================================
// Native lookups
// =============================================================================

/// Forwards saorsa-core lookup callbacks without blocking the lookup.
#[cfg(feature = "native")]
struct ForwardingObserver {
    started: Instant,
    events: mpsc::UnboundedSender<LookupEvent>,
}

#[cfg(feature = "native")]
fn peer_addresses(node: &DHTNode) -> PeerAddrs {
    (node.peer_id, node.addresses_by_priority())
}

#[cfg(feature = "native")]
impl LookupObserver for ForwardingObserver {
    fn on_query(&self, iteration: usize, node: &DHTNode) {
        let _ = self.events.send(LookupEvent::Query {
            round: iteration,
            peer: peer_addresses(node),
            at_ms: elapsed_ms(self.started),
        });
    }

    fn on_response(&self, iteration: usize, responder: &DHTNode, candidates: &[DHTNode]) {
        let _ = self.events.send(LookupEvent::Response {
            round: iteration,
            responder: peer_addresses(responder),
            candidates: candidates.iter().map(peer_addresses).collect(),
            at_ms: elapsed_ms(self.started),
        });
    }
}

#[cfg(feature = "native")]
impl Client {
    /// Read one chunk with `strategy`, bypassing the chunk cache, and return
    /// what happened on the wire.
    pub async fn bench_chunk_read(&self, address: &XorName, strategy: ReadStrategy) -> ReadTrace {
        self.bench_chunk_read_with_drain(address, strategy, DUPLICATE_DRAIN_TIMEOUT)
            .await
    }

    /// [`Self::bench_chunk_read`], waiting at most `drain` after a
    /// lookup-driven read for GETs still in flight. A zero drain stops
    /// recording them; their bytes still arrive.
    pub async fn bench_chunk_read_with_drain(
        &self,
        address: &XorName,
        strategy: ReadStrategy,
        drain: Duration,
    ) -> ReadTrace {
        let recorder = Recorder::new();
        let count = self.config().close_group_size;
        let result = match strategy {
            ReadStrategy::Baseline => {
                self.bench_progressive_read(address, &recorder, |_| {
                    self.closest_peers(address, count)
                })
                .await
            }
            ReadStrategy::Progress | ReadStrategy::Eager => {
                let offer_queried = strategy == ReadStrategy::Eager;
                self.bench_progressive_read(address, &recorder, |progress| {
                    self.bench_progress_lookup(address, count, progress, offer_queried, &recorder)
                })
                .await
            }
            ReadStrategy::Combined
            | ReadStrategy::Capped
            | ReadStrategy::Have
            | ReadStrategy::HaveInline => {
                let (lookup, events) = self.bench_observed_lookup(address, count, &recorder);
                return self
                    .bench_lookup_read(address, strategy, &recorder, lookup, events, drain)
                    .await;
            }
        };
        let total_ms = recorder.now_ms();
        recorder.finish(strategy, address, &result, total_ms, None)
    }

    /// The native lookup, reporting its progress as [`LookupEvent`]s.
    fn bench_observed_lookup<'a>(
        &'a self,
        address: &'a XorName,
        count: usize,
        recorder: &Recorder,
    ) -> (
        impl Future<Output = Result<Vec<PeerAddrs>>> + 'a,
        mpsc::UnboundedReceiver<LookupEvent>,
    ) {
        let (events, received) = mpsc::unbounded_channel();
        let observer = Arc::new(ForwardingObserver {
            started: recorder.started,
            events,
        });
        let lookup = async move {
            let local = *self.network().peer_id();
            let found = self
                .network()
                .node()
                .dht()
                .find_closest_nodes_observed(address, count + 1, observer)
                .await
                .map_err(|e| Error::Network(format!("DHT closest-nodes lookup failed: {e}")))?;
            Ok(found
                .into_iter()
                .filter(|node| node.peer_id != local)
                .take(count)
                .map(|node| peer_addresses(&node))
                .collect())
        };
        (lookup, received)
    }

    /// Offer each responder, and each connected peer it returns, as an
    /// early-GET hint, as the browser does. With `offer_queried`, also offer
    /// each peer as the lookup queries it.
    async fn bench_progress_lookup(
        &self,
        address: &XorName,
        count: usize,
        progress: ReadProgress,
        offer_queried: bool,
        recorder: &Recorder,
    ) -> Result<Vec<PeerAddrs>> {
        let node = self.network().node();
        let (lookup, mut events) = self.bench_observed_lookup(address, count, recorder);
        tokio::pin!(lookup);
        loop {
            tokio::select! {
                biased;
                Some(event) = events.recv() => match event {
                    LookupEvent::Query { peer, at_ms, .. } => {
                        recorder.record_query(peer.0, at_ms);
                        if offer_queried {
                            progress.offer(vec![peer]);
                        }
                    }
                    LookupEvent::Response { round, responder, candidates, at_ms } => {
                        recorder.record_answer(round, &responder.0, at_ms);
                        let mut hints = vec![responder];
                        for candidate in candidates {
                            if node.is_peer_connected(&candidate.0).await {
                                hints.push(candidate);
                            }
                        }
                        progress.offer(hints);
                    }
                },
                result = &mut lookup => return result,
            }
        }
    }
}
