//! Concurrent callers for one key share a single operation.
//!
//! A caller that finds its key already in flight waits for that operation's
//! result instead of starting its own. Nothing is kept once the operation
//! ends, so a later caller starts afresh; keeping results is a cache's job.

use futures::channel::oneshot;
use futures::future::{FutureExt as _, Shared};
use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

/// Operations in flight, by key.
pub(crate) struct SingleFlight<K, T> {
    flights: Mutex<HashMap<K, Flight<T>>>,
}

struct Flight<T> {
    result: Shared<oneshot::Receiver<Arc<T>>>,
    /// Whether any caller sharing the operation asked for it to be observed.
    observe: bool,
}

/// How a caller came by its result.
pub(crate) enum Flown<T> {
    /// This caller ran the operation. `observe` is whether it, or any caller
    /// that shared the operation, asked for the operation to be observed, so
    /// one operation is observed at most once.
    Led { value: T, observe: bool },
    /// Another caller ran the operation; this is a copy of its result.
    Joined(T),
}

impl<K, T> Default for SingleFlight<K, T> {
    fn default() -> Self {
        Self {
            flights: Mutex::new(HashMap::new()),
        }
    }
}

impl<K: Hash + Eq + Clone, T> SingleFlight<K, T> {
    /// Run `operation` for `key`, or wait for the caller already running it.
    ///
    /// Waiters receive `copy` of the result, so a failure fails them all with
    /// the same error. If the caller running the operation is dropped, its
    /// waiters do not fail: one of them runs the operation in its place.
    pub(crate) async fn run<Fut>(
        &self,
        key: K,
        observe: bool,
        operation: impl FnOnce() -> Fut,
        copy: impl Fn(&T) -> T,
    ) -> Flown<T>
    where
        Fut: Future<Output = T>,
    {
        let sender = loop {
            let result = {
                let mut flights = lock(&self.flights);
                match flights.get_mut(&key) {
                    Some(flight) => {
                        flight.observe |= observe;
                        flight.result.clone()
                    }
                    None => {
                        let (sender, receiver) = oneshot::channel();
                        let flight = Flight {
                            result: receiver.shared(),
                            observe,
                        };
                        flights.insert(key.clone(), flight);
                        break sender;
                    }
                }
            };
            // A dropped leader removes its flight before its sender goes, so a
            // waiter woken by the cancellation finds the flight gone and may
            // lead in its place.
            if let Ok(result) = result.await {
                return Flown::Joined(copy(&result));
            }
        };
        let lead = Lead {
            flights: &self.flights,
            key: &key,
            sender: Some(sender),
        };
        let value = operation().await;
        let observe = lead.finish(&value, copy);
        Flown::Led { value, observe }
    }

    #[cfg(test)]
    fn in_flight(&self) -> usize {
        lock(&self.flights).len()
    }
}

fn lock<K, T>(flights: &Mutex<HashMap<K, Flight<T>>>) -> MutexGuard<'_, HashMap<K, Flight<T>>> {
    flights.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The running caller's claim on its flight. Only the leader removes its
/// flight, whether the operation completes or the leader is dropped.
struct Lead<'a, K: Hash + Eq, T> {
    flights: &'a Mutex<HashMap<K, Flight<T>>>,
    key: &'a K,
    sender: Option<oneshot::Sender<Arc<T>>>,
}

impl<K: Hash + Eq, T> Lead<'_, K, T> {
    /// Remove the flight and return whether any caller asked to observe it.
    fn remove(&self) -> bool {
        lock(self.flights)
            .remove(self.key)
            .is_some_and(|flight| flight.observe)
    }

    /// Hand waiters a copy of `value`. Callers arriving from now on start a
    /// new operation.
    fn finish(mut self, value: &T, copy: impl Fn(&T) -> T) -> bool {
        let observe = self.remove();
        if let Some(sender) = self.sender.take() {
            // With no waiters, the removed flight held the only receiver.
            if !sender.is_canceled() {
                let _ = sender.send(Arc::new(copy(value)));
            }
        }
        observe
    }
}

