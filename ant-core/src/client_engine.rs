//! Runtime-neutral scheduling and session state shared by native and browser clients.

use futures_util::{stream, stream::FuturesUnordered, Stream, StreamExt as _};
#[cfg(any(feature = "browser-wasm", test))]
use std::collections::HashMap;
use std::future::Future;
#[cfg(any(feature = "browser-wasm", test))]
use std::hash::Hash;
use std::time::Duration;
#[cfg(any(feature = "browser-wasm", test))]
use std::time::Instant;

#[cfg_attr(
    all(feature = "browser-wasm", not(feature = "native")),
    allow(dead_code)
)]
#[path = "data/client/adaptive.rs"]
pub(crate) mod adaptive;

/// Maximum combined source-record bytes scheduled for concurrent storage.
///
/// A record is sent to several close-group peers, so its actual wire footprint
/// is larger than its source body. Keeping the shared budget expressed in
/// source bytes lets native QUIC and browser WebRTC use the same conservative
/// scheduling policy without coupling it to either transport.
pub(crate) const STORE_INFLIGHT_BYTE_BUDGET: usize = 64 * 1024 * 1024;

/// Number of whole-record store retries after the first attempt.
pub(crate) const STORE_MAX_RETRIES: u32 = 3;

/// Initial delay for exponential whole-record store retries.
pub(crate) const STORE_RETRY_BASE_DELAY_MS: u64 = 500;

/// Outcome of a quorum operation over an ordered target set.
#[derive(Debug)]
pub(crate) struct QuorumOutcome<T, E> {
    pub(crate) successes: usize,
    pub(crate) failures: Vec<(T, E)>,
    pub(crate) reached: bool,
}

/// Run an operation against the first `required` targets concurrently, using
/// later targets one-for-one as fallbacks when an attempt fails.
///
/// The function returns as soon as quorum is reached and drops any remaining
/// in-flight work. This is the transport-neutral close-group delivery policy
/// shared by native QUIC and browser WebRTC uploads.
pub(crate) async fn quorum_with_fallback<T, F, Fut, V, E>(
    targets: impl IntoIterator<Item = T>,
    required: usize,
    operation: F,
) -> QuorumOutcome<T, E>
where
    T: Clone,
    F: Fn(T) -> Fut,
    Fut: Future<Output = Result<V, E>>,
{
    if required == 0 {
        return QuorumOutcome {
            successes: 0,
            failures: Vec::new(),
            reached: true,
        };
    }

    let mut targets = targets.into_iter();
    let launch = |target: T| {
        let future = operation(target.clone());
        async move { (target, future.await) }
    };
    let mut in_flight = FuturesUnordered::new();
    for target in targets.by_ref().take(required) {
        in_flight.push(launch(target));
    }

    let mut successes = 0usize;
    let mut failures = Vec::new();
    while let Some((target, result)) = in_flight.next().await {
        match result {
            Ok(_) => {
                successes += 1;
                if successes >= required {
                    return QuorumOutcome {
                        successes,
                        failures,
                        reached: true,
                    };
                }
            }
            Err(error) => {
                failures.push((target, error));
                if let Some(fallback) = targets.next() {
                    in_flight.push(launch(fallback));
                }
            }
        }
    }

    QuorumOutcome {
        successes,
        failures,
        reached: false,
    }
}

/// Run all items with a rolling concurrency window whose cap is re-read after
/// every completion.
///
/// Unlike [`bounded_unordered`], this collects every result and therefore fits
/// retry rounds: one failed item does not prevent untouched siblings from
/// being attempted. The cap callback lets callers combine the shared adaptive
/// limiter with a payload-byte ceiling.
pub(crate) async fn rolling_unordered<I, F, Fut, C>(
    items: I,
    mut operation: F,
    current_cap: C,
) -> Vec<Fut::Output>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: Future,
    C: Fn() -> usize,
{
    let mut items = items.into_iter();
    let mut in_flight = FuturesUnordered::new();
    let mut results = Vec::new();
    loop {
        let cap = current_cap().max(1);
        while in_flight.len() < cap {
            match items.next() {
                Some(item) => in_flight.push(operation(item)),
                None => break,
            }
        }
        let Some(result) = in_flight.next().await else {
            break;
        };
        results.push(result);
    }
    results
}

/// Limit concurrent record stores by the shared source-body byte budget.
#[must_use]
pub(crate) fn store_byte_bound(max_record_bytes: usize) -> usize {
    STORE_INFLIGHT_BYTE_BUDGET
        .checked_div(max_record_bytes)
        .map_or(usize::MAX, |bound| bound.max(1))
}

/// Exponential delay for retry round `attempt`, where attempt 1 is the first
/// retry after the initial operation.
#[must_use]
pub(crate) fn store_retry_delay(attempt: u32) -> Duration {
    Duration::from_millis(STORE_RETRY_BASE_DELAY_MS * 2u64.pow(attempt.saturating_sub(1)))
}

/// Run futures with a bounded rolling concurrency window.
///
/// This is the common scheduling primitive behind native upload waves and the
/// browser upload pipeline. Callers retain responsibility for classifying
/// results and adapting the next window's limit.
pub(crate) fn bounded_unordered<I, F>(
    futures: I,
    concurrency: usize,
) -> impl Stream<Item = F::Output>
where
    I: IntoIterator<Item = F>,
    F: Future,
{
    stream::iter(futures).buffer_unordered(concurrency.max(1))
}

#[cfg(any(feature = "browser-wasm", test))]
#[derive(Debug, Clone)]
struct FailureRecord {
    endpoint: String,
    failed_at: Instant,
}

