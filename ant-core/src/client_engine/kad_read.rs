//! Chunk reads through a client-driven Kademlia lookup (ant-node ADR-0020).
//!
//! Each peer the lookup queries is asked for the chunk and for closer peers
//! in one step. A peer that answers get-or-closer requests gets one. Any other
//! peer gets FIND_NODE and a GET at once, which takes the same round trip and
//! works with every node in service today. The read stops at the first
//! verified chunk.
//!
//! Requests belong to the read, not to the lookup round that started them:
//! a round waits for closer peers only within its grace period, but a chunk
//! that is still arriving when the round moves on is not cancelled.
//!
//! The adapter supplies the network and validates everything it returns;
//! this module owns the lookup, the step order and when the read ends.

use ant_protocol::transport::client_routing::{compute_winner, SubjectReports};
use ant_protocol::transport::{
    collect_after_first_with_grace, run_iterative_lookup, DHTNode, IterativeLookup, LookupConfig,
    LookupKey, LookupQuery, LookupQueryOutcome, PeerId, ITERATION_GRACE_TIMEOUT_SECS,
    LOOKUP_TIMEOUT_SECS,
};
use futures::stream::{FuturesUnordered, StreamExt};
use std::collections::{HashMap, HashSet};
use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How long a round waits for the rest of its answers after the first one,
/// as in saorsa-core's own lookup.
const ROUND_GRACE: Duration = Duration::from_secs(ITERATION_GRACE_TIMEOUT_SECS);

/// The whole lookup's budget, as in saorsa-core's own lookup.
const LOOKUP_DEADLINE: Duration = Duration::from_secs(LOOKUP_TIMEOUT_SECS as u64);

/// What a peer answered one lookup step with.
pub(crate) enum StepAnswer<T> {
    /// It holds the chunk, already verified against the address.
    Found(T),
    /// Its closest known peers to the address, already validated.
    Closer(Vec<DHTNode>),
}

/// The network a kad read runs over. Implementations validate everything
/// they return: content against its address, peers as FIND_NODE answers.
pub(crate) trait KadNetwork {
    /// A verified chunk.
    type Chunk;
    /// A failed request.
    type Error;

    /// Whether the lookup may query `peer` now.
    fn is_dialable(&self, peer: &DHTNode) -> impl Future<Output = bool>;

    /// Connect to `peer`, and say whether it answers get-or-closer requests.
    fn prepare(&self, peer: &DHTNode) -> impl Future<Output = Result<bool, Self::Error>>;

    /// One get-or-closer step against a peer that answers them.
    fn get_or_closer(
        &self,
        peer: &DHTNode,
    ) -> impl Future<Output = Result<StepAnswer<Self::Chunk>, Self::Error>>;

    /// One FIND_NODE step against a peer.
    fn find_node(&self, peer: &DHTNode) -> impl Future<Output = Result<Vec<DHTNode>, Self::Error>>;

    /// A GET; `None` when the peer does not hold the chunk.
    fn get(&self, peer: &DHTNode)
        -> impl Future<Output = Result<Option<Self::Chunk>, Self::Error>>;
}

/// How a kad read ended without an error.
pub(crate) enum KadReadOutcome<T> {
    /// A queried peer returned the verified chunk.
    Found(T),
    /// The lookup ended and no queried peer returned the chunk. This is not
    /// evidence that the chunk is missing: the caller's ordinary read path,
    /// with its absence rule, decides that.
    NotFound,
}

/// A queried peer, handed from the lookup round to the read.
struct Step {
    peer: DHTNode,
    reply: oneshot::Sender<StepReply>,
}

/// What a step tells the lookup.
enum StepReply {
    Closer(Vec<DHTNode>),
    /// The peer answered with the chunk, so it has no peers to add.
    Answered,
    Failed,
}

