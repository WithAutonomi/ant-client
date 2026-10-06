//! V2-1358 read-strategy prototype bench. Not a production path.
//!
//! Three ways to read one chunk on the same live client, each recording every
//! GET it sends:
//!
//! - [`ReadStrategy::Baseline`]: today's `chunk_get` policy.
//! - [`ReadStrategy::Progress`]: native only. The same policy, but each
//!   FIND_NODE responder, and each connected peer it returns, becomes an
//!   early-GET hint while the lookup runs. This is what the browser already
//!   does, so on the browser it is the baseline.
//! - [`ReadStrategy::Eager`]: native only. Like `Progress`, but each peer is
//!   offered as a hint when the lookup queries it, not when it answers, so a
//!   holder's GET shares the lookup's dial and round trip. Still at most two
//!   GETs at once.
//! - [`ReadStrategy::Combined`]: emulates a request that returns the chunk or
//!   closer peers. Every peer the lookup queries also gets a GET at the same
//!   moment, and the read stops at the first verified chunk. Nodes need no
//!   change: a GET and a FIND_NODE sent together to one peer cost one round
//!   trip, as the combined request would.
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
use ant_protocol::{DataChunk, XorName};
use futures::stream::{FuturesUnordered, StreamExt};
#[cfg(feature = "native")]
use saorsa_core::LookupObserver;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::future::Future;
#[cfg(feature = "native")]
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use web_time::Instant;

/// How long to keep collecting in-flight GETs after a Combined read has its
/// chunk, so the duplicate bytes they carry are counted.
const DUPLICATE_DRAIN_TIMEOUT: Duration = Duration::from_secs(15);

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
    answers: Mutex<Vec<LookupAnswer>>,
    queried_at: Mutex<HashMap<PeerId, u64>>,
    lookup_ms: Mutex<Option<u64>>,
}

impl Recorder {
    pub(crate) fn new() -> Self {
        Self {
            started: Instant::now(),
            attempts: Mutex::new(Vec::new()),
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

    /// Emulated chunk-or-closer-peers lookup: GET every peer `lookup` queries,
    /// as it queries it, and stop at the first verified chunk.
    pub(crate) async fn bench_combined_read<L>(
        &self,
        address: &XorName,
        recorder: &Recorder,
        lookup: L,
        mut events: mpsc::UnboundedReceiver<LookupEvent>,
    ) -> ReadTrace
    where
        L: Future<Output = Result<Vec<PeerAddrs>>>,
    {
        tokio::pin!(lookup);
        let mut probes = FuturesUnordered::new();
        let mut tried = HashSet::new();
        let mut lookup_result = None;
        let mut found = None;

        // Run the lookup and the probes it triggers until a verified chunk
        // arrives, or until the lookup is done and no probe is left.
        while found.is_none() && (lookup_result.is_none() || !probes.is_empty()) {
            // Events first, so every queried peer is probed before the
            // lookup's completion is observed.
            tokio::select! {
                biased;
                Some(event) = events.recv() => match event {
                    LookupEvent::Query { round, peer, at_ms } => {
                        recorder.record_query(peer.0, at_ms);
                        if tried.insert(peer.0) {
                            probes.push(self.bench_probe(address, peer, round, recorder));
                        }
                    }
                    LookupEvent::Response { round, responder, at_ms, .. } => {
                        recorder.record_answer(round, &responder.0, at_ms);
                    }
                },
                Some((round, result)) = probes.next(), if !probes.is_empty() => {
                    match result {
                        Ok(Some(chunk)) => found = Some((Some(round), Ok(Some(chunk)))),
                        Err(error) if !is_retryable(&error) => {
                            found = Some((Some(round), Err(error)));
                        }
                        _ => {}
                    }
                }
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
                    self.bench_combined_fallback(address, closest, &tried, recorder)
                        .await,
                )
            }
        };
        let total_ms = recorder.now_ms();

        // A holder that had the request in flight still sends its copy.
        let _ = crate::runtime::timeout(DUPLICATE_DRAIN_TIMEOUT, async {
            while probes.next().await.is_some() {}
        })
        .await;
        recorder.finish(
            ReadStrategy::Combined,
            address,
            &result,
            total_ms,
            found_round,
        )
    }

    async fn bench_probe(
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
            ReadStrategy::Combined => {
                let (lookup, events) = self.bench_observed_lookup(address, count, &recorder);
                return self
                    .bench_combined_read(address, &recorder, lookup, events)
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