/// Runtime-neutral negative cache for transport endpoints.
///
/// Entries are keyed by authenticated peer identity and also retain the exact
/// endpoint that failed. A peer is immediately eligible again when it
/// republishes a different endpoint, while repeated use of the same dead
/// address is suppressed for the configured cooldown.
#[cfg(any(feature = "browser-wasm", test))]
#[derive(Debug)]
pub(crate) struct EndpointFailureCache<K> {
    cooldown: Duration,
    max_entries: usize,
    entries: HashMap<K, FailureRecord>,
}

#[cfg(any(feature = "browser-wasm", test))]
impl<K> EndpointFailureCache<K>
where
    K: Clone + Eq + Hash,
{
    pub(crate) fn new(cooldown: Duration, max_entries: usize) -> Self {
        Self {
            cooldown,
            max_entries: max_entries.max(1),
            entries: HashMap::new(),
        }
    }

    pub(crate) fn is_suppressed(&mut self, peer: &K, endpoint: &str) -> bool {
        let Some(record) = self.entries.get(peer) else {
            return false;
        };
        if record.endpoint != endpoint || record.failed_at.elapsed() >= self.cooldown {
            self.entries.remove(peer);
            return false;
        }
        true
    }

    pub(crate) fn record_failure(&mut self, peer: K, endpoint: String) {
        if !self.entries.contains_key(&peer) && self.entries.len() >= self.max_entries {
            let oldest = self
                .entries
                .iter()
                .max_by_key(|(_, record)| record.failed_at.elapsed())
                .map(|(peer, _)| peer.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(
            peer,
            FailureRecord {
                endpoint,
                failed_at: Instant::now(),
            },
        );
    }

    pub(crate) fn record_success(&mut self, peer: &K) {
        self.entries.remove(peer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn changed_endpoint_bypasses_failure_cooldown() {
        let mut cache = EndpointFailureCache::new(Duration::from_secs(60), 8);
        cache.record_failure(7_u8, "old".to_string());
        assert!(cache.is_suppressed(&7, "old"));
        assert!(!cache.is_suppressed(&7, "new"));
        assert!(!cache.is_suppressed(&7, "old"));
    }

    #[test]
    fn success_clears_failure() {
        let mut cache = EndpointFailureCache::new(Duration::from_secs(60), 8);
        cache.record_failure(7_u8, "endpoint".to_string());
        cache.record_success(&7);
        assert!(!cache.is_suppressed(&7, "endpoint"));
    }

    #[test]
    fn failure_cache_evicts_oldest_entry_at_capacity() {
        let mut cache = EndpointFailureCache::new(Duration::from_secs(60), 1);
        cache.record_failure(7_u8, "first".to_string());
        cache.record_failure(8_u8, "second".to_string());

        assert!(!cache.is_suppressed(&7, "first"));
        assert!(cache.is_suppressed(&8, "second"));
    }

    #[test]
    fn bounded_scheduler_keeps_all_outputs() {
        let outputs = futures::executor::block_on(async {
            bounded_unordered(
                [
                    futures_util::future::ready(1_u8),
                    futures_util::future::ready(2_u8),
                ],
                0,
            )
            .collect::<Vec<_>>()
            .await
        });
        assert_eq!(outputs.len(), 2);
        assert!(outputs.contains(&1));
        assert!(outputs.contains(&2));
    }

    #[test]
    fn quorum_starts_only_the_required_targets() {
        let launched = Rc::new(Cell::new(0usize));
        let outcome = futures::executor::block_on({
            let launched = Rc::clone(&launched);
            async move {
                quorum_with_fallback(0_u8..7, 4, move |_| {
                    launched.set(launched.get() + 1);
                    futures_util::future::ready(Ok::<(), ()>(()))
                })
                .await
            }
        });

        assert!(outcome.reached);
        assert_eq!(outcome.successes, 4);
        assert!(outcome.failures.is_empty());
        assert_eq!(launched.get(), 4);
    }

    #[test]
    fn quorum_advances_through_fallbacks_after_failures() {
        let launched = Rc::new(Cell::new(0usize));
        let outcome = futures::executor::block_on({
            let launched = Rc::clone(&launched);
            async move {
                quorum_with_fallback(0_u8..7, 4, move |target| {
                    launched.set(launched.get() + 1);
                    futures_util::future::ready(if target < 2 { Err(target) } else { Ok(()) })
                })
                .await
            }
        });

        assert!(outcome.reached);
        assert_eq!(outcome.successes, 4);
        assert_eq!(outcome.failures.len(), 2);
        assert_eq!(launched.get(), 6);
    }

    #[test]
    fn quorum_reports_exhausted_target_set() {
        let outcome = futures::executor::block_on(async {
            quorum_with_fallback(0_u8..7, 4, |target| {
                futures_util::future::ready(Err::<(), _>(target))
            })
            .await
        });

        assert!(!outcome.reached);
        assert_eq!(outcome.successes, 0);
        assert_eq!(outcome.failures.len(), 7);
    }

    #[test]
    fn store_byte_bound_and_retry_schedule_match_native_policy() {
        assert_eq!(store_byte_bound(4 * 1024 * 1024), 16);
        assert_eq!(store_byte_bound(0), usize::MAX);
        assert_eq!(store_retry_delay(1), Duration::from_millis(500));
        assert_eq!(store_retry_delay(2), Duration::from_secs(1));
        assert_eq!(store_retry_delay(3), Duration::from_secs(2));
    }
}
