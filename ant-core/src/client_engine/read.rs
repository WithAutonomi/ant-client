//! Transport-neutral native read policy. Adapters supply discovery, GET, sleep and
//! the hedge delay their transfers need; the engine owns ordering, fallback
//! bounds, absence decisions and retry timing.
use ant_protocol::CLOSE_GROUP_SIZE;
use futures::channel::oneshot;
use futures::future::{select, Either};
#[cfg(test)]
use futures::FutureExt;
use futures::{stream::FuturesUnordered, StreamExt};
use rand::Rng as _;
#[cfg(test)]
use std::cell::{Cell, RefCell};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Mutex;
use std::task::Poll;
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    time::Duration,
};

pub(crate) const MAX_GET_FALLBACK_PEERS: usize = 20;
pub(crate) const CLOSE_GROUP_RETRY_DELAY: Duration = Duration::from_secs(1);
/// Rounds a read waits for a refusing holder before reporting its holders
/// busy. Each wait lasts until the first refusal that may hide a holder ends,
/// so with the browser's 15 s refusal cooldown (ant-node ADR-0023) the waits
/// add at most 45 s and three lookups to a read on a network that is full.
const MAX_REFUSAL_WAITS: usize = 3;
/// A stalled early GET may race one other candidate this long after it was
/// dispatched, unless the transport sets its own delay. Sized to a healthy peer's
/// QUIC transfer of a 4 MiB record, which takes about a second.
pub(crate) const DEFAULT_EARLY_READ_HEDGE_DELAY: Duration = Duration::from_secs(1);
/// Speculative GETs per round are bounded by this multiple of the early
/// allowance, including hints that may hold the record after stale hints used it.
const MAX_EARLY_READS_PER_ALLOWANCE: usize = 2;
/// Early GETs in flight at once: one that stalled, and the candidate raced against it.
const MAX_CONCURRENT_EARLY_READS: usize = 2;

/// Tells a read that its GET has been dispatched to its peer: it has passed the
/// client's own limits, which another GET would wait for too, and from then on
/// waits only on that peer, for a connection, a session slot, the send and the
/// reply. The read counts the GET's hedge delay from then, so the client's own
/// limits never make a peer look stalled, while a busy or silent peer does.
pub struct RequestDispatched(Option<oneshot::Sender<()>>);

impl RequestDispatched {
    /// A notice and the receiver that learns of it.
    pub(crate) fn channel() -> (Self, oneshot::Receiver<()>) {
        let (sender, receiver) = oneshot::channel();
        (Self(Some(sender)), receiver)
    }

    /// A notice no read waits for, such as an ordinary GET's.
    pub(crate) fn unobserved() -> Self {
        Self(None)
    }

    /// Record that the request has been dispatched to its peer.
    pub fn notify(self) {
        if let Some(sender) = self.0 {
            let _ = sender.send(());
        }
    }
}

/// How a read speculates on hints while discovery runs.
#[derive(Clone, Copy)]
pub(crate) struct EarlyReads {
    /// Hints a round may read before later ones must converge on the target.
    pub(crate) limit: usize,
    /// How long an early GET waits, once dispatched, before another candidate
    /// may race it: a healthy peer's transfer of a full record.
    pub(crate) hedge_delay: Duration,
}

/// Resolves once a GET has waited `delay` since it was dispatched. A GET that
/// fails before it is dispatched never stalls; its result retires it.
async fn stall_after_dispatch<S, SF>(dispatched: oneshot::Receiver<()>, sleep: &S, delay: Duration)
where
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    if dispatched.await.is_ok() {
        sleep(delay).await;
    } else {
        futures::future::pending::<()>().await;
    }
}

/// A GET's stall timer, cleared once it fires.
type StallTimer<F> = Option<Pin<Box<F>>>;

/// The stall timers of the early GETs in flight, by peer.
struct Stalls<F>(Vec<([u8; 32], StallTimer<F>)>);

impl<F: Future<Output = ()>> Stalls<F> {
    fn new() -> Self {
        Self(Vec::new())
    }

    fn start(&mut self, peer: [u8; 32], stall: F) {
        self.0.push((peer, Some(Box::pin(stall))));
    }

    fn retire(&mut self, peer: &[u8; 32]) {
        self.0.retain(|(id, _)| id != peer);
    }

    /// Another GET may join those in flight only as a hedge: beside at most one,
    /// once that one has stalled.
    fn admit_another(&self) -> bool {
        self.0.len() < MAX_CONCURRENT_EARLY_READS && self.0.iter().all(|(_, stall)| stall.is_none())
    }

    /// Resolves when the next GET in flight stalls.
    fn next(&mut self) -> impl Future<Output = ()> + '_ {
        futures::future::poll_fn(move |cx| {
            for (_, stall) in &mut self.0 {
                if stall
                    .as_mut()
                    .is_some_and(|timer| timer.as_mut().poll(cx).is_ready())
                {
                    *stall = None;
                    return Poll::Ready(());
                }
            }
            Poll::Pending
        })
    }
}

pub(crate) fn is_authoritative_not_found(not_found: usize, queried: usize) -> bool {
    queried >= ant_protocol::CLOSE_GROUP_MAJORITY && not_found == queried
}

/// Settle one GET's result: the record, or an error that is not retryable, ends
/// the read. A miss records `id` as a peer that answered without the record.
fn settle<T, E>(
    id: [u8; 32],
    result: Result<Option<T>, E>,
    not_found: &Mutex<HashSet<[u8; 32]>>,
    retryable: impl Fn(&E) -> bool,
) -> ControlFlow<Result<Option<T>, E>> {
    match result {
        Ok(Some(value)) => ControlFlow::Break(Ok(Some(value))),
        Ok(None) => {
            not_found
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(id);
            ControlFlow::Continue(())
        }
        Err(error) if !retryable(&error) => ControlFlow::Break(Err(error)),
        Err(_) => ControlFlow::Continue(()),
    }
}

