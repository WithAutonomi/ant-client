//! Physical GET admission, independently bounded by bytes and local processing.
//! Network failures never lower the local processing allowance.
#[cfg(test)]
use futures::{executor::block_on, FutureExt};
#[cfg(test)]
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;
use web_time::Instant;

// A responsiveness target, not a device throughput assumption. Recovery needs
// several healthy observations so a single small response cannot undo stress.
const PROCESSING_TARGET: Duration = Duration::from_millis(50);
const PROCESSING_WINDOW_SAMPLES: usize = 8;
/// Speculative reads, such as read-ahead, hold at most this fraction (one in N)
/// of the read cap, so reads a caller waits for always have the rest. This
/// includes speculative reads whose caller was cancelled: the transport keeps
/// their permits until their responses are drained.
const SPECULATIVE_SHARE_DIVISOR: usize = 2;

struct State {
    active: usize,
    speculative: usize,
    queued: usize,
    processing_cap: usize,
    processing_started: Option<Instant>,
    processing_time: Duration,
    processing_samples: usize,
    slow_samples: usize,
    closed: bool,
}

pub(crate) struct ReadBudget {
    state: Mutex<State>,
    capacity: usize,
    changed: watch::Sender<()>,
}

impl ReadBudget {
    pub(crate) fn new(max_bytes: usize, reservation_bytes: usize) -> Arc<Self> {
        let capacity = (max_bytes / reservation_bytes.max(1)).max(1);
        let (changed, _) = watch::channel(());
        Arc::new(Self {
            state: Mutex::new(State {
                active: 0,
                speculative: 0,
                queued: 0,
                processing_cap: capacity,
                processing_started: None,
                processing_time: Duration::ZERO,
                processing_samples: 0,
                slow_samples: 0,
                closed: false,
            }),
            capacity,
            changed,
        })
    }

    pub(crate) fn limit(&self, desired: usize) -> usize {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        desired.max(1).min(self.capacity).min(state.processing_cap)
    }

    /// Only admitted physical reads consume reservations. Cancellation drops
    /// both queued admissions and acquired permits without leaking capacity.
    ///
    /// While `speculative()` holds, no caller waits for the read, and it is
    /// admitted only within the speculative share of the cap. It is
    /// re-evaluated while queued, so a read a caller comes to wait for is
    /// admitted as an ordinary one once [`ReadBudget::notify`] wakes the queue.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        desired: impl Fn() -> usize,
        speculative: impl Fn() -> bool,
    ) -> Result<ReadPermit, &'static str> {
        let mut changed = self.changed.subscribe();
        let queued = QueuedRead::new(Arc::clone(self));
        loop {
            let desired = desired();
            let speculative = speculative();
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let cap = desired.max(1).min(self.capacity).min(state.processing_cap);
                if state.closed {
                    return Err("read admission is closed");
                }
                let within_share =
                    !speculative || state.speculative < cap / SPECULATIVE_SHARE_DIVISOR;
                if state.active < cap && within_share {
                    state.processing_started.get_or_insert_with(Instant::now);
                    state.active += 1;
                    state.speculative += usize::from(speculative);
                    drop(state);
                    drop(queued);
                    return Ok(ReadPermit {
                        budget: Arc::clone(self),
                        speculative,
                    });
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| "read admission is closed")?;
        }
    }

    /// Wake queued reads to re-evaluate whether they are speculative.
    pub(crate) fn notify(&self) {
        self.changed.send_replace(());
    }

    /// Reduce concurrency only for sustained CPU occupation with responsiveness
    /// stalls. A single slow response cannot get cheaper by serializing network
    /// waits, and timer clamping alone is not evidence of CPU saturation.
    pub(crate) fn observe_processing(
        &self,
        processing: Duration,
        event_loop_delay: Duration,
        desired: usize,
    ) {
        self.observe_processing_at(processing, event_loop_delay, desired, Instant::now());
    }

    fn observe_processing_at(
        &self,
        processing: Duration,
        event_loop_delay: Duration,
        desired: usize,
        now: Instant,
    ) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let started = *state.processing_started.get_or_insert(now);
        state.processing_time = state.processing_time.saturating_add(processing);
        state.processing_samples += 1;
        if processing.max(event_loop_delay) > PROCESSING_TARGET {
            state.slow_samples += 1;
        }
        if state.processing_samples < PROCESSING_WINDOW_SAMPLES {
            return;
        }
        // Reserve at least half the event loop for other browser work when GET
        // processing is actually busy. Eight observations prevent one burst
        // from repeatedly halving the cap using the same in-flight cohort.
        let busy = state.processing_time > now.saturating_duration_since(started) / 2;
        if busy && state.slow_samples >= 2 {
            state.processing_cap = (desired.min(state.processing_cap).max(1) / 2).max(1);
        } else {
            state.processing_cap = (state.processing_cap + 1).min(self.capacity);
        }
        state.processing_started = (state.active > 1).then_some(now);
        state.processing_time = Duration::ZERO;
        state.processing_samples = 0;
        state.slow_samples = 0;
        drop(state);
        self.changed.send_replace(());
    }

    pub(crate) fn close(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).closed = true;
        self.changed.send_replace(());
    }
}

pub(crate) struct ReadPermit {
    budget: Arc<ReadBudget>,
    speculative: bool,
}
impl Drop for ReadPermit {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
        state.speculative -= usize::from(self.speculative);
        drop(state);
        self.budget.changed.send_replace(());
    }
}

