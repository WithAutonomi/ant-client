//! Physical GET admission, independently bounded by bytes and local processing.
//! Network failures never lower the local processing allowance.
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

// A responsiveness target, not a device throughput assumption. Recovery needs
// several healthy observations so a single small response cannot undo stress.
const PROCESSING_TARGET: Duration = Duration::from_millis(50);
const HEALTHY_RECOVERY_SAMPLES: usize = 8;

struct State {
    active: usize,
    queued: usize,
    processing_cap: usize,
    healthy: usize,
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
                queued: 0,
                processing_cap: capacity,
                healthy: 0,
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
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        desired: impl Fn() -> usize,
    ) -> Result<ReadPermit, &'static str> {
        let mut changed = self.changed.subscribe();
        let queued = QueuedRead::new(Arc::clone(self));
        loop {
            let desired = desired();
            {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let cap = desired.max(1).min(self.capacity).min(state.processing_cap);
                if state.closed {
                    return Err("read admission is closed");
                }
                if state.active < cap {
                    state.active += 1;
                    drop(state);
                    drop(queued);
                    return Ok(ReadPermit {
                        budget: Arc::clone(self),
                    });
                }
            }
            changed
                .changed()
                .await
                .map_err(|_| "read admission is closed")?;
        }
    }

    /// CPU work and event-loop lateness are local pressure. Service/lookup
    /// latency is deliberately not used: a slow remote peer is not a slow CPU.
    pub(crate) fn observe_processing(&self, processing: Duration, desired: usize) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if processing > PROCESSING_TARGET {
            state.processing_cap = (desired.min(state.processing_cap).max(1) / 2).max(1);
            state.healthy = 0;
        } else {
            state.healthy += 1;
            if state.healthy >= HEALTHY_RECOVERY_SAMPLES {
                state.processing_cap = (state.processing_cap + 1).min(self.capacity);
                state.healthy = 0;
            }
        }
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
}
impl Drop for ReadPermit {
    fn drop(&mut self) {
        let mut state = self.budget.state.lock().unwrap_or_else(|e| e.into_inner());
        state.active -= 1;
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
    use futures::{executor::block_on, FutureExt};

    #[test]
    fn admissions_obey_both_the_adaptive_and_byte_limits() {
        let budget = ReadBudget::new(100, 20);
        assert_eq!(budget.limit(256), 5);
        let a = block_on(budget.acquire(|| 2)).unwrap();
        let b = block_on(budget.acquire(|| 2)).unwrap();
        assert!(budget.acquire(|| 2).now_or_never().is_none());
        assert_eq!(budget.state.lock().unwrap().queued, 0);
        drop(a);
        let c = block_on(budget.acquire(|| 2)).unwrap();
        drop((b, c));
        assert_eq!(budget.state.lock().unwrap().active, 0);
    }

    #[test]
    fn processing_pressure_can_reduce_to_one_and_recovers_gradually() {
        let budget = ReadBudget::new(100, 10);
        budget.observe_processing(Duration::from_millis(80), 4);
        assert_eq!(budget.limit(256), 2);
        budget.observe_processing(Duration::from_millis(80), 4);
        assert_eq!(budget.limit(256), 1);
        for _ in 0..7 {
            budget.observe_processing(Duration::from_millis(2), 4);
        }
        assert_eq!(budget.limit(256), 1);
        budget.observe_processing(Duration::from_millis(2), 4);
        assert_eq!(budget.limit(256), 2);
    }

    #[test]
    fn shrinking_does_not_abort_admitted_reads_and_closure_wakes_queue() {
        let budget = ReadBudget::new(100, 10);
        let a = block_on(budget.acquire(|| 2)).unwrap();
        let b = block_on(budget.acquire(|| 2)).unwrap();
        budget.observe_processing(Duration::from_millis(80), 2);
        assert_eq!(budget.state.lock().unwrap().active, 2);
        let mut wait = Box::pin(budget.acquire(|| 2));
        assert!(wait.as_mut().now_or_never().is_none());
        budget.close();
        assert!(block_on(wait).is_err());
        drop((a, b));
        assert_eq!(budget.state.lock().unwrap().active, 0);
        assert_eq!(budget.state.lock().unwrap().queued, 0);
    }
}