/// Discovery converges on the target, so a hint with fewer than a close group of
/// attempted peers nearer the target may still hold the record.
fn may_hold(attempted: &HashSet<[u8; 32]>, candidate: &[u8; 32], target: &[u8; 32]) -> bool {
    let distance = ant_protocol::transport::xor_distance(candidate, target);
    attempted
        .iter()
        .filter(|peer| ant_protocol::transport::xor_distance(peer, target) < distance)
        .count()
        < ant_protocol::CLOSE_GROUP_SIZE
}

/// Race one already-connected, known candidate against ordinary discovery and
/// retrieval. Both paths must verify content before returning `Some`. A cache
/// miss or transport failure is never evidence of network-wide absence; the
/// ordinary discovery/retry policy still runs. At most one extra GET is active.
pub(crate) async fn race_cached_read<T, E>(
    cached: Option<impl Future<Output = Result<Option<T>, E>>>,
    discovered: impl Future<Output = Result<Option<T>, E>>,
    retryable: impl Fn(&E) -> bool,
) -> Result<Option<T>, E> {
    let Some(cached) = cached else {
        return discovered.await;
    };
    let remaining = match select(Box::pin(cached), Box::pin(discovered)).await {
        Either::Left((result, remaining)) => match result {
            Ok(Some(value)) => return Ok(Some(value)),
            Err(error) if !retryable(&error) => return Err(error),
            _ => remaining.await,
        },
        Either::Right((result, remaining)) => match result {
            Ok(Some(value)) => return Ok(Some(value)),
            Err(error) if !retryable(&error) => return Err(error),
            _ => remaining.await,
        },
    };
    match remaining {
        Err(error) if retryable(&error) => Ok(None),
        result => result,
    }
}

/// Discovery results need not include all reachable storage holders.
pub(crate) struct ReadCandidates<P> {
    pub closest: Vec<P>,
    pub known: Vec<P>,
}

/// A uniformly random index below `len`, for spreading reads across holders.
pub(crate) fn uniform_choice(len: usize) -> usize {
    rand::thread_rng().gen_range(0..len.max(1))
}

/// Reorder the close group at the front of `peers` with `choose`, which
/// returns an index below its argument. Every client used to read a record
/// from its single nearest holder, so a popular record loaded one node while
/// the rest of its close group idled. A uniform `choose` gives every holder an
/// equal share of first reads; a `choose` that always returns zero keeps
/// nearest-first order.
fn spread_close_group<P>(peers: &mut [P], choose: &mut impl FnMut(usize) -> usize) {
    let close_group = peers.len().min(CLOSE_GROUP_SIZE);
    for index in 0..close_group {
        let swap = index + choose(close_group - index).min(close_group - index - 1);
        peers.swap(index, swap);
    }
}

/// The next hint to read before discovery completes. Unattempted, `eligible`
/// hints in the close group as currently known (the nearest `CLOSE_GROUP_SIZE`
/// hints) are chosen between with `choose`; once none of those remain, the
/// nearest remaining eligible hint is next.
fn next_early_peer<'a, P>(
    hints: &'a [P],
    attempted: &HashSet<[u8; 32]>,
    target: &[u8; 32],
    key: impl Fn(&P) -> [u8; 32],
    eligible: impl Fn(&P) -> bool,
    choose: &mut impl FnMut(usize) -> usize,
) -> Option<&'a P> {
    let mut ranked = hints.iter().collect::<Vec<_>>();
    ranked.sort_by_key(|peer| ant_protocol::transport::xor_distance(&key(peer), target));
    ranked.dedup_by_key(|peer| key(peer));
    let readable = |peer: &P| !attempted.contains(&key(peer)) && eligible(peer);
    let close_group = ranked
        .iter()
        .take(CLOSE_GROUP_SIZE)
        .filter(|peer| readable(peer))
        .copied()
        .collect::<Vec<_>>();
    let Some(last) = close_group.len().checked_sub(1) else {
        return ranked.into_iter().find(|peer| readable(peer));
    };
    close_group
        .get(choose(close_group.len()).min(last))
        .copied()
}

/// Keep primary peers first, the close group in `choose` order and the rest
/// by XOR distance, then bounded, deduplicated known peers. Discovery failure
/// suppression must not filter the latter.
pub(crate) fn read_targets<P>(
    candidates: ReadCandidates<P>,
    target: &[u8; 32],
    key: impl Fn(&P) -> [u8; 32],
    choose: &mut impl FnMut(usize) -> usize,
) -> Vec<P> {
    let mut primary = candidates.closest;
    let mut known = candidates.known;
    primary.sort_by_key(|p| ant_protocol::transport::xor_distance(&key(p), target));
    known.sort_by_key(|p| ant_protocol::transport::xor_distance(&key(p), target));
    let mut seen = HashSet::new();
    primary.retain(|p| seen.insert(key(p)));
    spread_close_group(&mut primary, choose);
    primary.extend(
        known
            .into_iter()
            .filter(|p| seen.insert(key(p)))
            .take(MAX_GET_FALLBACK_PEERS),
    );
    primary
}