struct QueuedRead {
    budget: Arc<ReadBudget>,
}
impl QueuedRead {
    fn new(budget: Arc<ReadBudget>) -> Self {
        budget
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued += 1;
        Self { budget }
    }
}
impl Drop for QueuedRead {
    fn drop(&mut self) {
        self.budget
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admissions_obey_both_the_adaptive_and_byte_limits() {
        let budget = ReadBudget::new(100, 20);
        assert_eq!(budget.limit(256), 5);
        let a = block_on(budget.acquire(|| 2, || false)).unwrap();
        let b = block_on(budget.acquire(|| 2, || false)).unwrap();
        assert!(budget.acquire(|| 2, || false).now_or_never().is_none());
        assert_eq!(budget.state.lock().unwrap().queued, 0);
        drop(a);
        let c = block_on(budget.acquire(|| 2, || false)).unwrap();
        drop((b, c));
        assert_eq!(budget.state.lock().unwrap().active, 0);
    }

    fn processing_window(
        budget: &ReadBudget,
        cpu_ms: u64,
        interval_ms: u64,
        delay_ms: u64,
        desired: usize,
    ) {
        let started = Instant::now();
        budget.state.lock().unwrap().processing_started = Some(started);
        for i in 1..=PROCESSING_WINDOW_SAMPLES {
            budget.observe_processing_at(
                Duration::from_millis(cpu_ms),
                Duration::from_millis(delay_ms),
                desired,
                started + Duration::from_millis(interval_ms * i as u64),
            );
        }
    }

    #[test]
    fn processing_pressure_can_reduce_to_one_and_recovers_gradually() {
        let budget = ReadBudget::new(100, 10);
        processing_window(&budget, 80, 100, 0, 4);
        assert_eq!(budget.limit(256), 2);
        processing_window(&budget, 80, 100, 0, 4);
        assert_eq!(budget.limit(256), 1);
        for _ in 0..7 {
            budget.observe_processing(Duration::from_millis(2), Duration::ZERO, 4);
        }
        assert_eq!(budget.limit(256), 1);
        budget.observe_processing(Duration::from_millis(2), Duration::ZERO, 4);
        assert_eq!(budget.limit(256), 2);
    }

    #[test]
    fn isolated_slow_responses_and_timer_clamping_do_not_serialize_network_waits() {
        let budget = ReadBudget::new(100, 10);
        processing_window(&budget, 80, 1000, 0, 4);
        assert_eq!(
            budget.limit(4),
            4,
            "8% CPU occupancy leaves room for concurrent I/O"
        );
        processing_window(&budget, 2, 1000, 1000, 4);
        assert_eq!(budget.limit(4), 4, "a clamped timer is not CPU saturation");
    }

    #[test]
    fn shrinking_does_not_abort_admitted_reads_and_closure_wakes_queue() {
        let budget = ReadBudget::new(100, 10);
        let a = block_on(budget.acquire(|| 2, || false)).unwrap();
        let b = block_on(budget.acquire(|| 2, || false)).unwrap();
        processing_window(&budget, 80, 100, 0, 2);
        assert_eq!(budget.state.lock().unwrap().active, 2);
        let mut wait = Box::pin(budget.acquire(|| 2, || false));
        assert!(wait.as_mut().now_or_never().is_none());
        budget.close();
        assert!(block_on(wait).is_err());
        drop((a, b));
        assert_eq!(budget.state.lock().unwrap().active, 0);
        assert_eq!(budget.state.lock().unwrap().queued, 0);
    }

    #[test]
    fn speculative_reads_leave_the_rest_of_the_cap_to_awaited_reads() {
        let budget = ReadBudget::new(100, 10);
        let speculative = (0..2)
            .map(|_| block_on(budget.acquire(|| 4, || true)).unwrap())
            .collect::<Vec<_>>();
        assert!(budget.acquire(|| 4, || true).now_or_never().is_none());
        let awaited = (0..2)
            .map(|_| block_on(budget.acquire(|| 4, || false)).unwrap())
            .collect::<Vec<_>>();
        assert!(budget.acquire(|| 4, || false).now_or_never().is_none());
        drop(awaited);
        assert!(budget.acquire(|| 4, || true).now_or_never().is_none());
        drop(speculative);
        let state = budget.state.lock().unwrap();
        assert_eq!((state.active, state.speculative, state.queued), (0, 0, 0));
    }

    #[test]
    fn a_queued_speculative_read_is_admitted_once_a_caller_waits_for_it() {
        let budget = ReadBudget::new(100, 10);
        let _held = block_on(budget.acquire(|| 2, || true)).unwrap();
        let awaited = Cell::new(false);
        let mut wait = Box::pin(budget.acquire(|| 2, || !awaited.get()));
        assert!(wait.as_mut().now_or_never().is_none());
        awaited.set(true);
        budget.notify();
        let permit = block_on(wait).unwrap();
        assert!(!permit.speculative);
        assert_eq!(budget.state.lock().unwrap().speculative, 1);
    }

    #[test]
    fn under_a_cap_of_one_only_awaited_reads_are_admitted() {
        let budget = ReadBudget::new(100, 10);
        assert!(budget.acquire(|| 1, || true).now_or_never().is_none());
        assert!(block_on(budget.acquire(|| 1, || false)).is_ok());
    }
}