impl<K: Hash + Eq, T> Drop for Lead<'_, K, T> {
    fn drop(&mut self) {
        // Dropped mid-operation: remove the flight, then let the sender go so
        // waiters wake to find it gone and one of them leads.
        if self.sender.is_some() {
            self.remove();
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use futures::future::join_all;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    type Flights = SingleFlight<u8, Result<u32, String>>;

    fn copy(result: &Result<u32, String>) -> Result<u32, String> {
        result.clone()
    }

    fn value(flown: Flown<Result<u32, String>>) -> Result<u32, String> {
        match flown {
            Flown::Led { value, .. } | Flown::Joined(value) => value,
        }
    }

    /// An operation that counts its runs and finishes when `gate` opens.
    async fn gated(
        runs: &AtomicUsize,
        gate: &Notify,
        result: Result<u32, String>,
    ) -> Result<u32, String> {
        runs.fetch_add(1, Ordering::SeqCst);
        gate.notified().await;
        result
    }

    #[tokio::test]
    async fn concurrent_callers_of_one_key_share_one_operation() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        let gate = Notify::new();
        let calls =
            join_all((0..8).map(|_| flights.run(1, false, || gated(&runs, &gate, Ok(7)), copy)));
        let open = async {
            tokio::task::yield_now().await;
            gate.notify_waiters();
        };
        let (results, ()) = tokio::join!(calls, open);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        let led = results
            .iter()
            .filter(|flown| matches!(flown, Flown::Led { .. }))
            .count();
        assert_eq!(led, 1);
        assert!(results.into_iter().all(|flown| value(flown) == Ok(7)));
        assert_eq!(flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn different_keys_do_not_share() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        let results = join_all((0..3).map(|key| {
            let runs = &runs;
            flights.run(
                key,
                false,
                move || async move {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Ok(u32::from(key))
                },
                copy,
            )
        }))
        .await;
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        let values: Vec<_> = results.into_iter().map(value).collect();
        assert_eq!(values, vec![Ok(0), Ok(1), Ok(2)]);
    }

    #[tokio::test]
    async fn a_failure_fails_every_waiter_with_the_same_error() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        let gate = Notify::new();
        let calls = join_all((0..4).map(|_| {
            flights.run(
                1,
                false,
                || gated(&runs, &gate, Err("timeout".into())),
                copy,
            )
        }));
        let open = async {
            tokio::task::yield_now().await;
            gate.notify_waiters();
        };
        let (results, ()) = tokio::join!(calls, open);
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert!(results
            .into_iter()
            .all(|flown| value(flown) == Err("timeout".into())));
    }

    #[tokio::test]
    async fn a_finished_operation_is_not_reused() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        for _ in 0..2 {
            let flown = flights
                .run(
                    1,
                    false,
                    || async {
                        runs.fetch_add(1, Ordering::SeqCst);
                        Ok(1)
                    },
                    copy,
                )
                .await;
            assert!(matches!(flown, Flown::Led { .. }));
        }
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_waiter_takes_over_when_the_leader_is_dropped() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        let gate = Notify::new();
        let mut leader = Box::pin(flights.run(1, false, || gated(&runs, &gate, Ok(1)), copy));
        assert!(futures::poll!(leader.as_mut()).is_pending());
        let mut waiter = Box::pin(flights.run(1, true, || gated(&runs, &gate, Ok(2)), copy));
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        drop(leader);
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        assert_eq!(runs.load(Ordering::SeqCst), 2, "the waiter ran it again");
        gate.notify_waiters();
        match waiter.await {
            Flown::Led { value, observe } => {
                assert_eq!(value, Ok(2));
                assert!(observe);
            }
            Flown::Joined(_) => panic!("the waiter should have led"),
        }
        assert_eq!(flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn dropping_every_caller_leaves_nothing_in_flight() {
        let flights = Flights::default();
        let runs = AtomicUsize::new(0);
        let gate = Notify::new();
        let mut leader = Box::pin(flights.run(1, false, || gated(&runs, &gate, Ok(1)), copy));
        let mut waiter = Box::pin(flights.run(1, false, || gated(&runs, &gate, Ok(1)), copy));
        assert!(futures::poll!(leader.as_mut()).is_pending());
        assert!(futures::poll!(waiter.as_mut()).is_pending());
        drop(leader);
        drop(waiter);
        assert_eq!(flights.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_shared_operation_is_observed_if_any_caller_asks() {
        for (leader_observes, waiter_observes) in
            [(false, false), (false, true), (true, false), (true, true)]
        {
            let flights = Flights::default();
            let runs = AtomicUsize::new(0);
            let gate = Notify::new();
            let mut leader =
                Box::pin(flights.run(1, leader_observes, || gated(&runs, &gate, Ok(1)), copy));
            assert!(futures::poll!(leader.as_mut()).is_pending());
            let mut waiter =
                Box::pin(flights.run(1, waiter_observes, || gated(&runs, &gate, Ok(1)), copy));
            assert!(futures::poll!(waiter.as_mut()).is_pending());
            gate.notify_waiters();
            match leader.await {
                Flown::Led { observe, .. } => {
                    assert_eq!(observe, leader_observes || waiter_observes);
                }
                Flown::Joined(_) => panic!("the first caller should lead"),
            }
            assert!(matches!(waiter.await, Flown::Joined(Ok(1))));
            assert_eq!(runs.load(Ordering::SeqCst), 1);
        }
    }
}
