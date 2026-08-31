//! Runtime-neutral scheduling and session state shared by native and browser clients.

use futures_util::{stream, Stream, StreamExt as _};
#[cfg(any(feature = "browser-wasm", test))]
use std::collections::HashMap;
use std::future::Future;
#[cfg(any(feature = "browser-wasm", test))]
use std::hash::Hash;
#[cfg(any(feature = "browser-wasm", test))]
use std::time::{Duration, Instant};

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
}