/// Discover and read concurrently. Early hints are never absence votes or
/// close-group authority. An early GET that has waited `early.hedge_delay` since
/// it was dispatched may race one other candidate. At most two GETs run, including
/// after discovery, and a GET joins another only once that one has stalled: a raced
/// GET, and one still in flight when discovery finishes, must stall before another
/// candidate joins it. The same rules hold on native and WASM. A peer is queried at
/// most once per round, and only the completed final candidate set can establish
/// absence.
///
/// `refused` lists peers the adapter skips for now because they declined a
/// session while full, each with the time until it is tried again. While one
/// of them may hold the record, a round that finds nothing proves nothing:
/// the read waits for the first of them to return, up to `MAX_REFUSAL_WAITS`
/// times, and then fails with `busy()` rather than report the record absent.
#[allow(clippy::too_many_arguments)] // Adapters supply each hook; the engine owns how they combine.
pub(crate) async fn retrieve_progressive<P, T, E, D, DF, G, GF, S, SF>(
    target: [u8; 32],
    early: EarlyReads,
    mut choose: impl FnMut(usize) -> usize,
    discover: D,
    key: impl Fn(&P) -> [u8; 32],
    get: G,
    retryable: impl Fn(&E) -> bool,
    refused: impl Fn() -> Vec<([u8; 32], Duration)>,
    busy: impl FnOnce() -> E,
    sleep: S,
) -> Result<Option<T>, E>
where
    P: Clone,
    D: Fn(tokio::sync::watch::Sender<Vec<P>>) -> DF,
    DF: Future<Output = ReadCandidates<P>>,
    G: Fn(P, bool, RequestDispatched) -> GF,
    GF: Future<Output = Result<Option<T>, E>>,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    let allowance = early.limit.min(MAX_GET_FALLBACK_PEERS);
    let max_early_reads = allowance * MAX_EARLY_READS_PER_ALLOWANCE;
    let mut delay = None;
    let mut retried = false;
    let mut refusal_waits = 0;
    loop {
        if let Some(delay) = delay {
            sleep(delay).await;
        }
        let refused_at_start = refused();
        let (sender, mut updates) = tokio::sync::watch::channel(Vec::new());
        let mut discovery = Box::pin(discover(sender));
        let mut attempted = HashSet::new();
        let not_found = Mutex::new(HashSet::new());
        let mut active = FuturesUnordered::new();
        let mut stalls = Stalls::new();
        let mut updates_open = true;
        let candidates = loop {
            if stalls.admit_another() && attempted.len() < max_early_reads {
                // Stale hints can use the allowance before discovery reaches the
                // holders; hints converging on the target remain eligible.
                let within_allowance = attempted.len() < allowance;
                let next = {
                    let hints = updates.borrow_and_update();
                    next_early_peer(
                        &hints,
                        &attempted,
                        &target,
                        &key,
                        |peer| within_allowance || may_hold(&attempted, &key(peer), &target),
                        &mut choose,
                    )
                    .cloned()
                };
                if let Some(peer) = next {
                    let id = key(&peer);
                    attempted.insert(id);
                    let (dispatched, on_dispatched) = RequestDispatched::channel();
                    stalls.start(
                        id,
                        stall_after_dispatch(on_dispatched, &sleep, early.hedge_delay),
                    );
                    let future = get(peer, true, dispatched);
                    active.push(async move { (id, future.await) });
                }
            }
            let event = {
                let changed = async {
                    if updates_open {
                        updates.changed().await.is_ok()
                    } else {
                        futures::future::pending().await
                    }
                };
                let read = async {
                    if active.is_empty() {
                        futures::future::pending().await
                    } else {
                        active.next().await.expect("nonempty early reads")
                    }
                };
                // Stall timers are polled before hint updates: a timer starts
                // when first polled after its GET is dispatched, so a burst of
                // updates must not delay it.
                match select(
                    discovery.as_mut(),
                    Box::pin(select(
                        Box::pin(read),
                        Box::pin(select(Box::pin(stalls.next()), Box::pin(changed))),
                    )),
                )
                .await
                {
                    Either::Left((candidates, _)) => Either::Left(candidates),
                    Either::Right((event, _)) => Either::Right(match event {
                        Either::Left((result, _)) => Either::Left(result),
                        Either::Right((event, _)) => Either::Right(match event {
                            Either::Left(_) => None,
                            Either::Right((open, _)) => Some(open),
                        }),
                    }),
                }
            };
            match event {
                Either::Left(candidates) => break candidates,
                Either::Right(Either::Right(Some(open))) => updates_open = open,
                // A GET stalled; the next pass may race it.
                Either::Right(Either::Right(None)) => {}
                Either::Right(Either::Left((id, result))) => {
                    stalls.retire(&id);
                    if let ControlFlow::Break(outcome) = settle(id, result, &not_found, &retryable)
                    {
                        return outcome;
                    }
                }
            }
        };
        let peers = read_targets(candidates, &target, &key, &mut choose);
        let final_ids = peers.iter().map(&key).collect::<HashSet<_>>();
        // Discovery may finish with early GETs still in flight. The ordinary path
        // joins them as another early GET would, beside at most one that has
        // stalled. It never creates a third wire GET, nor races a GET still
        // within its hedge delay. One not yet dispatched waits for the client's
        // own limits, which the ordinary path's GET would wait for too.
        let (slot, slot_available) = oneshot::channel();
        let wait_for_slot = !stalls.admit_another();
        let pending_early = (!active.is_empty()).then(|| {
            let not_found = &not_found;
            let retryable = &retryable;
            async move {
                let mut slot = Some(slot);
                loop {
                    if stalls.admit_another() {
                        if let Some(slot) = slot.take() {
                            let _ = slot.send(());
                        }
                    }
                    let event = match select(active.next(), Box::pin(stalls.next())).await {
                        Either::Left((read, _)) => Some(read),
                        Either::Right(_) => None,
                    };
                    match event {
                        Some(Some((id, result))) => {
                            stalls.retire(&id);
                            if let ControlFlow::Break(outcome) =
                                settle(id, result, not_found, retryable)
                            {
                                return outcome;
                            }
                        }
                        Some(None) => return Ok(None),
                        // A GET stalled; the next pass may let the ordinary path start.
                        None => {}
                    }
                }
            }
        });
        let ordinary = async {
            if wait_for_slot {
                let _ = slot_available.await;
            }
            for peer in peers {
                let id = key(&peer);
                if attempted.contains(&id) {
                    continue;
                }
                let result = get(peer, false, RequestDispatched::unobserved()).await;
                if let ControlFlow::Break(outcome) = settle(id, result, &not_found, &retryable) {
                    return outcome;
                }
            }
            Ok(None)
        };
        if let Some(value) = race_cached_read(pending_early, ordinary, &retryable).await? {
            return Ok(Some(value));
        }
        let refused_now = refused().into_iter().collect::<HashMap<_, _>>();
        let (absent, refusal) = {
            let misses = not_found.lock().unwrap_or_else(|e| e.into_inner());
            let absent = is_authoritative_not_found(
                final_ids.intersection(&misses).count(),
                final_ids.len(),
            );
            // A refused peer stays out of discovery and reads, so this round
            // may never have asked it. It cannot hold the record once it, or a
            // close group of nearer peers, answered NotFound. A refusal that
            // ended during the round is ready now.
            let refusal = refused_at_start
                .iter()
                .map(|(peer, _)| peer)
                .chain(refused_now.keys())
                .filter(|peer| !misses.contains(*peer) && may_hold(&misses, peer, &target))
                .map(|peer| refused_now.get(peer).copied().unwrap_or_default())
                .min();
            (absent, refusal)
        };
        delay = Some(match refusal {
            Some(_) if refusal_waits == MAX_REFUSAL_WAITS => return Err(busy()),
            Some(remaining) => {
                refusal_waits += 1;
                remaining.max(CLOSE_GROUP_RETRY_DELAY)
            }
            None if absent || retried => return Ok(None),
            None => {
                retried = true;
                CLOSE_GROUP_RETRY_DELAY
            }
        });
    }
}