/// Read `target` through a lookup seeded with `seeds`, returning the first
/// verified chunk any queried peer sends.
///
/// # Errors
///
/// Returns the first error `retryable` rejects, such as content that does not
/// match its address. Every other failure only removes that peer from the
/// lookup.
pub(crate) async fn kad_read<N: KadNetwork>(
    network: &N,
    target: LookupKey,
    count: usize,
    seeds: Vec<DHTNode>,
    retryable: impl Fn(&N::Error) -> bool,
) -> Result<KadReadOutcome<N::Chunk>, N::Error> {
    let Ok(mut lookup) = IterativeLookup::new(target, LookupConfig::saorsa(count)) else {
        return Ok(KadReadOutcome::NotFound);
    };
    for seed in seeds {
        if lookup.is_contactable(seed.peer_id.as_bytes()) {
            let _ = lookup.add_candidate(seed);
        }
    }
    let (steps_tx, mut steps) = mpsc::unbounded_channel();
    let mut query = KadQuery {
        network,
        steps: steps_tx,
        reports: HashMap::new(),
        contacted: HashSet::new(),
    };
    let walk = run_iterative_lookup(
        &mut lookup,
        &mut query,
        crate::runtime::sleep(LOOKUP_DEADLINE),
    );
    tokio::pin!(walk);
    let mut requests = FuturesUnordered::new();
    let mut walking = true;

    while walking || !requests.is_empty() {
        // Steps first, so every queried peer is asked before the lookup's
        // end is observed.
        tokio::select! {
            biased;
            Some(step) = steps.recv() => requests.push(serve_step(network, step, &retryable)),
            Some(result) = requests.next(), if !requests.is_empty() => match result {
                Ok(Some(chunk)) => return Ok(KadReadOutcome::Found(chunk)),
                Err(error) => return Err(error),
                Ok(None) => {}
            },
            _ = &mut walk, if walking => walking = false,
        }
    }
    Ok(KadReadOutcome::NotFound)
}

/// Ask one queried peer for the chunk and for closer peers, replying to the
/// lookup as soon as the closer peers are known.
///
/// Returns the chunk if this peer sent it, `None` otherwise, or an error that
/// `retryable` rejects.
async fn serve_step<N: KadNetwork>(
    network: &N,
    Step { peer, reply }: Step,
    retryable: &impl Fn(&N::Error) -> bool,
) -> Result<Option<N::Chunk>, N::Error> {
    let capable = match network.prepare(&peer).await {
        Ok(capable) => capable,
        Err(error) => {
            let _ = reply.send(StepReply::Failed);
            return fatal(error, retryable);
        }
    };
    if capable {
        let (answer, result) = match network.get_or_closer(&peer).await {
            Ok(StepAnswer::Found(chunk)) => (StepReply::Answered, Ok(Some(chunk))),
            Ok(StepAnswer::Closer(nodes)) => (StepReply::Closer(nodes), Ok(None)),
            Err(error) => (StepReply::Failed, fatal(error, retryable)),
        };
        let _ = reply.send(answer);
        return result;
    }

    // A peer that predates get-or-closer: FIND_NODE answers the lookup, and a
    // GET sent with it may still be transferring after that.
    let find = network.find_node(&peer);
    let get = network.get(&peer);
    tokio::pin!(find, get);
    let mut reply = Some(reply);
    let fetched = loop {
        tokio::select! {
            nodes = &mut find, if reply.is_some() => {
                if let Some(reply) = reply.take() {
                    let _ = reply.send(closer_reply(nodes));
                }
            }
            fetched = &mut get => break fetched,
        }
    };
    if let Some(reply) = reply.take() {
        let answer = if matches!(fetched, Ok(Some(_))) {
            StepReply::Answered
        } else {
            closer_reply((&mut find).await)
        };
        let _ = reply.send(answer);
    }
    match fetched {
        Ok(found) => Ok(found),
        Err(error) => fatal(error, retryable),
    }
}

fn closer_reply<E>(nodes: Result<Vec<DHTNode>, E>) -> StepReply {
    match nodes {
        Ok(nodes) => StepReply::Closer(nodes),
        Err(_) => StepReply::Failed,
    }
}

/// An error ends the read only when `retryable` rejects it.
fn fatal<T, E>(error: E, retryable: &impl Fn(&E) -> bool) -> Result<Option<T>, E> {
    if retryable(&error) {
        Ok(None)
    } else {
        Err(error)
    }
}

/// The lookup side of a kad read: hands each queried peer to the read and
/// turns the answers into lookup outcomes.
struct KadQuery<'a, N> {
    network: &'a N,
    steps: mpsc::UnboundedSender<Step>,
    /// Every responder's report about each peer, as in saorsa-core's lookup.
    reports: HashMap<PeerId, SubjectReports>,
    contacted: HashSet<PeerId>,
}

impl<N> KadQuery<'_, N> {
    /// The candidates `responder` reported, each replaced by the view that
    /// wins across every responder's report about it.
    fn winners(&mut self, responder: LookupKey, nodes: Vec<DHTNode>) -> Vec<DHTNode> {
        let responder = PeerId::from_bytes(responder);
        let mut winners = Vec::with_capacity(nodes.len());
        for node in nodes {
            if self.contacted.contains(&node.peer_id) {
                continue;
            }
            let subject = node.peer_id;
            let reports = self.reports.entry(subject).or_default();
            reports.insert(responder, node);
            if let Some((_, winner)) = compute_winner(&subject, reports) {
                winners.push(winner);
            }
        }
        winners
    }
}

