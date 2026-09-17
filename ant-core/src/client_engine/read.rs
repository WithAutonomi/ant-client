//! Transport-neutral native read policy. Adapters supply discovery, GET and sleep;
//! the engine owns ordering, fallback bounds, absence decisions and retry timing.
use futures::future::{select, Either};
use std::{collections::HashSet, future::Future, time::Duration};

pub(crate) const MAX_GET_FALLBACK_PEERS: usize = 20;
pub(crate) const CLOSE_GROUP_RETRY_DELAY: Duration = Duration::from_secs(1);

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

/// Errors from GET are classified by the adapter; invalid local input remains
/// fatal, while transport/remote errors allow another replica to answer.
pub(crate) async fn retrieve<P, T, E, D, DF, G, GF, S, SF>(
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
    for attempt in 0..2 {
        if attempt > 0 {
            sleep(CLOSE_GROUP_RETRY_DELAY).await;
        }
        let peers = read_targets(discover().await, &target, &key);
        let queried = peers.len();
        let mut not_found = 0;
        for peer in peers {
            match get(peer).await {
                Ok(Some(value)) => return Ok(Some(value)),
                Ok(None) => not_found += 1,
                Err(error) if !retryable(&error) => return Err(error),
                Err(_) => {}
            }
        }
        if is_authoritative_not_found(not_found, queried) {
            break;
        }
    }
    Ok(None)
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
