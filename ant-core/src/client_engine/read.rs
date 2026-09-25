//! Transport-neutral native read policy. Adapters supply discovery, GET and sleep;
//! the engine owns ordering, fallback bounds, absence decisions and retry timing.
use futures::future::{select, Either};
use futures::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashSet, future::Future, time::Duration};

pub(crate) const MAX_GET_FALLBACK_PEERS: usize = 20;
pub(crate) const CLOSE_GROUP_RETRY_DELAY: Duration = Duration::from_secs(1);
const EARLY_READ_HEDGE_DELAY: Duration = Duration::from_secs(1);

pub(crate) fn is_authoritative_not_found(not_found: usize, queried: usize) -> bool {
    queried >= ant_protocol::CLOSE_GROUP_MAJORITY && not_found == queried
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

/// Keep primary peers first, ordered by XOR distance, then bounded, deduplicated
/// known peers. Discovery failure suppression must not filter the latter.
pub(crate) fn read_targets<P>(
    candidates: ReadCandidates<P>,
    target: &[u8; 32],
    key: impl Fn(&P) -> [u8; 32],
) -> Vec<P> {
    let mut primary = candidates.closest;
    let mut known = candidates.known;
    primary.sort_by_key(|p| ant_protocol::transport::xor_distance(&key(p), target));
    known.sort_by_key(|p| ant_protocol::transport::xor_distance(&key(p), target));
    let mut seen = HashSet::new();
    primary.retain(|p| seen.insert(key(p)));
    primary.extend(
        known
            .into_iter()
            .filter(|p| seen.insert(key(p)))
            .take(MAX_GET_FALLBACK_PEERS),
    );
    primary
}

/// Discover and read concurrently. Early hints are never absence votes or
/// close-group authority. A stalled early GET may race one other candidate after
/// a delay, on both native and WASM. At most two GETs run, including after discovery.
/// A peer is queried at most once per round, and only the completed final candidate
/// set can establish absence.
pub(crate) async fn retrieve_progressive<P, T, E, D, DF, G, GF, S, SF>(
    target: [u8; 32],
    early_limit: usize,
    discover: D,
    key: impl Fn(&P) -> [u8; 32],
    get: G,
    retryable: impl Fn(&E) -> bool,
    sleep: S,
) -> Result<Option<T>, E>
where
    P: Clone,
    D: Fn(tokio::sync::watch::Sender<Vec<P>>) -> DF,
    DF: Future<Output = ReadCandidates<P>>,
    G: Fn(P, bool) -> GF,
    GF: Future<Output = Result<Option<T>, E>>,
    S: Fn(Duration) -> SF,
    SF: Future<Output = ()>,
{
    for attempt in 0..2 {
        if attempt > 0 {
            sleep(CLOSE_GROUP_RETRY_DELAY).await;
        }
        let (sender, mut updates) = tokio::sync::watch::channel(Vec::new());
        let mut discovery = Box::pin(discover(sender));
        let mut attempted = HashSet::new();
        let not_found = std::sync::Mutex::new(HashSet::new());
        let mut active = FuturesUnordered::new();
        let mut hedge_timer = None;
        let mut hedge_ready = false;
        let mut updates_open = true;
        let candidates = loop {
            if (active.is_empty() || (active.len() == 1 && hedge_ready))
                && attempted.len() < early_limit.min(MAX_GET_FALLBACK_PEERS)
            {
                let next = updates
                    .borrow_and_update()
                    .iter()
                    .filter(|peer| !attempted.contains(&key(peer)))
                    .min_by_key(|peer| ant_protocol::transport::xor_distance(&key(peer), &target))
                    .cloned();
                if let Some(peer) = next {
                    let id = key(&peer);
                    attempted.insert(id);
                    if active.is_empty() {
                        hedge_ready = false;
                        hedge_timer = Some(Box::pin(sleep(EARLY_READ_HEDGE_DELAY)));
                    }
                    let future = get(peer, true);
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
                let hedge = async {
                    match &mut hedge_timer {
                        Some(timer) => timer.await,
                        None => futures::future::pending().await,
                    }
                };
                match select(
                    discovery.as_mut(),
                    Box::pin(select(
                        Box::pin(read),
                        Box::pin(select(Box::pin(changed), Box::pin(hedge))),
                    )),
                )
                .await
                {
                    Either::Left((candidates, _)) => Either::Left(candidates),
                    Either::Right((event, _)) => Either::Right(match event {
                        Either::Left((result, _)) => Either::Left(result),
                        Either::Right((event, _)) => Either::Right(match event {
                            Either::Left((open, _)) => Some(open),
                            Either::Right(_) => None,
                        }),
                    }),
                }
            };
            match event {
                Either::Left(candidates) => break candidates,
                Either::Right(Either::Right(Some(open))) => updates_open = open,
                Either::Right(Either::Right(None)) => {
                    hedge_ready = true;
                    hedge_timer = None;
                }
                Either::Right(Either::Left((id, result))) => match result {
                    Ok(Some(value)) => return Ok(Some(value)),
                    Ok(None) => {
                        not_found
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id);
                    }
                    Err(error) if !retryable(&error) => return Err(error),
                    Err(_) => {}
                },
            }
        };
        let peers = read_targets(candidates, &target, &key);
        let final_ids = peers.iter().map(&key).collect::<HashSet<_>>();
        // Discovery may finish with two early reads still active. Start the
        // ordinary path only once one retires; never create a third wire GET.
        let (slot, slot_available) = futures::channel::oneshot::channel();
        let wait_for_slot = active.len() == 2;
        let pending_early = (!active.is_empty()).then(|| {
            let not_found = &not_found;
            let retryable = &retryable;
            async move {
                let mut slot = Some(slot);
                while let Some((id, result)) = active.next().await {
                    if let Some(slot) = slot.take() {
                        let _ = slot.send(());
                    }
                    match result {
                        Ok(Some(value)) => return Ok(Some(value)),
                        Ok(None) => {
                            not_found
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(id);
                        }
                        Err(error) if !retryable(&error) => return Err(error),
                        Err(_) => {}
                    }
                }
                Ok(None)
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
                match get(peer, false).await {
                    Ok(Some(value)) => return Ok(Some(value)),
                    Ok(None) => {
                        not_found
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(id);
                    }
                    Err(error) if !retryable(&error) => return Err(error),
                    Err(_) => {}
                }
            }
            Ok(None)
        };
        if let Some(value) = race_cached_read(pending_early, ordinary, &retryable).await? {
            return Ok(Some(value));
        }
        let misses = not_found.lock().unwrap_or_else(|e| e.into_inner());
        if is_authoritative_not_found(final_ids.intersection(&misses).count(), final_ids.len()) {
            break;
        }
    }
    Ok(None)
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
        0,
        |_| discover(),
        key,
        |peer, _| get(peer),
        retryable,
        sleep,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    fn peer(id: u8) -> [u8; 32] {
        [id; 32]
    }

    #[derive(Debug, PartialEq)]
    enum ReadError {
        Transport,
        Integrity,
    }

    #[tokio::test(start_paused = true)]
    async fn a_stalled_early_read_can_race_a_new_hint_before_discovery_finishes() {
        let gets = RefCell::new(Vec::new());
        let start = tokio::time::Instant::now();
        let read = retrieve_progressive(
            peer(0),
            7,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                tokio::time::sleep(Duration::from_millis(500)).await;
                updates.send_replace(vec![peer(1), peer(2)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, early| {
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
            tokio::time::sleep,
        );
        let result = tokio::time::timeout(Duration::from_secs(2), read)
            .await
            .expect("hedged read stalled");
        assert_eq!(result, Ok(Some(42)));
        assert_eq!(start.elapsed(), EARLY_READ_HEDGE_DELAY);
        assert_eq!(*gets.borrow(), vec![(peer(1), true), (peer(2), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_completion_does_not_add_a_third_get_to_two_early_reads() {
        use futures::FutureExt;
        let (found, found_rx) = futures::channel::oneshot::channel();
        let found = RefCell::new(Some(found));
        let found_rx = RefCell::new(Some(found_rx));
        let (release, release_rx) = futures::channel::oneshot::channel();
        let release_rx = RefCell::new(Some(release_rx));
        let gets = RefCell::new(Vec::new());
        let future = retrieve_progressive(
            peer(0),
            7,
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
            |p, early| {
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
            tokio::time::sleep,
        );
        futures::pin_mut!(future);
        assert!(future.as_mut().now_or_never().is_none());
        tokio::time::advance(EARLY_READ_HEDGE_DELAY).await;
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
    async fn integrity_failure_from_a_hedged_read_remains_fatal() {
        let read = retrieve_progressive(
            peer(0),
            7,
            |updates| async move {
                updates.send_replace(vec![peer(1), peer(2)]);
                futures::future::pending().await
            },
            |p| *p,
            |p, _| async move {
                if p == peer(1) {
                    futures::future::pending().await
                } else {
                    Err::<Option<()>, _>(ReadError::Integrity)
                }
            },
            |error| *error == ReadError::Transport,
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
            7,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                futures::future::pending().await
            },
            |p| *p,
            |_, early| async move {
                assert!(early);
                Ok::<_, ReadError>(Some(42))
            },
            |_| true,
            |_| async {},
        ));
        assert_eq!(result, Ok(Some(42)));
    }

    #[test]
    fn early_misses_cannot_end_incomplete_discovery() {
        use futures::FutureExt;
        let gets = Cell::new(0);
        let result = retrieve_progressive(
            peer(0),
            7,
            |updates| async move {
                updates.send_replace((1..=7).map(peer).collect());
                futures::future::pending().await
            },
            |p| *p,
            |_, early| {
                assert!(early);
                gets.set(gets.get() + 1);
                async { Ok::<Option<()>, ReadError>(None) }
            },
            |_| true,
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
            7,
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
            |p, early| {
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
            2,
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
            |p, early| {
                gets.borrow_mut().push((p, early));
                if p == peer(2) {
                    send.borrow_mut().take().unwrap().send(()).unwrap();
                }
                async { Ok::<Option<()>, ReadError>(None) }
            },
            |_| true,
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
            7,
            |updates| async move {
                updates.send_replace(vec![peer(1)]);
                futures::future::pending().await
            },
            |p| *p,
            |_, _| async { Err::<Option<()>, _>(ReadError::Integrity) },
            |error| *error == ReadError::Transport,
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
        );
        assert_eq!(&targets[..4], &[peer(1), peer(3), peer(0), peer(2)]);
        assert_eq!(targets.len(), 22);
        assert_eq!(targets.iter().collect::<HashSet<_>>().len(), 22);
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