impl<N: KadNetwork> LookupQuery<DHTNode> for KadQuery<'_, N> {
    type Error = Infallible;

    async fn is_candidate_eligible(&mut self, candidate: &DHTNode) -> Result<bool, Infallible> {
        Ok(self.network.is_dialable(candidate).await)
    }

    async fn query_batch(
        &mut self,
        _target: LookupKey,
        _count: usize,
        _iteration: usize,
        batch: Vec<DHTNode>,
    ) -> Result<Vec<LookupQueryOutcome<DHTNode>>, Infallible> {
        self.contacted.extend(batch.iter().map(|node| node.peer_id));
        let replies: FuturesUnordered<_> = batch
            .into_iter()
            .map(|peer| {
                let responder = *peer.peer_id.as_bytes();
                let (reply, answer) = oneshot::channel();
                let sent = self.steps.send(Step { peer, reply }).is_ok();
                async move { (responder, if sent { answer.await.ok() } else { None }) }
            })
            .collect();
        let answers =
            collect_after_first_with_grace(replies, || crate::runtime::sleep(ROUND_GRACE)).await;
        let mut outcomes = Vec::with_capacity(answers.len());
        for (responder, answer) in answers {
            outcomes.push(match answer {
                Some(StepReply::Closer(nodes)) => LookupQueryOutcome::Succeeded {
                    responder,
                    candidates: self.winners(responder, nodes),
                },
                Some(StepReply::Answered) => LookupQueryOutcome::Succeeded {
                    responder,
                    candidates: Vec::new(),
                },
                Some(StepReply::Failed) | None => LookupQueryOutcome::Failed { responder },
            });
        }
        Ok(outcomes)
    }

    async fn candidate_evicted(&mut self, peer: LookupKey) -> Result<(), Infallible> {
        self.reports.remove(&PeerId::from_bytes(peer));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A slow transfer that outlives its round's grace period.
    const SLOW_TRANSFER: Duration = Duration::from_secs(8);

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum MockError {
        Unreachable,
        Corrupt,
    }

    #[derive(Clone, Default)]
    struct MockPeer {
        answers_get_or_closer: bool,
        holds: bool,
        corrupt: bool,
        unreachable: bool,
        closer: Vec<u8>,
        transfer: Duration,
    }

    #[derive(Default)]
    struct MockNetwork {
        peers: HashMap<u8, MockPeer>,
        requests: std::sync::Mutex<Vec<(u8, &'static str)>>,
    }

    fn id(peer: &DHTNode) -> u8 {
        peer.peer_id.as_bytes()[0]
    }

    fn node(peer: u8) -> DHTNode {
        DHTNode {
            peer_id: PeerId::from_bytes([peer; 32]),
            addresses: Vec::new(),
            address_types: Vec::new(),
            distance: None,
            reliability: 1.0,
            address_authority: None,
        }
    }

    impl MockNetwork {
        fn with(peers: impl IntoIterator<Item = (u8, MockPeer)>) -> Self {
            Self {
                peers: peers.into_iter().collect(),
                requests: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn peer(&self, peer: &DHTNode) -> MockPeer {
            self.peers.get(&id(peer)).cloned().unwrap_or_default()
        }

        fn log(&self, peer: &DHTNode, request: &'static str) {
            self.requests.lock().unwrap().push((id(peer), request));
        }

        fn requests(&self) -> Vec<(u8, &'static str)> {
            self.requests.lock().unwrap().clone()
        }

        async fn chunk(&self, peer: &MockPeer) -> Result<Option<&'static str>, MockError> {
            if !peer.holds {
                return Ok(None);
            }
            tokio::time::sleep(peer.transfer).await;
            if peer.corrupt {
                Err(MockError::Corrupt)
            } else {
                Ok(Some("chunk"))
            }
        }
    }

    impl KadNetwork for MockNetwork {
        type Chunk = &'static str;
        type Error = MockError;

        async fn is_dialable(&self, _peer: &DHTNode) -> bool {
            true
        }

        async fn prepare(&self, peer: &DHTNode) -> Result<bool, MockError> {
            let mock = self.peer(peer);
            if mock.unreachable {
                return Err(MockError::Unreachable);
            }
            Ok(mock.answers_get_or_closer)
        }

        async fn get_or_closer(
            &self,
            peer: &DHTNode,
        ) -> Result<StepAnswer<&'static str>, MockError> {
            self.log(peer, "get_or_closer");
            let mock = self.peer(peer);
            match self.chunk(&mock).await? {
                Some(chunk) => Ok(StepAnswer::Found(chunk)),
                None => Ok(StepAnswer::Closer(
                    mock.closer.iter().copied().map(node).collect(),
                )),
            }
        }

        async fn find_node(&self, peer: &DHTNode) -> Result<Vec<DHTNode>, MockError> {
            self.log(peer, "find_node");
            Ok(self.peer(peer).closer.iter().copied().map(node).collect())
        }

        async fn get(&self, peer: &DHTNode) -> Result<Option<&'static str>, MockError> {
            self.log(peer, "get");
            let mock = self.peer(peer);
            self.chunk(&mock).await
        }
    }

    fn retryable(error: &MockError) -> bool {
        *error == MockError::Unreachable
    }

    async fn read(network: &MockNetwork, seeds: &[u8]) -> Result<Option<&'static str>, MockError> {
        let outcome = kad_read(
            network,
            [0; 32],
            ant_protocol::CLOSE_GROUP_SIZE,
            seeds.iter().copied().map(node).collect(),
            retryable,
        )
        .await?;
        Ok(match outcome {
            KadReadOutcome::Found(chunk) => Some(chunk),
            KadReadOutcome::NotFound => None,
        })
    }

    fn router(closer: &[u8]) -> MockPeer {
        MockPeer {
            answers_get_or_closer: true,
            closer: closer.to_vec(),
            ..MockPeer::default()
        }
    }

    fn holder() -> MockPeer {
        MockPeer {
            answers_get_or_closer: true,
            holds: true,
            ..MockPeer::default()
        }
    }

    fn legacy(peer: MockPeer) -> MockPeer {
        MockPeer {
            answers_get_or_closer: false,
            ..peer
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_holder_found_by_get_or_closer_ends_the_read() {
        let network = MockNetwork::with([(9, router(&[2])), (2, holder())]);

        assert_eq!(read(&network, &[9]).await, Ok(Some("chunk")));
        assert_eq!(
            network.requests(),
            vec![(9, "get_or_closer"), (2, "get_or_closer")]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn older_peers_get_find_node_and_a_get_in_the_same_step() {
        let network = MockNetwork::with([(9, legacy(router(&[2]))), (2, legacy(holder()))]);

        assert_eq!(read(&network, &[9]).await, Ok(Some("chunk")));
        let requests = network.requests();
        for step in [(9, "find_node"), (9, "get"), (2, "find_node"), (2, "get")] {
            assert!(requests.contains(&step), "missing {step:?} in {requests:?}");
        }
        assert!(!requests
            .iter()
            .any(|(_, request)| *request == "get_or_closer"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_mixed_network_reads_through_both_kinds_of_peer() {
        let network = MockNetwork::with([
            (9, legacy(router(&[5]))),
            (5, router(&[2])),
            (2, legacy(holder())),
        ]);

        assert_eq!(read(&network, &[9]).await, Ok(Some("chunk")));
        let requests = network.requests();
        assert!(requests.contains(&(5, "get_or_closer")));
        assert!(requests.contains(&(2, "get")));
    }

    #[tokio::test(start_paused = true)]
    async fn a_transfer_outlives_its_rounds_grace_period() {
        let slow = MockPeer {
            transfer: SLOW_TRANSFER,
            ..holder()
        };
        let network = MockNetwork::with([(9, router(&[3, 2])), (3, router(&[])), (2, slow)]);

        assert_eq!(read(&network, &[9]).await, Ok(Some("chunk")));
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_peer_only_leaves_the_lookup() {
        let unreachable = MockPeer {
            unreachable: true,
            ..holder()
        };
        let network = MockNetwork::with([(9, router(&[3, 2])), (3, unreachable), (2, holder())]);

        assert_eq!(read(&network, &[9]).await, Ok(Some("chunk")));
    }

    #[tokio::test(start_paused = true)]
    async fn corrupt_content_ends_the_read_with_an_error() {
        let corrupt = MockPeer {
            corrupt: true,
            ..holder()
        };
        let network = MockNetwork::with([(9, router(&[2])), (2, corrupt)]);

        assert_eq!(read(&network, &[9]).await, Err(MockError::Corrupt));
    }

    #[tokio::test(start_paused = true)]
    async fn a_lookup_without_a_holder_reports_not_found() {
        let network = MockNetwork::with([
            (9, router(&[3, 2])),
            (3, legacy(router(&[]))),
            (2, router(&[])),
        ]);

        assert_eq!(read(&network, &[9]).await, Ok(None));
        let requests = network.requests();
        assert!(requests.contains(&(3, "get")));
        assert!(requests.contains(&(2, "get_or_closer")));
    }
}