/// Nearest-first holder choice, for tests that pin the read order.
#[cfg(test)]
fn nearest(_len: usize) -> usize {
    0
}

#[cfg(test)]
async fn retrieve<P: Clone, T, E, D, DF, G, GF, S, SF>(
    target: [u8; 32],
    discover: D,
    key: impl Fn(&P) -> [u8; 32],
    get: G,
    retryable: impl Fn(&E) -> bool,
    sleep: S,
) -> Result<Option<T>, E>
where
    D: Fn() -> DF,
    DF: Future<Output = ReadCandidates<P>>,
    G: Fn(P) -> GF,
    GF: Future<Output = Result<Option<T>, E>>,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    retrieve_progressive(
        target,
        EarlyReads {
            limit: 0,
            hedge_delay: DEFAULT_EARLY_READ_HEDGE_DELAY,
        },
        nearest,
        |_| discover(),
        key,
        |peer, _, _| get(peer),
        retryable,
        Vec::new,
        || unreachable!("no peer is refused"),
        sleep,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEDGE_DELAY: Duration = DEFAULT_EARLY_READ_HEDGE_DELAY;

    fn early_reads(limit: usize) -> EarlyReads {
        EarlyReads {
            limit,
            hedge_delay: HEDGE_DELAY,
        }
    }

    fn peer(id: u8) -> [u8; 32] {
        [id; 32]
    }

    #[derive(Debug, PartialEq)]
    enum ReadError {
        Transport,
        Integrity,
        Busy,
    }

    /// The browser's refusal cooldown, as these tests model it.
    const REFUSAL_COOLDOWN: Duration = Duration::from_secs(15);

    /// Read `peer(0)` while `refusal` gives the time left on `holder`'s
    /// refusal at each instant. Discovery that starts while it is refused
    /// skips it and finds the next seven peers, which answer NotFound. Each
    /// discovery takes `round`.
    async fn read_past_refused_holder(
        holder: [u8; 32],
        refusal: impl Fn(tokio::time::Instant) -> Option<Duration>,
        round: Duration,
    ) -> (Result<Option<u8>, ReadError>, usize) {
        let rounds = Cell::new(0);
        let refusal = &refusal;
        let result = retrieve_progressive(
            peer(0),
            early_reads(0),
            nearest,
            |_| {
                rounds.set(rounds.get() + 1);
                let closest = if refusal(tokio::time::Instant::now()).is_some() {
                    (2..=8).map(peer).collect()
                } else {
                    (1..=7).map(peer).collect()
                };
                async move {
                    tokio::time::sleep(round).await;
                    ReadCandidates {
                        closest,
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |p, _, _| async move { Ok(if p == holder { Some(42) } else { None }) },
            |_| true,
            || {
                refusal(tokio::time::Instant::now())
                    .map(|remaining| (holder, remaining))
                    .into_iter()
                    .collect()
            },
            || ReadError::Busy,
            tokio::time::sleep,
        )
        .await;
        (result, rounds.get())
    }

    /// A single refusal that ends at `until`.
    fn refused_until(
        until: tokio::time::Instant,
    ) -> impl Fn(tokio::time::Instant) -> Option<Duration> {
        move |now| {
            until
                .checked_duration_since(now)
                .filter(|remaining| !remaining.is_zero())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_holder_is_read_after_its_cooldown_rather_than_reported_absent() {
        let start = tokio::time::Instant::now();
        let (result, rounds) = read_past_refused_holder(
            peer(1),
            refused_until(start + REFUSAL_COOLDOWN),
            Duration::ZERO,
        )
        .await;
        // Seven NotFound answers would otherwise prove the record absent.
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(rounds, 2);
        assert_eq!(start.elapsed(), REFUSAL_COOLDOWN);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_that_ends_during_a_round_is_retried_after_the_usual_delay() {
        const ROUND: Duration = Duration::from_secs(2);
        let start = tokio::time::Instant::now();
        let (result, rounds) = read_past_refused_holder(
            peer(1),
            refused_until(start + Duration::from_secs(1)),
            ROUND,
        )
        .await;
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(rounds, 2);
        assert_eq!(start.elapsed(), ROUND + CLOSE_GROUP_RETRY_DELAY + ROUND);
    }

    #[tokio::test(start_paused = true)]
    async fn holders_that_keep_refusing_are_reported_busy_rather_than_absent() {
        let start = tokio::time::Instant::now();
        // Refused again on every attempt, so always a full cooldown away.
        let (result, rounds) =
            read_past_refused_holder(peer(1), |_| Some(REFUSAL_COOLDOWN), Duration::ZERO).await;
        assert_eq!(result, Err(ReadError::Busy));
        assert_eq!(rounds, MAX_REFUSAL_WAITS + 1);
        let waits = u32::try_from(MAX_REFUSAL_WAITS).unwrap();
        assert_eq!(start.elapsed(), REFUSAL_COOLDOWN * waits);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_peer_beyond_the_close_group_does_not_delay_absence() {
        let start = tokio::time::Instant::now();
        let (result, rounds) =
            read_past_refused_holder(peer(200), |_| Some(REFUSAL_COOLDOWN), Duration::ZERO).await;
        assert_eq!(result, Ok(None));
        assert_eq!(rounds, 1);
        assert_eq!(start.elapsed(), Duration::ZERO);
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_early_read_can_race_a_new_hint_before_discovery_finishes() {
        let gets = RefCell::new(Vec::new());
        let start = tokio::time::Instant::now();
        let read = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                tokio::time::sleep(Duration::from_millis(500)).await;
                updates.send_replace(vec![peer(1), peer(2)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                async move {
                    if p == peer(1) {
                        futures::future::pending().await
                    } else {
                        Ok::<_, ReadError>(Some(42))
                    }
                }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        let result = tokio::time::timeout(Duration::from_secs(2), read)
            .await
            .expect("hedged read stalled");
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(start.elapsed(), HEDGE_DELAY);
        assert_eq!(*gets.borrow(), vec![(peer(1), true), (peer(2), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_completion_does_not_add_a_third_get_to_two_early_reads() {
        let (found, found_rx) = futures::channel::oneshot::channel();
        let found = RefCell::new(Some(found));
        let found_rx = RefCell::new(Some(found_rx));
        let (release, release_rx) = futures::channel::oneshot::channel();
        let release_rx = RefCell::new(Some(release_rx));
        let gets = RefCell::new(Vec::new());
        let future = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| {
                let found_rx = found_rx.borrow_mut().take().unwrap();
                async move {
                    updates.send_replace(vec![peer(1), peer(2)]);
                    found_rx.await.unwrap();
                    ReadCandidates {
                        closest: vec![peer(1), peer(2), peer(3)],
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                let release_rx = if p == peer(1) {
                    release_rx.borrow_mut().take()
                } else {
                    None
                };
                let found = &found;
                async move {
                    if let Some(release_rx) = release_rx {
                        release_rx.await.unwrap();
                        Ok::<_, ReadError>(None)
                    } else if p == peer(2) {
                        found.borrow_mut().take().unwrap().send(()).unwrap();
                        futures::future::pending().await
                    } else {
                        Ok(Some(42))
                    }
                }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        futures::pin_mut!(future);
        assert!(future.as_mut().now_or_never().is_none());
        tokio::time::advance(HEDGE_DELAY).await;
        assert!(future.as_mut().now_or_never().is_none());
        assert_eq!(*gets.borrow(), vec![(peer(1), true), (peer(2), true)]);
        release.send(()).unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(2), future)
                .await
                .expect("ordinary read stalled"),
            Ok(Some(42))
        );
        assert_eq!(
            *gets.borrow(),
            vec![(peer(1), true), (peer(2), true), (peer(3), false)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_completion_waits_for_a_single_early_read_to_stall() {
        // Discovery finishes a quarter of the way into the early GET's hedge
        // delay. A holder answering within the delay is the only GET; one that
        // stalls is raced once the delay ends, not when discovery finishes.
        for (reply, elapsed, expected) in [
            (
                Some(HEDGE_DELAY / 2),
                HEDGE_DELAY / 2,
                vec![(peer(1), true)],
            ),
            (None, HEDGE_DELAY, vec![(peer(1), true), (peer(2), false)]),
        ] {
            let gets = RefCell::new(Vec::new());
            let start = tokio::time::Instant::now();
            let read = retrieve_progressive(
                peer(0),
                early_reads(7),
                nearest,
                |updates| async move {
                    updates.send_replace(vec![peer(1)]);
                    tokio::time::sleep(HEDGE_DELAY / 4).await;
                    ReadCandidates {
                        closest: vec![peer(1), peer(2)],
                        known: vec![],
                    }
                },
                |p| *p,
                |p, early, dispatched| {
                    gets.borrow_mut().push((p, early));
                    dispatched.notify();
                    async move {
                        match (p == peer(1), reply) {
                            (true, Some(delay)) => {
                                tokio::time::sleep(delay).await;
                                Ok::<_, ReadError>(Some(1))
                            }
                            (true, None) => futures::future::pending().await,
                            (false, _) => Ok(Some(2)),
                        }
                    }
                },
                |_| true,
                Vec::new,
                || unreachable!("no peer is refused"),
                tokio::time::sleep,
            );
            let expected_value = if reply.is_some() { 1 } else { 2 };
            assert_eq!(read.await, Ok(Some(expected_value)));
            assert_eq!(start.elapsed(), elapsed);
            assert_eq!(*gets.borrow(), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn time_before_a_get_is_dispatched_does_not_count_toward_its_hedge() {
        // The first GET waits for the client's own limits for two hedge delays
        // before it is dispatched, then stalls. It is raced a full delay after it
        // was dispatched.
        let queued = HEDGE_DELAY * 2;
        let gets = RefCell::new(Vec::new());
        let start = tokio::time::Instant::now();
        let read = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1), peer(2)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, early, dispatched| {
                gets.borrow_mut().push((p, early));
                async move {
                    if p == peer(1) {
                        tokio::time::sleep(queued).await;
                        dispatched.notify();
                        futures::future::pending().await
                    } else {
                        dispatched.notify();
                        Ok::<_, ReadError>(Some(42))
                    }
                }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        assert_eq!(read.await, Ok(Some(42)));
        assert_eq!(start.elapsed(), queued + HEDGE_DELAY);
        assert_eq!(*gets.borrow(), vec![(peer(1), true), (peer(2), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_raced_get_must_stall_before_another_joins_it() {
        // Peer 1 stalls and is raced by peer 2, then misses. Peer 3 joins peer 2
        // only once peer 2 has stalled too, a hedge delay after it was dispatched.
        let gets = RefCell::new(Vec::new());
        let start = tokio::time::Instant::now();
        let read = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1), peer(2), peer(3)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, early, dispatched| {
                gets.borrow_mut().push((p, early));
                dispatched.notify();
                async move {
                    if p == peer(1) {
                        tokio::time::sleep(HEDGE_DELAY + HEDGE_DELAY / 2).await;
                        Ok::<_, ReadError>(None)
                    } else if p == peer(2) {
                        futures::future::pending().await
                    } else {
                        Ok(Some(42))
                    }
                }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        assert_eq!(read.await, Ok(Some(42)));
        assert_eq!(start.elapsed(), HEDGE_DELAY * 2);
        assert_eq!(
            *gets.borrow(),
            vec![(peer(1), true), (peer(2), true), (peer(3), true)]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hint_near_the_target_is_read_after_stale_hints_use_the_allowance() {
        let allowance = ant_protocol::CLOSE_GROUP_SIZE;
        let gets = RefCell::new(Vec::new());
        let read = retrieve_progressive(
            peer(0),
            early_reads(allowance),
            nearest,
            |updates| async move {
                updates.send_replace((200..).take(allowance).map(peer).collect());
                tokio::time::sleep(Duration::from_secs(10)).await;
                updates.send_replace(vec![peer(1)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                async move { Ok::<_, ReadError>((p == peer(1)).then_some(42)) }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        let result = tokio::time::timeout(Duration::from_secs(20), read)
            .await
            .expect("the read waited for discovery to finish");
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(gets.borrow().len(), allowance + 1);
        assert_eq!(gets.borrow().last(), Some(&(peer(1), true)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_far_hint_is_not_read_after_the_allowance_is_used() {
        let allowance = ant_protocol::CLOSE_GROUP_SIZE;
        let gets = RefCell::new(Vec::new());
        let read = retrieve_progressive(
            peer(0),
            early_reads(allowance),
            nearest,
            |updates| async move {
                // A close group of attempted peers is nearer than the late hint.
                updates.send_replace((1..).take(allowance).map(peer).collect());
                tokio::time::sleep(Duration::from_secs(10)).await;
                updates.send_replace(vec![peer(200)]);
                tokio::time::sleep(Duration::from_secs(10)).await;
                ReadCandidates {
                    closest: vec![peer(100)],
                    known: vec![],
                }
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                async move { Ok::<_, ReadError>((p == peer(100)).then_some(42)) }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        assert_eq!(read.await, Ok(Some(42)));
        assert!(!gets.borrow().contains(&(peer(200), true)));
        assert_eq!(gets.borrow().last(), Some(&(peer(100), false)));
    }

    #[tokio::test(start_paused = true)]
    async fn speculative_reads_stop_at_twice_the_allowance() {
        let gets = RefCell::new(Vec::new());
        let read = retrieve_progressive(
            peer(0),
            early_reads(1),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(200)]);
                tokio::time::sleep(Duration::from_secs(1)).await;
                // Each of these may hold the record, but only one fits the bound.
                updates.send_replace(vec![peer(1), peer(2), peer(3)]);
                tokio::time::sleep(Duration::from_secs(1)).await;
                ReadCandidates {
                    closest: vec![peer(1), peer(2), peer(3)],
                    known: vec![],
                }
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                async move { Ok::<_, ReadError>((p == peer(3)).then_some(42)) }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        assert_eq!(read.await, Ok(Some(42)));
        assert_eq!(
            *gets.borrow(),
            vec![
                (peer(200), true),
                (peer(1), true),
                (peer(2), false),
                (peer(3), false)
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn integrity_failure_from_a_hedged_read_remains_fatal() {
        let read = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1), peer(2)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, _, dispatched| async move {
                dispatched.notify();
                if p == peer(1) {
                    futures::future::pending().await
                } else {
                    Err::<Option<()>, _>(ReadError::Integrity)
                }
            },
            |error| *error == ReadError::Transport,
            Vec::new,
            || unreachable!("no peer is refused"),
            tokio::time::sleep,
        );
        let result = tokio::time::timeout(Duration::from_secs(2), read)
            .await
            .expect("integrity failure was not returned");
        assert_eq!(result, Err(ReadError::Integrity));
    }

    #[test]
    fn progressive_verified_read_finishes_before_discovery() {
        let result = futures::executor::block_on(retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                futures::future::pending().await
            },
            |p| *p,
            |_, early, dispatched| async move {
                dispatched.notify();
                assert!(early);
                Ok::<_, ReadError>(Some(42))
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            |_| async {},
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn early_misses_cannot_end_incomplete_discovery() {
        let gets = Cell::new(0);
        let result = retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace((1..=7).map(peer).collect());
                futures::future::pending().await
            },
            |p| *p,
            |_, early, dispatched| {
                dispatched.notify();
                assert!(early);
                gets.set(gets.get() + 1);
                async { Ok::<Option<()>, ReadError>(None) }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            |_| async {},
        )
        .now_or_never();
        assert!(result.is_none());
        assert_eq!(gets.get(), 7);
    }

    #[test]
    fn slow_early_get_is_deduplicated_and_does_not_block_another_holder() {
        let (send, receive) = futures::channel::oneshot::channel();
        let send = RefCell::new(Some(send));
        let receive = RefCell::new(Some(receive));
        let gets = RefCell::new(Vec::new());
        let result = futures::executor::block_on(retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| {
                let receive = receive.borrow_mut().take().unwrap();
                async move {
                    updates.send_replace(vec![peer(1)]);
                    receive.await.unwrap();
                    ReadCandidates {
                        closest: vec![peer(1), peer(2)],
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                let send = &send;
                async move {
                    if p == peer(1) {
                        send.borrow_mut().take().unwrap().send(()).unwrap();
                        futures::future::pending().await
                    } else {
                        Ok::<_, ReadError>(Some(42))
                    }
                }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            |_| async {},
        ));
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(*gets.borrow(), vec![(peer(1), true), (peer(2), false)]);
    }

    #[test]
    fn progressive_early_budget_preserves_ordinary_fallback_and_absence() {
        let (send, receive) = futures::channel::oneshot::channel();
        let send = RefCell::new(Some(send));
        let receive = RefCell::new(Some(receive));
        let gets = RefCell::new(Vec::new());
        let result = futures::executor::block_on(retrieve_progressive(
            peer(0),
            early_reads(2),
            nearest,
            |updates| {
                let receive = receive.borrow_mut().take().unwrap();
                async move {
                    updates.send_replace((1..=7).map(peer).collect());
                    receive.await.unwrap();
                    ReadCandidates {
                        closest: (1..=7).map(peer).collect(),
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |p, early, dispatched| {
                dispatched.notify();
                gets.borrow_mut().push((p, early));
                if p == peer(2) {
                    send.borrow_mut().take().unwrap().send(()).unwrap();
                }
                async { Ok::<Option<()>, ReadError>(None) }
            },
            |_| true,
            Vec::new,
            || unreachable!("no peer is refused"),
            |_| async {},
        ));
        assert_eq!(result, Ok(None));
        let gets = gets.borrow();
        assert_eq!(gets.len(), 7);
        assert_eq!(gets.iter().filter(|(_, early)| *early).count(), 2);
        assert_eq!(gets.iter().map(|(p, _)| p).collect::<HashSet<_>>().len(), 7);
    }

    #[test]
    fn early_integrity_errors_remain_fatal_before_discovery() {
        let result = futures::executor::block_on(retrieve_progressive(
            peer(0),
            early_reads(7),
            nearest,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                futures::future::pending().await
            },
            |p| *p,
            |_, _, dispatched| async move {
                dispatched.notify();
                Err::<Option<()>, _>(ReadError::Integrity)
            },
            |error| *error == ReadError::Transport,
            Vec::new,
            || unreachable!("no peer is refused"),
            |_| async {},
        ));
        assert_eq!(result, Err(ReadError::Integrity));
    }

    #[test]
    fn cached_verified_content_does_not_wait_for_discovery() {
        let result = futures::executor::block_on(race_cached_read(
            Some(async { Ok::<_, ReadError>(Some(42)) }),
            futures::future::pending(),
            |_| false,
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn cached_misses_and_transport_errors_preserve_discovery() {
        for cached in [Ok(None), Err(ReadError::Transport)] {
            let result = futures::executor::block_on(race_cached_read(
                Some(async { cached }),
                async { Ok(Some(42)) },
                |error| *error == ReadError::Transport,
            ));
            assert_eq!(result, Ok(Some(42)));
        }
    }

    #[test]
    fn cached_integrity_errors_remain_fatal() {
        let result = futures::executor::block_on(race_cached_read(
            Some(async { Err::<Option<()>, _>(ReadError::Integrity) }),
            futures::future::pending(),
            |error| *error == ReadError::Transport,
        ));
        assert_eq!(result, Err(ReadError::Integrity));
    }

    #[test]
    fn an_unresponsive_cached_peer_does_not_delay_a_discovered_holder() {
        let result = futures::executor::block_on(race_cached_read(
            Some(futures::future::pending()),
            async { Ok::<_, ReadError>(Some(42)) },
            |_| false,
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn discovered_absence_does_not_discard_a_late_cached_holder() {
        let (send, receive) = futures::channel::oneshot::channel();
        let result = futures::executor::block_on(race_cached_read(
            Some(async { Ok::<_, ReadError>(Some(receive.await.unwrap())) }),
            async {
                send.send(42).unwrap();
                Ok(None)
            },
            |_| false,
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn cached_miss_is_not_an_authoritative_not_found_vote() {
        let rounds = Cell::new(0);
        let discovered = retrieve(
            peer(0),
            || {
                rounds.set(rounds.get() + 1);
                async {
                    ReadCandidates {
                        closest: vec![peer(1)],
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |_| async { Ok::<Option<()>, ReadError>(None) },
            |_| true,
            |_| async {},
        );
        let result = futures::executor::block_on(race_cached_read(
            Some(async { Ok(None) }),
            discovered,
            |_| true,
        ));
        assert_eq!(result, Ok(None));
        assert_eq!(rounds.get(), 2);
    }

    #[test]
    fn fallback_is_sorted_deduplicated_and_bounded() {
        let targets = read_targets(
            ReadCandidates {
                closest: vec![peer(3), peer(1), peer(1)],
                known: (0..30).rev().map(peer).collect(),
            },
            &peer(0),
            |p| *p,
            &mut nearest,
        );
        assert_eq!(&targets[..4], &[peer(1), peer(3), peer(0), peer(2)]);
        assert_eq!(targets.len(), 22);
        assert_eq!(targets.iter().collect::<HashSet<_>>().len(), 22);
    }

    #[test]
    fn uniform_choice_gives_each_close_group_holder_an_equal_share_of_first_reads() {
        const TRIALS: usize = 7_000;
        const TOLERANCE: usize = 300;
        let expected = TRIALS / CLOSE_GROUP_SIZE;
        let mut first_reads = [0_usize; 10];
        for _ in 0..TRIALS {
            let targets = read_targets(
                ReadCandidates {
                    closest: (1..10).map(peer).collect(),
                    known: vec![],
                },
                &peer(0),
                |p| *p,
                &mut uniform_choice,
            );
            first_reads[usize::from(targets[0][0])] += 1;
            // Peers beyond the close group keep nearest-first order.
            assert_eq!(&targets[CLOSE_GROUP_SIZE..], &[peer(8), peer(9)]);
        }
        for (holder, served) in first_reads
            .iter()
            .enumerate()
            .skip(1)
            .take(CLOSE_GROUP_SIZE)
        {
            assert!(
                served.abs_diff(expected) < TOLERANCE,
                "holder {holder} served {served} of {TRIALS} first reads"
            );
        }
        assert_eq!(first_reads[8] + first_reads[9], 0);
    }

    #[test]
    fn early_reads_choose_within_the_known_close_group_then_go_nearest_first() {
        let hints = (1..10).map(peer).collect::<Vec<_>>();
        let offered = RefCell::new(Vec::new());
        let mut farthest = |len: usize| {
            offered.borrow_mut().push(len);
            len - 1
        };
        let attempted = HashSet::new();
        let next = next_early_peer(
            &hints,
            &attempted,
            &peer(0),
            |p| *p,
            |_| true,
            &mut farthest,
        );
        assert_eq!(next, Some(&peer(7)));

        let attempted = [1, 2].map(peer).into_iter().collect::<HashSet<_>>();
        let next = next_early_peer(
            &hints,
            &attempted,
            &peer(0),
            |p| *p,
            |_| true,
            &mut farthest,
        );
        assert_eq!(next, Some(&peer(7)));
        assert_eq!(
            *offered.borrow(),
            vec![CLOSE_GROUP_SIZE, CLOSE_GROUP_SIZE - 2]
        );

        // Once the known close group has been tried, the nearest hint is next
        // without consulting the chooser.
        let attempted = (1..=7).map(peer).collect::<HashSet<_>>();
        let next = next_early_peer(
            &hints,
            &attempted,
            &peer(0),
            |p| *p,
            |_| true,
            &mut farthest,
        );
        assert_eq!(next, Some(&peer(8)));
        assert_eq!(offered.borrow().len(), 2);
    }

    #[test]
    fn an_ineligible_hint_is_never_chosen_over_an_eligible_one() {
        let hints = (1..10).map(peer).collect::<Vec<_>>();
        let attempted = HashSet::new();
        let next = next_early_peer(
            &hints,
            &attempted,
            &peer(0),
            |p| *p,
            |p| *p == peer(1),
            &mut |len: usize| len - 1,
        );
        assert_eq!(next, Some(&peer(1)));
    }

    #[test]
    fn an_out_of_range_choice_stays_within_the_close_group() {
        let mut peers = (1..10).map(peer).collect::<Vec<_>>();
        spread_close_group(&mut peers, &mut |_| usize::MAX);
        let close_group = peers[..CLOSE_GROUP_SIZE].iter().collect::<HashSet<_>>();
        assert_eq!(close_group.len(), CLOSE_GROUP_SIZE);
        assert!((1..=7).all(|id| close_group.contains(&peer(id))));
        assert_eq!(&peers[CLOSE_GROUP_SIZE..], &[peer(8), peer(9)]);
    }

    #[test]
    fn failed_discovery_still_reads_known_holder() {
        let result = futures::executor::block_on(retrieve(
            peer(0),
            || async {
                ReadCandidates {
                    closest: vec![],
                    known: vec![peer(1)],
                }
            },
            |p| *p,
            |_| async { Ok::<_, ()>(Some(42)) },
            |_| true,
            |_| async {},
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn native_retry_rules_include_thin_results_and_remote_failures() {
        for (count, remote_error, expected_rounds) in
            [(3, false, 2), (4, false, 1), (7, true, 2), (0, false, 2)]
        {
            let rounds = Cell::new(0);
            let sleeps = RefCell::new(Vec::new());
            let result = futures::executor::block_on(retrieve(
                peer(0),
                || {
                    rounds.set(rounds.get() + 1);
                    async move {
                        ReadCandidates {
                            closest: (0..count).map(peer).collect(),
                            known: vec![],
                        }
                    }
                },
                |p| *p,
                |_| async move {
                    if remote_error {
                        Err(())
                    } else {
                        Ok(None::<()>)
                    }
                },
                |_| true,
                |delay| {
                    sleeps.borrow_mut().push(delay);
                    async {}
                },
            ));
            assert_eq!(result, Ok(None));
            assert_eq!(rounds.get(), expected_rounds);
            assert_eq!(sleeps.borrow().len(), expected_rounds - 1);
        }
    }

    #[test]
    fn retry_rediscovers_and_can_find_a_new_holder() {
        let rounds = Cell::new(0);
        let result = futures::executor::block_on(retrieve(
            peer(0),
            || {
                rounds.set(rounds.get() + 1);
                let id = rounds.get();
                async move {
                    ReadCandidates {
                        closest: vec![peer(id)],
                        known: vec![],
                    }
                }
            },
            |p| *p,
            |p| async move { Ok::<_, ()>((p == peer(2)).then_some(42)) },
            |_| true,
            |_| async {},
        ));
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(rounds.get(), 2);
    }
}
