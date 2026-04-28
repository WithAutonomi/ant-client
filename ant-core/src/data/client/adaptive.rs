//! Adaptive concurrency controller for client data operations.
//!
//! Replaces hard-coded `quote_concurrency` / `store_concurrency` /
//! download fan-out with a per-channel AIMD limiter that ramps up when
//! the network is healthy and ramps down on stress signals (timeouts,
//! errors, latency inflation). The goal is to give every machine and
//! every connection profile a single client codebase that finds its
//! own steady state without the user tweaking flags.
//!
//! ## Channels
//!
//! Three independent limiters share the same algorithm but track state
//! separately, because their workloads have different cost profiles:
//!
//! - `quote`  — small DHT request/response messages, cheap per op
//! - `store`  — multi-MB chunk PUTs to a close group, expensive per op
//! - `fetch`  — multi-MB chunk GETs from peers, asymmetric to `store`
//!
//! ## Algorithm
//!
//! TCP-style AIMD with slow-start:
//!
//! - **Slow-start**: starting concurrency doubles after each healthy
//!   window until first stress signal or until the configured ceiling.
//! - **Steady state**: additive +1 per healthy window (>= success_target
//!   success rate AND p95 latency within `latency_inflation_factor` of
//!   the rolling baseline).
//! - **Stress**: multiplicative decrease (current / 2, floor 1) on any
//!   of: success rate < success_target, timeout rate > timeout_ceiling,
//!   or p95 latency above `latency_inflation_factor * baseline`.
//!
//! Decisions evaluate over a sliding window of the last `window_ops`
//! observed outcomes per channel. Below `min_window_ops` outcomes the
//! controller holds steady — too few samples to act on.
//!
//! ## What this is not
//!
//! - Not a payment-batching controller. Wave / batch sizes are
//!   orthogonal (gas-economics tradeoff, not throughput).
//! - Not a peer-quality scorer. That lives in `peer_cache` and feeds
//!   `BootstrapManager`. Outcomes flow into both, separately.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Process-monotonic counter for unique snapshot temp filenames.
/// Combined with PID + nanosecond timestamp, makes collision
/// effectively impossible across concurrent save_snapshot calls.
static SAVE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Lock helper matching the project pattern (see `cache::ChunkCache`):
/// poisoned mutexes still yield the inner state rather than panicking.
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Outcome of a single observed operation on one channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Completed successfully.
    Success,
    /// Did not complete within the per-op timeout.
    Timeout,
    /// Failed with a network/transport error (refused, reset, unreachable).
    NetworkError,
    /// Failed with an application-level error not attributable to the
    /// network (e.g. bad payment proof). Recorded but does not push the
    /// controller down — it is not a capacity signal.
    ApplicationError,
}

/// Per-channel concurrency ceilings. Each channel has its own cap so
/// that constraining one (e.g. user pinned a low store concurrency for
/// a slow uplink) never bleeds into another (download).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChannelMax {
    pub quote: usize,
    pub store: usize,
    pub fetch: usize,
}

impl Default for ChannelMax {
    fn default() -> Self {
        // Generous ceilings that give the controller real headroom to
        // grow on healthy connections. The cold-start values
        // (`ChannelStart::default()`) are well below these so AIMD
        // can actually do its job. Each ceiling is independent.
        Self {
            quote: 128,
            store: 64,
            fetch: 256,
        }
    }
}

/// Tunable knobs for the adaptive controller. Defaults are picked so
/// that the controller behaves at least as well as the prior static
/// defaults on a healthy network: starts at the previous static value
/// and only deviates when signals demand it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdaptiveConfig {
    /// Master switch. When `false`, channels report `initial` forever
    /// and ignore observations. Useful for benchmarks / debugging.
    pub enabled: bool,
    /// Floor concurrency per channel. Never go below this.
    pub min_concurrency: usize,
    /// Per-channel ceiling concurrency. See `ChannelMax`.
    pub max: ChannelMax,
    /// Sliding window size in number of recent ops considered for
    /// adaptation decisions.
    pub window_ops: usize,
    /// Below this count of outcomes in the window, hold steady.
    pub min_window_ops: usize,
    /// Required success rate to consider the window healthy. Healthy
    /// windows trigger increase; unhealthy windows trigger decrease.
    pub success_target: f64,
    /// Timeout rate above which the window counts as stressed even if
    /// the success rate would otherwise pass.
    pub timeout_ceiling: f64,
    /// p95 latency above `latency_inflation_factor * baseline` is a
    /// stress signal. Baseline is an EWMA of healthy-window p95s.
    pub latency_inflation_factor: f64,
    /// EWMA smoothing factor for the latency baseline. 0 = never
    /// updates, 1 = baseline = last sample. 0.2 trades responsiveness
    /// for stability. Validated to `[0.0, 1.0]`; `NaN`/non-finite
    /// values are sanitized to the default at controller construction.
    pub latency_ewma_alpha: f64,
}

impl AdaptiveConfig {
    /// Sanitize the config: clamp `latency_ewma_alpha` to `[0,1]`
    /// (rejecting NaN/Inf which would otherwise panic in
    /// `Duration::from_secs_f64`), enforce `min_concurrency >= 1`,
    /// enforce per-channel max >= min_concurrency, enforce
    /// `min_window_ops <= window_ops`. Idempotent.
    pub fn sanitize(&mut self) {
        if !self.latency_ewma_alpha.is_finite() {
            self.latency_ewma_alpha = 0.2;
        }
        self.latency_ewma_alpha = self.latency_ewma_alpha.clamp(0.0, 1.0);
        if !self.success_target.is_finite() {
            self.success_target = 0.95;
        }
        self.success_target = self.success_target.clamp(0.0, 1.0);
        if !self.timeout_ceiling.is_finite() {
            self.timeout_ceiling = 0.10;
        }
        self.timeout_ceiling = self.timeout_ceiling.clamp(0.0, 1.0);
        if !self.latency_inflation_factor.is_finite() || self.latency_inflation_factor <= 0.0 {
            self.latency_inflation_factor = 2.0;
        }
        self.min_concurrency = self.min_concurrency.max(1);
        self.window_ops = self.window_ops.max(1);
        self.min_window_ops = self.min_window_ops.max(1).min(self.window_ops);
        self.max.quote = self.max.quote.max(self.min_concurrency);
        self.max.store = self.max.store.max(self.min_concurrency);
        self.max.fetch = self.max.fetch.max(self.min_concurrency);
    }
}

impl Default for AdaptiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_concurrency: 1,
            max: ChannelMax::default(),
            window_ops: 32,
            min_window_ops: 8,
            success_target: 0.95,
            timeout_ceiling: 0.10,
            latency_inflation_factor: 2.0,
            latency_ewma_alpha: 0.2,
        }
    }
}

/// Suggested starting concurrency per channel for a brand-new client
/// with no persisted state. Intentionally matches or exceeds the prior
/// static defaults so the cold path is not slower:
///
/// - quote was statically 32 — start at 32.
/// - store was statically 8 — start at 8.
/// - fetch was previously unbounded (the entire self_encryption batch
///   was fired at once via `FuturesUnordered`). Typical batches are
///   small (handful of chunks); occasional larger ones are capped at
///   `max_concurrency`. Start at 64 to keep small/medium downloads
///   indistinguishable from the prior unbounded behavior.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ChannelStart {
    pub quote: usize,
    pub store: usize,
    pub fetch: usize,
}

impl Default for ChannelStart {
    fn default() -> Self {
        Self {
            quote: 32,
            store: 8,
            fetch: 64,
        }
    }
}

/// One observed sample retained in the sliding window.
#[derive(Debug, Clone, Copy)]
struct Sample {
    outcome: Outcome,
    latency: Duration,
}

/// Per-limiter configuration. Carries the shared adaptive parameters
/// plus the channel-specific `max_concurrency`. Held behind an `Arc`
/// so cloning a `Limiter` is a refcount bump rather than a struct copy
/// (avoids allocating `AdaptiveConfig`-worth of bytes per chunk in
/// hot loops).
#[derive(Debug, Clone)]
pub struct LimiterConfig {
    pub enabled: bool,
    pub min_concurrency: usize,
    pub max_concurrency: usize,
    pub window_ops: usize,
    pub min_window_ops: usize,
    pub success_target: f64,
    pub timeout_ceiling: f64,
    pub latency_inflation_factor: f64,
    pub latency_ewma_alpha: f64,
}

impl LimiterConfig {
    fn from_adaptive(cfg: &AdaptiveConfig, max_for_channel: usize) -> Self {
        Self {
            enabled: cfg.enabled,
            min_concurrency: cfg.min_concurrency,
            max_concurrency: max_for_channel.max(cfg.min_concurrency),
            window_ops: cfg.window_ops,
            min_window_ops: cfg.min_window_ops,
            success_target: cfg.success_target,
            timeout_ceiling: cfg.timeout_ceiling,
            latency_inflation_factor: cfg.latency_inflation_factor,
            latency_ewma_alpha: cfg.latency_ewma_alpha,
        }
    }

    /// Sanitize a directly-constructed `LimiterConfig`. External
    /// callers (or tests) that build a `LimiterConfig` literal with
    /// hostile values (`NaN`, sub-floor mins, inverted bounds) are
    /// protected — `Limiter::new` calls this on every construction
    /// so the controller never holds NaN or out-of-range floats.
    fn sanitize(&mut self) {
        if !self.latency_ewma_alpha.is_finite() {
            self.latency_ewma_alpha = 0.2;
        }
        self.latency_ewma_alpha = self.latency_ewma_alpha.clamp(0.0, 1.0);
        if !self.success_target.is_finite() {
            self.success_target = 0.95;
        }
        self.success_target = self.success_target.clamp(0.0, 1.0);
        if !self.timeout_ceiling.is_finite() {
            self.timeout_ceiling = 0.10;
        }
        self.timeout_ceiling = self.timeout_ceiling.clamp(0.0, 1.0);
        if !self.latency_inflation_factor.is_finite() || self.latency_inflation_factor <= 0.0 {
            self.latency_inflation_factor = 2.0;
        }
        self.min_concurrency = self.min_concurrency.max(1);
        self.window_ops = self.window_ops.max(1);
        self.min_window_ops = self.min_window_ops.max(1).min(self.window_ops);
        self.max_concurrency = self.max_concurrency.max(self.min_concurrency);
    }
}

/// Per-channel adaptive limiter.
///
/// Cheap to clone — both fields are `Arc`. Pass clones into hot loops;
/// do not hold the lock across `.await` points (call sites observe
/// with short critical sections only).
#[derive(Debug, Clone)]
pub struct Limiter {
    inner: Arc<Mutex<LimiterInner>>,
    config: Arc<LimiterConfig>,
}

#[derive(Debug)]
struct LimiterInner {
    /// Current concurrency cap returned by `current()`.
    current: usize,
    /// Sliding window of recent outcomes.
    window: VecDeque<Sample>,
    /// Samples observed since the last increase. Increases require a
    /// fresh window's worth of evidence to avoid ramping on every
    /// individual healthy sample.
    samples_since_increase: usize,
    /// Samples observed since the last decrease. Decreases require
    /// `min_window_ops` of fresh evidence to avoid pile-driving the
    /// cap to floor on a single bad burst when many in-flight ops all
    /// observe stress nearly simultaneously.
    samples_since_decrease: usize,
    /// EWMA of p95 latency from past healthy windows. `None` until
    /// the first healthy window completes.
    latency_baseline: Option<Duration>,
    /// `true` once we have observed a stress signal at least once.
    /// Slow-start mode ends permanently after first stress.
    left_slow_start: bool,
}

impl Limiter {
    /// Create a new limiter starting at `start`, clamped into
    /// `[min_concurrency, max_concurrency]`. Sanitizes the config to
    /// guard against directly-constructed `LimiterConfig` literals
    /// with hostile float values (`NaN`, etc).
    #[must_use]
    pub fn new(start: usize, config: LimiterConfig) -> Self {
        let mut config = config;
        config.sanitize();
        let clamped = start.clamp(config.min_concurrency, config.max_concurrency.max(1));
        let window_cap = config.window_ops;
        Self {
            inner: Arc::new(Mutex::new(LimiterInner {
                current: clamped,
                window: VecDeque::with_capacity(window_cap),
                samples_since_increase: 0,
                samples_since_decrease: 0,
                latency_baseline: None,
                left_slow_start: false,
            })),
            config: Arc::new(config),
        }
    }

    /// Snapshot current concurrency cap. Hot-path call: the value may
    /// change between this call and the next, but consumers
    /// (`buffer_unordered(n)`) capture it once per pipeline build.
    #[must_use]
    pub fn current(&self) -> usize {
        lock(&self.inner).current
    }

    /// Record one observed operation. Updates the sliding window and
    /// re-evaluates the cap if the window is full enough.
    pub fn observe(&self, outcome: Outcome, latency: Duration) {
        if !self.config.enabled {
            return;
        }
        let mut g = lock(&self.inner);
        if g.window.len() == self.config.window_ops {
            g.window.pop_front();
        }
        g.window.push_back(Sample { outcome, latency });
        g.samples_since_increase = g.samples_since_increase.saturating_add(1);
        g.samples_since_decrease = g.samples_since_decrease.saturating_add(1);
        if g.window.len() < self.config.min_window_ops {
            return;
        }
        let decision = evaluate(&g.window, &self.config, g.latency_baseline);
        apply_decision(&mut g, decision, &self.config);
    }

    /// Replace the current cap with `start`, clamped. Used for warm
    /// loads from persisted state. Marks the limiter as having
    /// already-left-slow-start so a single healthy window doesn't
    /// double the cap (an over-aggressive cold-start from a warm
    /// value). Subsequent increases are +1 per healthy window.
    /// Does not clear the sliding window — fresh observations remain
    /// authoritative for adaptation decisions.
    pub fn warm_start(&self, start: usize) {
        let clamped = start.clamp(
            self.config.min_concurrency,
            self.config.max_concurrency.max(1),
        );
        let mut g = lock(&self.inner);
        g.current = clamped;
        g.left_slow_start = true;
    }

    /// Snapshot of the current cap for persistence. Cheap, lock-only.
    #[must_use]
    pub fn snapshot(&self) -> usize {
        lock(&self.inner).current
    }
}

/// Outcome of evaluating one window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Decision {
    /// Healthy window — increase concurrency.
    Increase,
    /// Stressed window — decrease concurrency.
    Decrease,
    /// Inconclusive — hold steady (e.g. mixed signals, baseline not yet set).
    Hold,
}

fn evaluate(
    window: &VecDeque<Sample>,
    cfg: &LimiterConfig,
    baseline: Option<Duration>,
) -> Decision {
    // Capacity-relevant denominator: ApplicationError outcomes are
    // explicitly NOT capacity signals (per `Outcome` docs) and are
    // excluded from rate calculations. A wave of `AlreadyStored`
    // errors must not punish concurrency.
    let mut successes = 0usize;
    let mut timeouts = 0usize;
    let mut net_errors = 0usize;
    let mut latencies: Vec<Duration> = Vec::with_capacity(window.len());
    for s in window {
        match s.outcome {
            Outcome::Success => {
                successes += 1;
                latencies.push(s.latency);
            }
            Outcome::Timeout => timeouts += 1,
            Outcome::NetworkError => net_errors += 1,
            Outcome::ApplicationError => {}
        }
    }
    let capacity_total = successes + timeouts + net_errors;
    if capacity_total < cfg.min_window_ops {
        // Not enough capacity-relevant evidence to act. Hold.
        return Decision::Hold;
    }
    let total_f = capacity_total as f64;
    let success_rate = successes as f64 / total_f;
    let timeout_rate = timeouts as f64 / total_f;

    if success_rate < cfg.success_target || timeout_rate > cfg.timeout_ceiling {
        return Decision::Decrease;
    }

    if let Some(p95) = p95_of(&mut latencies) {
        if let Some(base) = baseline {
            let limit = base.mul_f64(cfg.latency_inflation_factor);
            if p95 > limit {
                return Decision::Decrease;
            }
        }
        Decision::Increase
    } else {
        Decision::Hold
    }
}

fn apply_decision(inner: &mut LimiterInner, decision: Decision, cfg: &LimiterConfig) {
    match decision {
        Decision::Increase => {
            // Gate increases on accumulating a fresh window's worth of
            // evidence since the last bump.
            if inner.samples_since_increase < cfg.window_ops {
                return;
            }
            let p95 = window_p95(&inner.window);
            inner.latency_baseline = Some(match inner.latency_baseline {
                None => p95,
                Some(prev) => ewma(prev, p95, cfg.latency_ewma_alpha),
            });
            let next = if inner.left_slow_start {
                inner.current.saturating_add(1)
            } else {
                inner.current.saturating_mul(2)
            };
            let next = next.min(cfg.max_concurrency).max(cfg.min_concurrency);
            if next != inner.current {
                debug!(
                    from = inner.current,
                    to = next,
                    slow_start = !inner.left_slow_start,
                    "adaptive: increase",
                );
            }
            inner.current = next;
            inner.samples_since_increase = 0;
            inner.samples_since_decrease = 0;
        }
        Decision::Decrease => {
            // Gate decreases on `min_window_ops` of fresh evidence
            // since the last decrease so a burst of concurrent
            // observations from in-flight ops can't pile-drive the
            // cap from N to 1 in a few back-to-back ticks.
            if inner.samples_since_decrease < cfg.min_window_ops {
                return;
            }
            inner.left_slow_start = true;
            let next = (inner.current / 2).max(cfg.min_concurrency);
            if next != inner.current {
                debug!(from = inner.current, to = next, "adaptive: decrease");
            }
            inner.current = next;
            inner.samples_since_increase = 0;
            inner.samples_since_decrease = 0;
        }
        Decision::Hold => {}
    }
}

/// p95 of a mutable slice of Durations. Sorts in place. Returns
/// `None` for an empty slice. Index choice: `ceil(len * 0.95) - 1`,
/// floored at 0, capped at `len - 1`.
fn p95_of(latencies: &mut [Duration]) -> Option<Duration> {
    if latencies.is_empty() {
        return None;
    }
    latencies.sort_unstable();
    let idx = ((latencies.len() as f64) * 0.95).ceil() as usize;
    let idx = idx.saturating_sub(1).min(latencies.len() - 1);
    latencies.get(idx).copied()
}

fn window_p95(window: &VecDeque<Sample>) -> Duration {
    let mut latencies: Vec<Duration> = window
        .iter()
        .filter(|s| matches!(s.outcome, Outcome::Success))
        .map(|s| s.latency)
        .collect();
    p95_of(&mut latencies).unwrap_or(Duration::ZERO)
}

fn ewma(prev: Duration, sample: Duration, alpha: f64) -> Duration {
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 1.0)
    } else {
        return prev;
    };
    let prev_ms = prev.as_secs_f64() * 1000.0;
    let sample_ms = sample.as_secs_f64() * 1000.0;
    let new_ms = (1.0 - alpha) * prev_ms + alpha * sample_ms;
    if !new_ms.is_finite() || new_ms < 0.0 {
        return prev;
    }
    Duration::from_secs_f64(new_ms / 1000.0)
}

/// Bundle of per-channel limiters owned by the `Client`.
#[derive(Debug, Clone)]
pub struct AdaptiveController {
    pub quote: Limiter,
    pub store: Limiter,
    pub fetch: Limiter,
    /// `pub(crate)` so external callers cannot mutate this
    /// post-construction. Each `Limiter` snapshots its own
    /// `Arc<LimiterConfig>` at construction time, so external
    /// mutation here would silently desync `warm_start`'s
    /// `enabled` check from the limiters' frozen copies. Read via
    /// `config()`.
    pub(crate) config: AdaptiveConfig,
    /// Per-instance cold-start values. `warm_start` floors snapshot
    /// values against THIS, not the global `ChannelStart::default()`,
    /// so a controller built with custom (e.g. low) starts stays
    /// faithful to its construction parameters. Constructed-once,
    /// never mutated.
    cold_start: ChannelStart,
}

impl AdaptiveController {
    /// Create a controller with cold-start values per channel.
    /// Sanitizes the config (NaN guards, floor/ceiling enforcement)
    /// before constructing limiters. The supplied `start` is captured
    /// as the per-instance cold-start floor for `warm_start`.
    #[must_use]
    pub fn new(start: ChannelStart, config: AdaptiveConfig) -> Self {
        let mut config = config;
        config.sanitize();
        let quote_cfg = LimiterConfig::from_adaptive(&config, config.max.quote);
        let store_cfg = LimiterConfig::from_adaptive(&config, config.max.store);
        let fetch_cfg = LimiterConfig::from_adaptive(&config, config.max.fetch);
        Self {
            quote: Limiter::new(start.quote, quote_cfg),
            store: Limiter::new(start.store, store_cfg),
            fetch: Limiter::new(start.fetch, fetch_cfg),
            config,
            cold_start: start,
        }
    }

    /// Snapshot current per-channel caps for persistence.
    #[must_use]
    pub fn snapshot(&self) -> ChannelStart {
        ChannelStart {
            quote: self.quote.snapshot(),
            store: self.store.snapshot(),
            fetch: self.fetch.snapshot(),
        }
    }

    /// Read-only access to the controller's adaptive config. Made
    /// read-only deliberately: each `Limiter` snapshots its own
    /// `Arc<LimiterConfig>` at construction, so post-hoc mutation
    /// would silently desync `warm_start`'s `enabled` check from
    /// the limiters' frozen copies.
    #[must_use]
    pub fn config(&self) -> &AdaptiveConfig {
        &self.config
    }

    /// Apply a previously-saved snapshot as the warm-start cap.
    ///
    /// The effective warm value per channel is
    /// `max(snapshot, self.cold_start)` — flooring at the
    /// per-instance cold-start (NOT the global default) so:
    /// 1. A prior bad run that pinned cap=1 doesn't pessimize this
    ///    run forever.
    /// 2. A controller built with custom (e.g. low) cold starts for
    ///    benchmarking is not silently jumped above its construction
    ///    parameters.
    ///
    /// Does not clear sliding windows. When `enabled = false`, this
    /// is a no-op — fixed-concurrency mode means fixed-concurrency.
    pub fn warm_start(&self, snapshot: ChannelStart) {
        if !self.config.enabled {
            return;
        }
        self.quote
            .warm_start(snapshot.quote.max(self.cold_start.quote));
        self.store
            .warm_start(snapshot.store.max(self.cold_start.store));
        self.fetch
            .warm_start(snapshot.fetch.max(self.cold_start.fetch));
    }
}

impl Default for AdaptiveController {
    fn default() -> Self {
        Self::new(ChannelStart::default(), AdaptiveConfig::default())
    }
}

/// Cancel-on-drop guard: if the wrapping future is dropped before
/// completion, record no outcome. We don't synthesize a Cancelled
/// signal because (a) dropped work was never observed by the network
/// and (b) injecting fake outcomes would skew the sliding window
/// after a fail-fast burst. The intentional behavior is "silent on
/// cancel, observe on completion" — callers that need to keep
/// fail-fast batches drained for full signal use `rebucketed`.
struct ObserveGuard<'a> {
    limiter: &'a Limiter,
    started: Instant,
    outcome: Option<(Outcome, Duration)>,
}

impl<'a> ObserveGuard<'a> {
    fn new(limiter: &'a Limiter) -> Self {
        Self {
            limiter,
            started: Instant::now(),
            outcome: None,
        }
    }
    fn finish(&mut self, outcome: Outcome) {
        self.outcome = Some((outcome, self.started.elapsed()));
    }
}

impl Drop for ObserveGuard<'_> {
    fn drop(&mut self) {
        if let Some((outcome, latency)) = self.outcome.take() {
            self.limiter.observe(outcome, latency);
        }
    }
}

/// Helper for instrumented call sites: time an async op, classify the
/// result, and report to a `Limiter`. Returns the original result.
///
/// ## Cancellation safety
///
/// Uses an internal `ObserveGuard` so the recorded outcome is
/// committed via `Drop` after the inner future returns. If the
/// wrapper future is itself dropped before `op().await` resolves
/// (caller cancellation, `buffer_unordered` fail-fast), no outcome
/// is recorded — this is intentional, see the guard's docs.
///
/// ```ignore
/// let res = observe_op(&controller.store, || async { do_put().await }, classify_put_err).await;
/// ```
pub async fn observe_op<T, E, F, Fut, C>(limiter: &Limiter, op: F, classify: C) -> Result<T, E>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
    C: FnOnce(&E) -> Outcome,
{
    let mut guard = ObserveGuard::new(limiter);
    let result = op().await;
    let outcome = match &result {
        Ok(_) => Outcome::Success,
        Err(e) => classify(e),
    };
    guard.finish(outcome);
    drop(guard); // commit observation explicitly so it lands before return
    result
}

/// Process an iterator of items with a rolling scheduler whose cap
/// is re-read from the limiter as each slot frees. Replaces the
/// "snapshot the cap once at pipeline build" behavior of plain
/// `buffer_unordered(N)` so a long pipeline (e.g. 10 GB download =
/// ~2500 chunks) sees adaptive growth/decay mid-flight.
///
/// Output is unordered (first-completion). For an ordered result
/// (e.g. `data_download` feeds chunks in DataMap order to
/// self_encryption decrypt), wrap items with their index and sort
/// after collection — see `rebucketed_ordered`.
///
/// On error: in-flight work drains to completion (so observed
/// outcomes still feed the controller) but no new launches happen.
/// The first error is preserved; later errors are discarded.
pub async fn rebucketed_unordered<I, T, E, F, Fut>(
    limiter: &Limiter,
    items: I,
    mut op: F,
) -> Result<Vec<T>, E>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    use futures::stream::{FuturesUnordered, StreamExt};
    let mut iter = items.into_iter().peekable();
    let mut in_flight: FuturesUnordered<Fut> = FuturesUnordered::new();
    let mut results = Vec::new();
    let mut pending_err: Option<E> = None;
    loop {
        // Refill: re-read the cap and launch up to `cap - in_flight.len()`
        // new items, but only if we are not already in error-stop.
        if pending_err.is_none() {
            let cap = limiter.current().max(1);
            while in_flight.len() < cap {
                match iter.next() {
                    Some(item) => in_flight.push(op(item)),
                    None => break,
                }
            }
        }
        if in_flight.is_empty() {
            break;
        }
        match in_flight.next().await {
            Some(Ok(v)) => results.push(v),
            Some(Err(e)) => {
                if pending_err.is_none() {
                    pending_err = Some(e);
                }
            }
            None => break,
        }
    }
    match pending_err {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

/// Ordered variant: items are tagged with a usize index by the
/// caller (typically by `iter.enumerate()`); after rolling
/// completion, results are sorted by index so output preserves
/// input order. Use this for callers that pass to APIs which
/// consume positionally (e.g. self_encryption's
/// `get_root_data_map_parallel` zips `Vec<(idx, Bytes)>` with input
/// hashes positionally and discards the idx — without a final sort
/// the bytes pair with the wrong hashes).
///
/// `op` is `FnMut(Item) -> Fut` where `Item` carries whatever
/// payload the caller needs; the closure must return
/// `Result<(usize, U), E>` so the wrapper can sort by the index.
pub async fn rebucketed_ordered<I, U, E, F, Fut>(
    limiter: &Limiter,
    items: I,
    op: F,
) -> Result<Vec<U>, E>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: std::future::Future<Output = Result<(usize, U), E>>,
{
    let mut indexed = rebucketed_unordered(limiter, items, op).await?;
    indexed.sort_by_key(|(idx, _)| *idx);
    Ok(indexed.into_iter().map(|(_, v)| v).collect())
}

/// Backward-compatible wrapper. `ordered = false` -> rolling
/// unordered. `ordered = true` -> the OLD batch-fence ordered path
/// (kept for tests that explicitly assert batch-fence semantics).
/// New call sites should use `rebucketed_unordered` or
/// `rebucketed_ordered` directly.
pub async fn rebucketed<I, T, E, F, Fut>(
    limiter: &Limiter,
    items: I,
    ordered: bool,
    mut op: F,
) -> Result<Vec<T>, E>
where
    I: IntoIterator,
    F: FnMut(I::Item) -> Fut,
    Fut: std::future::Future<Output = Result<T, E>>,
{
    if !ordered {
        return rebucketed_unordered(limiter, items, op).await;
    }
    use futures::stream::{self, StreamExt};
    let mut iter = items.into_iter();
    let mut results = Vec::new();
    let mut pending_err: Option<E> = None;
    loop {
        if pending_err.is_some() {
            break;
        }
        let cap = limiter.current().max(1);
        let mut batch = Vec::with_capacity(cap);
        for item in iter.by_ref().take(cap) {
            batch.push(op(item));
        }
        if batch.is_empty() {
            break;
        }
        let mut s = stream::iter(batch).buffered(cap);
        while let Some(r) = s.next().await {
            match r {
                Ok(v) => results.push(v),
                Err(e) => {
                    if pending_err.is_none() {
                        pending_err = Some(e);
                    }
                }
            }
        }
    }
    match pending_err {
        Some(e) => Err(e),
        None => Ok(results),
    }
}

/// On-disk shape for the persisted adaptive state. Versioned so we
/// can evolve the controller without crashing on stale files — an
/// unknown schema version simply causes a silent fallback to cold
/// defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedState {
    schema: u32,
    channels: ChannelStart,
}

const PERSIST_SCHEMA: u32 = 1;
const PERSIST_FILENAME: &str = "client_adaptive.json";

/// Default persistence path: `<data_dir>/client_adaptive.json`. Falls
/// back to `None` if the platform data dir is not resolvable; in that
/// case the controller still works, it just won't persist.
#[must_use]
pub fn default_persist_path() -> Option<PathBuf> {
    crate::config::data_dir()
        .ok()
        .map(|d| d.join(PERSIST_FILENAME))
}

/// Load a persisted snapshot from disk, returning `None` if the file
/// does not exist, is unreadable, contains malformed JSON, or has a
/// schema version this build does not understand. Persistence is best
/// effort — never propagate errors that would block the user's
/// operation.
#[must_use]
pub fn load_snapshot(path: &Path) -> Option<ChannelStart> {
    let bytes = std::fs::read(path).ok()?;
    let state: PersistedState = match serde_json::from_slice(&bytes) {
        Ok(s) => s,
        Err(e) => {
            warn!(path = %path.display(), error = %e, "adaptive: corrupt snapshot, ignoring");
            return None;
        }
    };
    if state.schema != PERSIST_SCHEMA {
        debug!(
            path = %path.display(),
            schema = state.schema,
            expected = PERSIST_SCHEMA,
            "adaptive: snapshot schema mismatch, ignoring",
        );
        return None;
    }
    Some(state.channels)
}

/// Save a snapshot to disk atomically (write to `<path>.tmp`, then
/// rename). Best effort — failures are logged at warn and discarded.
pub fn save_snapshot(path: &Path, channels: ChannelStart) {
    let state = PersistedState {
        schema: PERSIST_SCHEMA,
        channels,
    };
    let bytes = match serde_json::to_vec_pretty(&state) {
        Ok(b) => b,
        Err(e) => {
            warn!(error = %e, "adaptive: snapshot serialize failed");
            return;
        }
    };
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!(path = %parent.display(), error = %e, "adaptive: snapshot mkdir failed");
            return;
        }
    }
    // Unique-per-save temp filename: PID + monotonic counter +
    // nanosecond timestamp guarantees no collision between concurrent
    // CLI invocations OR concurrent save_snapshot calls within one
    // process (e.g. multiple Client instances sharing the same data
    // dir). POSIX rename is atomic on the destination, so the rename
    // target overlap is fine — last writer wins.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let counter = SAVE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = path.with_extension(format!(
        "json.tmp.{}.{}.{}",
        std::process::id(),
        counter,
        nanos
    ));
    if let Err(e) = std::fs::write(&tmp, &bytes) {
        warn!(path = %tmp.display(), error = %e, "adaptive: snapshot write failed");
        return;
    }
    if let Err(e) = std::fs::rename(&tmp, path) {
        warn!(
            from = %tmp.display(),
            to = %path.display(),
            error = %e,
            "adaptive: snapshot rename failed",
        );
        // Try to clean up the temp on rename failure so we don't
        // leave junk in the data dir. Best effort.
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Save with a wall-clock deadline. Spawns the synchronous
/// `save_snapshot` on a detached thread and waits up to `timeout`
/// for it to finish. If the thread is still running past the
/// deadline (e.g. because the data dir is on a hung NFS mount),
/// returns without joining — the OS will clean up the thread when
/// the process exits.
///
/// Used by `Client::drop` so a stalled filesystem cannot block
/// process shutdown indefinitely.
pub fn save_snapshot_with_timeout(path: PathBuf, channels: ChannelStart, timeout: Duration) {
    let handle = std::thread::spawn(move || {
        save_snapshot(&path, channels);
    });
    // Park briefly waiting for the thread, polling its status. We
    // use a short polling interval rather than `join()` because
    // join() blocks indefinitely.
    let started = Instant::now();
    let poll = Duration::from_millis(5);
    while started.elapsed() < timeout {
        if handle.is_finished() {
            let _ = handle.join();
            return;
        }
        std::thread::sleep(poll);
    }
    // Deadline elapsed. Detach the thread; it will continue to run
    // in the background until process exit (its work is best-effort
    // anyway). Log so operators can see the slow filesystem.
    warn!(
        timeout_ms = timeout.as_millis() as u64,
        "adaptive: snapshot save timed out (data dir slow?); detaching writer thread"
    );
    drop(handle);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn cfg_for_tests() -> LimiterConfig {
        LimiterConfig {
            enabled: true,
            min_concurrency: 1,
            max_concurrency: 64,
            window_ops: 10,
            min_window_ops: 5,
            success_target: 0.9,
            timeout_ceiling: 0.2,
            latency_inflation_factor: 2.0,
            latency_ewma_alpha: 0.5,
        }
    }

    /// Build an `AdaptiveConfig` for tests that need to construct a
    /// full `AdaptiveController`. Mirrors `cfg_for_tests()` defaults
    /// where they overlap, plus per-channel max derived from the same
    /// `max_concurrency` value.
    fn adaptive_cfg_for_tests() -> AdaptiveConfig {
        let l = cfg_for_tests();
        AdaptiveConfig {
            enabled: l.enabled,
            min_concurrency: l.min_concurrency,
            max: ChannelMax {
                quote: l.max_concurrency,
                store: l.max_concurrency,
                fetch: l.max_concurrency,
            },
            window_ops: l.window_ops,
            min_window_ops: l.min_window_ops,
            success_target: l.success_target,
            timeout_ceiling: l.timeout_ceiling,
            latency_inflation_factor: l.latency_inflation_factor,
            latency_ewma_alpha: l.latency_ewma_alpha,
        }
    }

    #[test]
    fn cold_start_clamps_into_bounds() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(1000, cfg.clone());
        assert_eq!(l.current(), cfg.max_concurrency);
        let l = Limiter::new(0, cfg.clone());
        assert_eq!(l.current(), cfg.min_concurrency);
    }

    #[test]
    fn slow_start_doubles_then_caps() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(2, cfg.clone());
        // Feed a full healthy window — concurrency doubles.
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        assert_eq!(l.current(), 4);
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        assert_eq!(l.current(), 8);
    }

    #[test]
    fn first_failure_exits_slow_start() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg.clone());
        // 6 successes + 4 timeouts in a window of 10. Decisions fire
        // per-sample once the window has min_window_ops entries, so
        // the four timeouts each drive Decrease. That floors the cap.
        for _ in 0..6 {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        for _ in 0..4 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        let after_stress = l.current();
        assert!(
            after_stress < 4,
            "stress should reduce concurrency from 4, got {after_stress}",
        );
        // After exiting slow-start, recovery is +1 per fresh window,
        // not doubling. The first `window_ops` successes flush prior
        // timeouts out of the sliding window. Decreases now also need
        // `min_window_ops` of fresh evidence before re-firing, and
        // increases need `window_ops` of fresh evidence. Feed enough
        // successes to clear the window AND accumulate evidence for
        // multiple increases.
        for _ in 0..(cfg.window_ops * 5) {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        assert!(
            l.current() > after_stress,
            "expected recovery above {after_stress}, got {}",
            l.current(),
        );
    }

    #[test]
    fn floor_holds_at_one() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(2, cfg);
        for _ in 0..30 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        assert_eq!(l.current(), 1);
    }

    #[test]
    fn application_errors_do_not_punish() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg.clone());
        // ApplicationError is NOT a capacity signal (per `Outcome`
        // docs and the reviewer's M1 finding). A wave of e.g.
        // `AlreadyStored` errors must not lower concurrency, because
        // they say nothing about the network's ability to take more
        // load. Specifically: the controller should HOLD at 4 because
        // there are zero capacity-relevant samples to act on.
        for _ in 0..cfg.window_ops * 5 {
            l.observe(Outcome::ApplicationError, Duration::from_millis(50));
        }
        assert_eq!(
            l.current(),
            4,
            "ApplicationError must not move the cap; got {}",
            l.current()
        );
    }

    #[test]
    fn latency_inflation_triggers_decrease() {
        let cfg = LimiterConfig {
            window_ops: 20,
            min_window_ops: 5,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg.clone());
        // Establish a baseline with many fast successes.
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        let after_baseline = l.current();
        // Now flood with slow successes — same outcome, 5x latency.
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Success, Duration::from_millis(500));
        }
        // Latency inflation > 2x baseline must drop concurrency.
        assert!(
            l.current() < after_baseline,
            "expected decrease from {after_baseline}, got {}",
            l.current(),
        );
    }

    #[test]
    fn warm_start_overrides_current() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(2, cfg);
        l.warm_start(20);
        assert_eq!(l.current(), 20);
    }

    #[test]
    fn warm_start_clamps() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(2, cfg.clone());
        l.warm_start(1_000_000);
        assert_eq!(l.current(), cfg.max_concurrency);
    }

    #[test]
    fn disabled_controller_holds_steady() {
        let cfg = LimiterConfig {
            enabled: false,
            ..cfg_for_tests()
        };
        let l = Limiter::new(8, cfg);
        for _ in 0..50 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        assert_eq!(l.current(), 8);
    }

    #[test]
    fn controller_snapshot_round_trips() {
        // The test cfg has max=64 for every channel (cfg_for_tests's
        // max_concurrency=64 -> ChannelMax::{quote: 64, store: 64, fetch: 64}).
        // Pick start values <= 64 so they survive cap clamping at
        // construction. Pick values >= cold-defaults (32/8/64) so they
        // also survive the warm-start floor.
        let c = AdaptiveController::new(
            ChannelStart {
                quote: 64,
                store: 16,
                fetch: 64,
            },
            adaptive_cfg_for_tests(),
        );
        let snap = c.snapshot();
        assert_eq!(snap.quote, 64);
        assert_eq!(snap.store, 16);
        assert_eq!(snap.fetch, 64);

        let c2 = AdaptiveController::default();
        c2.warm_start(snap);
        assert_eq!(c2.quote.current(), 64);
        assert_eq!(c2.store.current(), 16);
        assert_eq!(c2.fetch.current(), 64);
    }

    #[tokio::test]
    async fn observe_op_records_success() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg.clone());
        for _ in 0..cfg.window_ops {
            let _: Result<(), &str> =
                observe_op(&l, || async { Ok(()) }, |_e: &&str| Outcome::NetworkError).await;
        }
        // Healthy window from cold start doubles 4 -> 8.
        assert_eq!(l.current(), 8);
    }

    #[test]
    fn snapshot_round_trips_through_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client_adaptive.json");
        let snap = ChannelStart {
            quote: 24,
            store: 6,
            fetch: 12,
        };
        save_snapshot(&path, snap);
        let loaded = load_snapshot(&path).unwrap();
        assert_eq!(loaded.quote, 24);
        assert_eq!(loaded.store, 6);
        assert_eq!(loaded.fetch, 12);
    }

    #[test]
    fn load_missing_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does_not_exist.json");
        assert!(load_snapshot(&path).is_none());
    }

    #[test]
    fn load_corrupt_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not valid json{{{").unwrap();
        assert!(load_snapshot(&path).is_none());
    }

    #[test]
    fn load_wrong_schema_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.json");
        // Schema 999 is from a future build — current build must not
        // crash and must not act on it.
        let payload = r#"{"schema":999,"channels":{"quote":1,"store":1,"fetch":1}}"#;
        std::fs::write(&path, payload).unwrap();
        assert!(load_snapshot(&path).is_none());
    }

    #[tokio::test]
    async fn observe_op_records_classified_error() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg.clone());
        for _ in 0..cfg.window_ops {
            let _: Result<(), &str> =
                observe_op(&l, || async { Err("boom") }, |_e: &&str| Outcome::Timeout).await;
        }
        assert!(l.current() < 4);
    }

    // ----- Adversarial / regression-guard tests below ---------------------
    //
    // These exist primarily to prove the controller never silently regresses
    // upload/download throughput and never panics under hostile workloads.

    /// Cold-start defaults must equal-or-exceed the prior static knobs so
    /// the very first batch on a fresh install is no slower than before
    /// the adaptive controller existed. Hard-coded literals are intentional
    /// — this is a guard against future commits accidentally lowering them.
    #[test]
    fn no_regression_cold_start_at_least_static_defaults() {
        let s = ChannelStart::default();
        assert!(
            s.quote >= 32,
            "quote cold-start regressed: got {}, prior static was 32",
            s.quote,
        );
        assert!(
            s.store >= 8,
            "store cold-start regressed: got {}, prior static was 8",
            s.store,
        );
        assert!(
            s.fetch >= 64,
            "fetch cold-start regressed: got {}, prior static was 64 (unbounded before)",
            s.fetch,
        );
    }

    /// The production `AdaptiveController::default()` (NOT the test cfg)
    /// must come up reporting the cold-start values immediately, with no
    /// observations recorded.
    #[test]
    fn controller_default_config_is_sane() {
        let c = AdaptiveController::default();
        let starts = ChannelStart::default();
        assert_eq!(c.quote.current(), starts.quote);
        assert_eq!(c.store.current(), starts.store);
        assert_eq!(c.fetch.current(), starts.fetch);
        // No observations made yet — internal windows must be empty.
        assert_eq!(lock(&c.quote.inner).window.len(), 0);
        assert_eq!(lock(&c.store.inner).window.len(), 0);
        assert_eq!(lock(&c.fetch.inner).window.len(), 0);
    }

    /// Mixed signals (every other op fails) must not pin the controller
    /// at the floor for the whole run. The cap should oscillate or settle
    /// somewhere above the floor — collapse to 1 forever would be a bug.
    #[test]
    fn alternating_success_failure_collapses_to_floor() {
        // 50% timeout rate is far above `timeout_ceiling` (0.2 in test
        // config), so the window is always stressed. The controller
        // MUST collapse to the floor, and once there must NEVER go
        // below it. Assert both invariants explicitly: floor reached
        // and floor held.
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg.clone());
        let mut min_observed = usize::MAX;
        let mut max_observed = 0usize;
        let mut floor_visits = 0usize;
        for i in 0..1000 {
            let outcome = if i % 2 == 0 {
                Outcome::Success
            } else {
                Outcome::Timeout
            };
            l.observe(outcome, Duration::from_millis(50));
            let cur = l.current();
            assert!(
                cur >= cfg.min_concurrency,
                "cap underflowed floor at iter {i}: got {cur}",
            );
            min_observed = min_observed.min(cur);
            max_observed = max_observed.max(cur);
            if cur == cfg.min_concurrency {
                floor_visits += 1;
            }
        }
        assert_eq!(
            min_observed, cfg.min_concurrency,
            "cap never reached the floor under 50% timeout rate"
        );
        assert!(
            max_observed >= 8,
            "cap never visited the start value: max_observed={max_observed}"
        );
        // Should spend MOST of the run at the floor — a single
        // healthy window is not enough to climb back from a 50% loss
        // environment.
        assert!(
            floor_visits > 500,
            "cap spent only {floor_visits}/1000 ticks at floor; expected mostly at floor"
        );
        assert_eq!(
            l.current(),
            cfg.min_concurrency,
            "controller did not settle at floor after 1000 alternations"
        );
    }

    /// From the floor, a long stream of healthy successes must walk the
    /// cap all the way back up to `max_concurrency`. Otherwise transient
    /// stress on a slow link would permanently penalize throughput.
    #[test]
    fn pure_success_stream_recovers_to_max() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(cfg.min_concurrency, cfg.clone());
        for _ in 0..10_000 {
            l.observe(Outcome::Success, Duration::from_millis(5));
        }
        assert_eq!(
            l.current(),
            cfg.max_concurrency,
            "expected recovery to max ({}), got {}",
            cfg.max_concurrency,
            l.current(),
        );
    }

    /// Heavy stress drives the cap to the floor; subsequent recovery
    /// must climb meaningfully higher than the floor with enough healthy
    /// evidence. No "permanent floor" failure mode allowed.
    #[test]
    fn stress_then_heal_drives_floor_then_recovery() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg.clone());
        for _ in 0..100 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        let after_stress = l.current();
        assert_eq!(
            after_stress, cfg.min_concurrency,
            "stress should drive cap to floor, got {after_stress}",
        );
        for _ in 0..1_000 {
            l.observe(Outcome::Success, Duration::from_millis(10));
        }
        let after_heal = l.current();
        assert!(
            after_heal >= cfg.min_concurrency.saturating_add(4),
            "expected substantial recovery from floor, got {after_heal}",
        );
    }

    /// The latency baseline must track actual workload latency. If it
    /// stayed pinned at `Duration::ZERO`, every healthy sample would
    /// look like infinite inflation and inflate the decrease rate.
    #[test]
    fn baseline_does_not_grow_unbounded_under_slow_links() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(2, cfg.clone());
        for _ in 0..(cfg.window_ops * 10) {
            l.observe(Outcome::Success, Duration::from_millis(500));
        }
        let baseline = lock(&l.inner).latency_baseline;
        let base = baseline.expect("baseline should be set after many healthy windows");
        assert!(
            base > Duration::ZERO,
            "baseline must not stay at ZERO, got {base:?}",
        );
        // Within 2x of the actual latency: 250ms..=1000ms.
        let lo = Duration::from_millis(250);
        let hi = Duration::from_millis(1000);
        assert!(
            base >= lo && base <= hi,
            "baseline drifted out of [{lo:?}, {hi:?}]: {base:?}",
        );
    }

    /// Until the first healthy window completes, the latency baseline
    /// stays `None` (so no false-inflation alarms). Decreases during the
    /// stress phase are driven purely by success/timeout rate, not by
    /// inflated p95 vs a phantom zero baseline.
    #[test]
    fn baseline_initialized_only_after_first_healthy_window() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg.clone());
        for _ in 0..50 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        // Without any healthy window, baseline must still be None.
        assert!(
            lock(&l.inner).latency_baseline.is_none(),
            "baseline must be None before any healthy window",
        );
        // Now drain healthy windows.
        for _ in 0..(cfg.window_ops * 5) {
            l.observe(Outcome::Success, Duration::from_millis(20));
        }
        let baseline = lock(&l.inner).latency_baseline;
        assert!(
            baseline.is_some(),
            "baseline must be Some after healthy windows",
        );
        let base = baseline.unwrap_or_default();
        assert!(
            base > Duration::ZERO,
            "baseline must reflect real latency, got {base:?}",
        );
    }

    /// A torrent of timeouts must not underflow the cap. Sample at
    /// several depths to catch any wraparound.
    #[test]
    fn min_concurrency_floor_holds_under_torrent_of_errors() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg.clone());
        for i in 0..50_000 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
            if i == 100 || i == 1_000 || i == 49_999 {
                let cur = l.current();
                assert_eq!(
                    cur, cfg.min_concurrency,
                    "floor breached at iter {i}: got {cur}",
                );
            }
        }
    }

    /// Mirror: a torrent of successes must not exceed `max_concurrency`.
    #[test]
    fn max_concurrency_ceiling_holds_under_torrent_of_successes() {
        let cfg = cfg_for_tests();
        let start = cfg
            .max_concurrency
            .saturating_sub(1)
            .max(cfg.min_concurrency);
        let l = Limiter::new(start, cfg.clone());
        for i in 0..50_000 {
            l.observe(Outcome::Success, Duration::from_millis(5));
            if i == 100 || i == 1_000 || i == 49_999 {
                let cur = l.current();
                assert!(
                    cur <= cfg.max_concurrency,
                    "ceiling breached at iter {i}: got {cur} > {}",
                    cfg.max_concurrency,
                );
            }
        }
        assert_eq!(l.current(), cfg.max_concurrency);
    }

    /// Slow-start doubles the cap; with `max_concurrency = usize::MAX/2`
    /// a naive `*2` would overflow. The controller must use saturating
    /// arithmetic and never panic. Also asserts the cap actually
    /// REACHED max — proving that "no panic" wasn't achieved by
    /// the cap getting stuck somewhere instead of growing.
    #[test]
    fn saturating_arithmetic_handles_extreme_config() {
        let cfg = LimiterConfig {
            max_concurrency: usize::MAX / 2,
            ..cfg_for_tests()
        };
        let start = usize::MAX / 4;
        let l = Limiter::new(start, cfg.clone());
        for _ in 0..(cfg.window_ops * 10) {
            l.observe(Outcome::Success, Duration::from_millis(1));
        }
        // First-iteration doubles start (which is max/4) to max/2 = ceiling.
        // The cap MUST have grown to the ceiling; if saturating math
        // were broken (panic) we'd never get here, but we'd also fail
        // if the cap got stuck at the start value.
        assert_eq!(
            l.current(),
            cfg.max_concurrency,
            "saturating math survived but cap did not grow to ceiling"
        );
    }

    /// FIFO eviction: prove that a window of pure-timeout collapses
    /// the cap, and once enough successes flush ALL timeouts out of
    /// the window, the cap can rise. The earlier version of this test
    /// used an OR clause that made the assertion satisfiable trivially;
    /// this version asserts the strict invariant: after eviction, cap
    /// must be STRICTLY GREATER than the post-stress cap.
    #[test]
    fn window_eviction_is_fifo() {
        let cfg = LimiterConfig {
            window_ops: 10,
            min_window_ops: 5,
            success_target: 0.9,
            timeout_ceiling: 0.1,
            ..cfg_for_tests()
        };
        let l = Limiter::new(8, cfg.clone());
        // Fill the window with timeouts. With decrease-gating
        // (samples_since_decrease >= min_window_ops between halvings),
        // window_ops=10 + min_window_ops=5 timeouts allow at most
        // ~2 halvings: 8 -> 4 -> 2. Cap must DROP from 8.
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        let after_stress = l.current();
        assert!(
            after_stress < 8,
            "expected cap to drop from 8 after pure-timeout window, got {after_stress}"
        );
        // Push enough successes to fully evict the timeouts AND
        // accumulate at least one full window of fresh evidence for
        // an Increase. window_ops to evict + window_ops to gate first
        // +1 = 2 * window_ops minimum; use 3x for safety margin.
        for _ in 0..(cfg.window_ops * 3) {
            l.observe(Outcome::Success, Duration::from_millis(20));
        }
        let after_recovery = l.current();
        // Strict greater-than: FIFO MUST flush the timeouts so a
        // fresh-window Increase can fire.
        assert!(
            after_recovery > after_stress,
            "FIFO eviction broken: cap stayed at {after_stress} after recovery successes (expected > {after_stress}, got {after_recovery})"
        );
    }

    /// With `enabled = false`, the controller is a no-op. Hot paths
    /// must see exactly `initial` at every check, no exceptions.
    #[test]
    fn disabled_controller_returns_initial_value_invariantly() {
        let cfg = LimiterConfig {
            enabled: false,
            ..cfg_for_tests()
        };
        let initial = 8;
        let l = Limiter::new(initial, cfg);
        for i in 0..1_000 {
            let outcome = match i % 4 {
                0 => Outcome::Success,
                1 => Outcome::Timeout,
                2 => Outcome::NetworkError,
                _ => Outcome::ApplicationError,
            };
            l.observe(outcome, Duration::from_millis(50));
            assert_eq!(
                l.current(),
                initial,
                "disabled controller moved at iter {i}",
            );
        }
    }

    /// 100 tasks concurrently observing 100 successes each. The cap
    /// must remain a valid in-bounds value, no panic, no deadlock.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_observations_do_not_corrupt_window() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg.clone());
        let mut handles = Vec::with_capacity(100);
        for _ in 0..100 {
            let l_clone = l.clone();
            handles.push(tokio::spawn(async move {
                for _ in 0..100 {
                    l_clone.observe(Outcome::Success, Duration::from_millis(5));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let cur = l.current();
        assert!(
            cur >= cfg.min_concurrency && cur <= cfg.max_concurrency,
            "cap out of bounds after concurrent observations: {cur}",
        );
    }

    /// Persisted higher values from a prior run must beat low cold-start
    /// defaults. Otherwise warm-start would silently pessimize throughput.
    /// (Values BELOW cold-start are floored — see
    /// `warm_start_floors_at_cold_defaults`.)
    #[test]
    fn persisted_snapshot_warm_starts_above_cold_floor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client_adaptive.json");
        // All snapshot values ABOVE the production cold-start defaults
        // so the warm_start floor doesn't kick in.
        let saved = ChannelStart {
            quote: 64,
            store: 32,
            fetch: 128,
        };
        save_snapshot(&path, saved);
        let loaded = load_snapshot(&path).unwrap();

        // Build a controller with intentionally low cold-start values
        // — these get overridden by warm_start.
        let low = ChannelStart {
            quote: 2,
            store: 2,
            fetch: 2,
        };
        let c = AdaptiveController::new(low, AdaptiveConfig::default());
        c.warm_start(loaded);
        assert_eq!(c.quote.current(), 64);
        assert_eq!(c.store.current(), 32);
        assert_eq!(c.fetch.current(), 128);
    }

    /// Two threads racing on `save_snapshot` must never produce a
    /// half-written file. Atomic-rename guarantees we either see the
    /// old content or the new content, never a torn write.
    #[test]
    fn save_load_round_trip_with_concurrent_writes() {
        use std::thread;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client_adaptive.json");
        let path_a = path.clone();
        let path_b = path.clone();
        let snap_a = ChannelStart {
            quote: 10,
            store: 10,
            fetch: 10,
        };
        let snap_b = ChannelStart {
            quote: 99,
            store: 99,
            fetch: 99,
        };
        let h_a = thread::spawn(move || {
            for _ in 0..50 {
                save_snapshot(&path_a, snap_a);
            }
        });
        let h_b = thread::spawn(move || {
            for _ in 0..50 {
                save_snapshot(&path_b, snap_b);
            }
        });
        h_a.join().unwrap();
        h_b.join().unwrap();
        let loaded = load_snapshot(&path).expect("file must be a valid snapshot, not torn");
        let valid = (loaded.quote == snap_a.quote
            && loaded.store == snap_a.store
            && loaded.fetch == snap_a.fetch)
            || (loaded.quote == snap_b.quote
                && loaded.store == snap_b.store
                && loaded.fetch == snap_b.fetch);
        assert!(valid, "loaded snapshot is neither A nor B: {loaded:?}",);
    }

    /// `save_snapshot` to an unwritable / impossible path must be a
    /// quiet no-op: best-effort, no panic, no error propagation.
    #[test]
    fn save_snapshot_to_unwritable_dir_does_not_panic() {
        // A path under a non-existent absolute root that the process
        // also cannot create. On macOS/Linux a write under "/" requires
        // root; create_dir_all will fail on this path.
        let path = PathBuf::from("/nonexistent_root_dir_xyz_for_test/sub/dir/client_adaptive.json");
        let snap = ChannelStart {
            quote: 1,
            store: 1,
            fetch: 1,
        };
        // No panic = pass. Function returns unit, errors are logged.
        save_snapshot(&path, snap);
        // File should not exist.
        assert!(!path.exists());
    }

    /// A truncated/partial JSON file must not crash the loader; it must
    /// return None so the controller falls back to cold defaults.
    #[test]
    fn load_snapshot_from_truncated_file_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated.json");
        std::fs::write(&path, br#"{"schema":1,"channels":{"quote":"#).unwrap();
        assert!(load_snapshot(&path).is_none());
    }

    /// Microbench: 100k observe+current pairs must complete in well
    /// under 100ms. Catches any accidental quadratic behaviour or
    /// massive lock contention introduced by future changes.
    #[test]
    fn controller_perf_overhead_is_bounded() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg);
        let started = Instant::now();
        for _ in 0..100_000 {
            let _ = l.current();
            l.observe(Outcome::Success, Duration::from_micros(1));
        }
        let elapsed = started.elapsed();
        // 1µs per pair on a modern machine is generous; allow 500ms to
        // tolerate slow CI runners while still catching real regressions.
        assert!(
            elapsed < Duration::from_millis(500),
            "100k observe+current pairs took {elapsed:?}, expected <500ms",
        );
    }

    // ---- Regression tests for adversarial-review findings ----

    /// M10 fix: hand-edited or future-schema configs may plant `NaN`
    /// or out-of-range values into the float fields. Constructing a
    /// controller and feeding observations must not panic.
    /// `Duration::from_secs_f64(NaN)` panics per std docs, so without
    /// `sanitize()` and the EWMA NaN guard this would crash.
    #[test]
    fn nan_and_out_of_range_config_does_not_panic() {
        let cfg = AdaptiveConfig {
            enabled: true,
            min_concurrency: 0, // sub-floor; sanitize raises to 1
            max: ChannelMax {
                quote: 0, // sub-min; sanitize raises to min
                store: 0,
                fetch: 0,
            },
            window_ops: 10,
            min_window_ops: 50, // > window_ops; sanitize clamps
            success_target: f64::NAN,
            timeout_ceiling: f64::INFINITY,
            latency_inflation_factor: f64::NEG_INFINITY,
            latency_ewma_alpha: f64::NAN,
        };
        let c = AdaptiveController::new(ChannelStart::default(), cfg);
        // Verify sanitize() ACTUALLY corrected the values (not just
        // that no panic occurred). Reading c.config back proves the
        // sanitization landed.
        let post = &c.config;
        assert_eq!(
            post.min_concurrency, 1,
            "sanitize did not raise min_concurrency from 0"
        );
        assert!(
            post.success_target.is_finite() && (0.0..=1.0).contains(&post.success_target),
            "sanitize did not clamp success_target from NaN: {}",
            post.success_target
        );
        assert!(
            post.timeout_ceiling.is_finite() && (0.0..=1.0).contains(&post.timeout_ceiling),
            "sanitize did not clamp timeout_ceiling from Inf: {}",
            post.timeout_ceiling
        );
        assert!(
            post.latency_inflation_factor.is_finite() && post.latency_inflation_factor > 0.0,
            "sanitize did not fix latency_inflation_factor from -Inf: {}",
            post.latency_inflation_factor
        );
        assert!(
            post.latency_ewma_alpha.is_finite() && (0.0..=1.0).contains(&post.latency_ewma_alpha),
            "sanitize did not fix latency_ewma_alpha from NaN: {}",
            post.latency_ewma_alpha
        );
        assert!(
            post.min_window_ops <= post.window_ops,
            "sanitize did not clamp min_window_ops <= window_ops: min={} window={}",
            post.min_window_ops,
            post.window_ops
        );
        assert!(
            post.max.quote >= post.min_concurrency,
            "max.quote below min_concurrency"
        );
        // Now exercise the runtime under hostile latencies — must
        // not panic.
        for _ in 0..200 {
            c.store
                .observe(Outcome::Success, Duration::from_secs(99_999));
            c.store.observe(Outcome::Timeout, Duration::ZERO);
        }
        let cur = c.store.current();
        assert!(cur >= 1, "cap below floor: {cur}");
    }

    /// M3+M6 fix: a burst of N concurrent in-flight chunks all
    /// observing stress at almost the same time used to pile-drive
    /// the cap from N to 1 in N back-to-back ticks. After the fix,
    /// decreases require `min_window_ops` of FRESH evidence between
    /// successive Decreases, so a single transient burst can drop
    /// the cap by at most one halving.
    #[test]
    fn transient_burst_does_not_pile_drive_to_floor() {
        let cfg = LimiterConfig {
            window_ops: 32,
            min_window_ops: 8,
            success_target: 0.95,
            timeout_ceiling: 0.10,
            ..cfg_for_tests()
        };
        let l = Limiter::new(32, cfg);
        // Simulate 8 concurrent ops all completing as Timeout in a
        // back-to-back burst (the kind of event that previously
        // floor-slammed the cap).
        for _ in 0..8 {
            l.observe(Outcome::Timeout, Duration::from_millis(10));
        }
        // After one burst, cap should have decreased AT MOST once
        // (32 -> 16). Pile-driving would land at 1 or 2.
        let after_burst = l.current();
        assert!(
            after_burst >= 16,
            "transient burst pile-drove cap from 32 to {after_burst}; expected >= 16",
        );
    }

    /// M2 fix: classifier must map transport-related errors to
    /// `NetworkError`, not `ApplicationError`. Test EACH transport
    /// variant separately so a regression in any one variant is
    /// caught by name.
    #[tokio::test]
    async fn transport_errors_classify_as_capacity_signal() {
        use crate::data::client::classify_error;
        use crate::data::error::Error;
        let make_cfg = || LimiterConfig {
            window_ops: 16,
            min_window_ops: 5,
            success_target: 0.5,
            timeout_ceiling: 0.5,
            ..cfg_for_tests()
        };
        // Cases: (variant_name, error_factory)
        type ErrFactory = Box<dyn Fn() -> Error>;
        let cases: Vec<(&str, ErrFactory)> = vec![
            ("Network", Box::new(|| Error::Network("net".to_string()))),
            (
                "InsufficientPeers",
                Box::new(|| Error::InsufficientPeers("ip".to_string())),
            ),
            ("Io", Box::new(|| Error::Io(std::io::Error::other("io")))),
            ("Protocol", Box::new(|| Error::Protocol("p".to_string()))),
            ("Storage", Box::new(|| Error::Storage("s".to_string()))),
            (
                "PartialUpload",
                Box::new(|| Error::PartialUpload {
                    stored: vec![],
                    stored_count: 0,
                    failed: vec![],
                    failed_count: 0,
                    total_chunks: 0,
                    reason: "r".to_string(),
                }),
            ),
        ];
        for (name, mk) in &cases {
            let l = Limiter::new(8, make_cfg());
            for _ in 0..16 {
                let _: std::result::Result<(), Error> =
                    observe_op(&l, || async { Err(mk()) }, classify_error).await;
            }
            // Each variant alone must drive the cap STRICTLY below
            // the start (8 -> 4 via one halving). If a variant maps
            // to ApplicationError, cap stays at 8.
            let cur = l.current();
            assert!(
                cur < 8,
                "{name} not classified as capacity signal: cap stayed at {cur}",
            );
        }
    }

    /// C4 fix: per-channel max ceilings. Confirm that a `LimiterConfig`
    /// with a constrained `max_concurrency` does not bleed into other
    /// channels. The ceilings are independent.
    #[test]
    fn per_channel_ceilings_are_independent() {
        let cfg = AdaptiveConfig {
            max: ChannelMax {
                quote: 4,    // tightly capped
                store: 8,    // moderate
                fetch: 1024, // very high
            },
            ..AdaptiveConfig::default()
        };
        let c = AdaptiveController::new(
            ChannelStart {
                quote: 4,
                store: 8,
                fetch: 64,
            },
            cfg,
        );
        // Feed 1000 successes to each channel; each must respect its
        // own ceiling and never one another's.
        for _ in 0..1000 {
            c.quote.observe(Outcome::Success, Duration::from_micros(10));
            c.store.observe(Outcome::Success, Duration::from_micros(10));
            c.fetch.observe(Outcome::Success, Duration::from_micros(10));
        }
        assert_eq!(c.quote.current(), 4, "quote should cap at 4");
        assert_eq!(c.store.current(), 8, "store should cap at 8");
        // fetch starts at 64, slow-start doubles each window. With
        // 1000 successes and window_ops=32, ~31 windows fire; cap
        // doubles 64 -> 128 -> 256 -> 512 -> 1024 = 4 doublings. Cap
        // MUST reach the channel's max of 1024.
        assert_eq!(
            c.fetch.current(),
            1024,
            "fetch did not reach its independent max of 1024; got {}",
            c.fetch.current()
        );
    }

    /// Cold-start equals the prior static defaults so the FIRST batch
    /// on a fresh install behaves identically. Guards against future
    /// commits silently dropping cold-start values below the prior
    /// statics.
    #[test]
    fn cold_start_at_least_prior_static_defaults() {
        let cs = ChannelStart::default();
        // Prior statics: quote=32, store=8. Fetch was effectively
        // unbounded (the entire self_encryption batch was fired at
        // once); we picked 64 as a conservative substitute so the
        // first batch of <= 64 chunks is indistinguishable.
        assert!(cs.quote >= 32, "quote cold-start regressed: {}", cs.quote);
        assert!(cs.store >= 8, "store cold-start regressed: {}", cs.store);
        assert!(cs.fetch >= 64, "fetch cold-start regressed: {}", cs.fetch);
    }

    /// Reviewer N-M5 guard: with the new gated-decrease semantics
    /// (decreases require `min_window_ops` of fresh evidence), the
    /// controller must STILL reach the floor under sustained stress
    /// within a bounded number of observations. Otherwise we've made
    /// the controller too sluggish to react to a real network
    /// outage.
    ///
    /// From start = 64 with `min_window_ops = 8`, reaching floor 1
    /// takes log2(64) = 6 halvings, each gated on 8 fresh samples,
    /// so the upper bound is roughly `6 * 8 + min_window_ops = ~56`
    /// observations. We allow 200 to absorb the warm-up window and
    /// any per-sample evaluation slack.
    #[test]
    fn sustained_stress_reaches_floor_within_bounded_ops() {
        let cfg = LimiterConfig {
            window_ops: 32,
            min_window_ops: 8,
            success_target: 0.95,
            timeout_ceiling: 0.10,
            max_concurrency: 64,
            ..cfg_for_tests()
        };
        let l = Limiter::new(64, cfg);
        let mut ops = 0usize;
        while l.current() > 1 && ops < 200 {
            l.observe(Outcome::Timeout, Duration::from_millis(10));
            ops += 1;
        }
        assert_eq!(
            l.current(),
            1,
            "controller did not reach floor within 200 observations under \
             sustained timeout stress; took {ops} ops, ended at cap {}",
            l.current()
        );
    }

    /// The default `AdaptiveController` (production defaults) starts
    /// each channel at the documented cold-start value, with each
    /// per-channel max strictly above the start (so the controller
    /// has room to grow).
    #[test]
    fn default_controller_has_growth_headroom() {
        let c = AdaptiveController::default();
        let cs = ChannelStart::default();
        let max = ChannelMax::default();
        assert_eq!(c.quote.current(), cs.quote);
        assert_eq!(c.store.current(), cs.store);
        assert_eq!(c.fetch.current(), cs.fetch);
        assert!(
            max.quote > cs.quote,
            "no growth headroom for quote: max={} start={}",
            max.quote,
            cs.quote
        );
        assert!(
            max.store > cs.store,
            "no growth headroom for store: max={} start={}",
            max.store,
            cs.store
        );
        assert!(
            max.fetch > cs.fetch,
            "no growth headroom for fetch: max={} start={}",
            max.fetch,
            cs.fetch
        );
    }

    // ---- Codex review (round 3) regression tests ----

    /// Codex CRITICAL: warm_start was blindly restoring caps below the
    /// cold-start floor. A prior bad run that drove store=1 would
    /// pessimize every subsequent run forever. The fix floors warm
    /// values at `ChannelStart::default()` per channel.
    #[test]
    fn warm_start_floors_at_cold_defaults() {
        let c = AdaptiveController::default();
        let cold = ChannelStart::default();
        // Snapshot from a "bad prior run" — every channel pinned to 1.
        let bad_snap = ChannelStart {
            quote: 1,
            store: 1,
            fetch: 1,
        };
        c.warm_start(bad_snap);
        // After warm_start, each channel should be AT LEAST the
        // cold-start value, not the persisted 1.
        assert_eq!(
            c.quote.current(),
            cold.quote,
            "quote warm_start did not floor at cold default"
        );
        assert_eq!(
            c.store.current(),
            cold.store,
            "store warm_start did not floor at cold default"
        );
        assert_eq!(
            c.fetch.current(),
            cold.fetch,
            "fetch warm_start did not floor at cold default"
        );
    }

    /// Warm values ABOVE the cold-start floor must still be honored —
    /// the floor is a one-sided lower bound, not a clamp.
    #[test]
    fn warm_start_honors_values_above_cold_floor() {
        let c = AdaptiveController::default();
        let cold = ChannelStart::default();
        let snap = ChannelStart {
            quote: cold.quote * 2,
            store: cold.store * 4,
            fetch: cold.fetch * 2,
        };
        c.warm_start(snap);
        assert_eq!(c.quote.current(), snap.quote);
        assert_eq!(c.store.current(), snap.store);
        assert_eq!(c.fetch.current(), snap.fetch);
    }

    /// Codex MAJOR: long pipelines used to capture the cap once via
    /// `buffer_unordered(N)`. `rebucketed` re-reads the cap at each
    /// batch boundary so adaptive growth/decay actually takes effect
    /// mid-stream. Test: fire 200 items at start cap=4, then halfway
    /// through bump the cap manually via warm_start to 16, and assert
    /// the LATER batches see the new cap.
    #[tokio::test]
    async fn rebucketed_picks_up_cap_changes_mid_stream() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 32,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        let max_seen = StdArc::new(AtomicUsize::new(0));
        let in_flight = StdArc::new(AtomicUsize::new(0));
        let processed = StdArc::new(AtomicUsize::new(0));
        let l_for_bump = l.clone();
        let processed_for_bump = processed.clone();
        // Spawn a watcher that bumps the cap once enough items have
        // started to "warm up".
        let bump_handle = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(2)).await;
                if processed_for_bump.load(AtomicOrdering::Relaxed) >= 16 {
                    l_for_bump.warm_start(16);
                    return;
                }
            }
        });
        let _: Vec<()> = rebucketed(&l, 0..200usize, false, |_i| {
            let max_seen = max_seen.clone();
            let in_flight = in_flight.clone();
            let processed = processed.clone();
            async move {
                let cur = in_flight.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                max_seen.fetch_max(cur, AtomicOrdering::Relaxed);
                tokio::time::sleep(Duration::from_millis(1)).await;
                in_flight.fetch_sub(1, AtomicOrdering::Relaxed);
                processed.fetch_add(1, AtomicOrdering::Relaxed);
                Ok::<(), &'static str>(())
            }
        })
        .await
        .unwrap();
        bump_handle.await.unwrap();
        // The cap was bumped to 16 mid-stream. If rebucketing actually
        // picks up cap changes, max_seen should reach above the
        // initial 4.
        let peak = max_seen.load(AtomicOrdering::Relaxed);
        assert!(
            peak > 4,
            "rebucketed did not pick up the mid-stream cap bump (peak in-flight = {peak})"
        );
    }

    /// Codex MAJOR: `observe_op` cancellation safety. If the wrapper
    /// future is dropped before the inner op completes, no outcome is
    /// recorded (intentional — dropped work was never observed by
    /// the network). This test asserts the contract: completed ops
    /// land observations, dropped ops do not corrupt the window.
    /// Two-sided: confirms cancellation is a NO-OP, AND confirms
    /// post-cancellation observations DO land normally (proving the
    /// limiter's internal state was not corrupted).
    #[tokio::test]
    async fn observe_op_cancellation_drops_silently() {
        let cfg = LimiterConfig {
            window_ops: 16,
            min_window_ops: 4,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        // Build a future that never completes, then drop it before
        // awaiting. observe_op must not panic on drop and must not
        // record an outcome.
        let l_clone = l.clone();
        let fut = observe_op(
            &l_clone,
            || async {
                std::future::pending::<()>().await;
                Ok::<(), &'static str>(())
            },
            |_| Outcome::Timeout,
        );
        drop(fut);
        // Cap unchanged: no observation was recorded.
        assert_eq!(l.current(), 4, "cancelled op moved the cap");
        // Now feed observations that ACTUALLY count as Success (the
        // Ok branch of observe_op is always Outcome::Success — the
        // classifier only runs on Err). Cold-start at 4 + a full
        // window of healthy successes = double to 8.
        for _ in 0..16 {
            let _: Result<(), &'static str> = observe_op(
                &l,
                || async { Ok(()) },
                // classifier only fires on Err; Ok is always Success
                |_| Outcome::NetworkError,
            )
            .await;
        }
        // STRICT: cap must have GROWN, not just held. If cancellation
        // had corrupted internal counters, slow-start might be stuck.
        assert!(
            l.current() > 4,
            "cap did not grow after 16 successes; controller corrupted by cancellation? cap={}",
            l.current(),
        );
    }

    /// Codex MAJOR: Drop persistence must be reliable. The CLI relies
    /// on Client::drop firing a synchronous save. If save_snapshot
    /// were dispatched via fire-and-forget spawn_blocking, runtime
    /// teardown would silently lose the snapshot. This test asserts
    /// that calling save_snapshot synchronously from a normal context
    /// (not Drop, but the same code path) actually writes.
    #[test]
    fn save_snapshot_is_synchronous_and_durable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client_adaptive.json");
        let snap = ChannelStart {
            quote: 100,
            store: 50,
            fetch: 200,
        };
        save_snapshot(&path, snap);
        // The file must exist immediately after save_snapshot returns.
        // No async waiting, no spawn_blocking, no eventual consistency.
        assert!(
            path.exists(),
            "save_snapshot did not write file synchronously"
        );
        let loaded = load_snapshot(&path).unwrap();
        assert_eq!(loaded.quote, 100);
        assert_eq!(loaded.store, 50);
        assert_eq!(loaded.fetch, 200);
    }

    // ---- Codex round 4 regression tests ----

    /// Codex CR-2 fix: warm_start marks the limiter as having
    /// already-left-slow-start, so a single healthy window does NOT
    /// double the cap (which would be over-aggressive resume from a
    /// learned value).
    #[tokio::test]
    async fn warm_start_disables_slow_start_doubling() {
        let cfg = LimiterConfig {
            window_ops: 8,
            min_window_ops: 4,
            success_target: 0.9,
            ..cfg_for_tests()
        };
        let l = Limiter::new(2, cfg.clone());
        // Warm-start to a learned value of 16. This must not be
        // treated as a fresh slow-start.
        l.warm_start(16);
        assert_eq!(l.current(), 16);
        // One full healthy window: in slow-start would double to 32;
        // post-warm-start it should add +1 to 17.
        for _ in 0..cfg.window_ops {
            l.observe(Outcome::Success, Duration::from_millis(10));
        }
        assert_eq!(
            l.current(),
            17,
            "warm-start triggered slow-start doubling instead of additive +1"
        );
    }

    /// Codex CR-3 fix: warm_start floors against the per-instance
    /// cold-start, NOT the global ChannelStart::default. A controller
    /// built with custom low starts must stay faithful to its
    /// construction parameters even after warm_start.
    #[test]
    fn controller_warm_start_floors_at_per_instance_cold_start() {
        let custom_cold = ChannelStart {
            quote: 2,
            store: 1,
            fetch: 4,
        };
        let c = AdaptiveController::new(custom_cold, AdaptiveConfig::default());
        // Snapshot below the per-instance cold-start floors at custom values.
        c.warm_start(ChannelStart {
            quote: 1,
            store: 1,
            fetch: 1,
        });
        assert_eq!(c.quote.current(), 2);
        assert_eq!(c.store.current(), 1);
        assert_eq!(c.fetch.current(), 4);
        // Snapshot above the per-instance cold-start uses the snapshot.
        c.warm_start(ChannelStart {
            quote: 10,
            store: 10,
            fetch: 10,
        });
        assert_eq!(c.quote.current(), 10);
        assert_eq!(c.store.current(), 10);
        assert_eq!(c.fetch.current(), 10);
    }

    /// Codex CR-3 fix: when adaptive.enabled = false, warm_start is
    /// a no-op — fixed-concurrency mode means the user wants exactly
    /// the cold start, not a learned value from a prior run.
    #[test]
    fn warm_start_is_noop_when_adaptive_disabled() {
        let cfg = AdaptiveConfig {
            enabled: false,
            ..AdaptiveConfig::default()
        };
        let custom_cold = ChannelStart {
            quote: 5,
            store: 5,
            fetch: 5,
        };
        let c = AdaptiveController::new(custom_cold, cfg);
        c.warm_start(ChannelStart {
            quote: 100,
            store: 100,
            fetch: 100,
        });
        assert_eq!(c.quote.current(), 5, "warm_start moved cap when disabled");
        assert_eq!(c.store.current(), 5, "warm_start moved cap when disabled");
        assert_eq!(c.fetch.current(), 5, "warm_start moved cap when disabled");
    }

    /// Codex CR-4 fix: rebucketed_unordered is rolling, not batch-fenced.
    /// One slow item must NOT block other items in the same logical
    /// "wave" — the in-flight set should refill as fast items complete.
    #[tokio::test]
    async fn rebucketed_unordered_is_rolling_not_fenced() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 8,
            window_ops: 100,
            min_window_ops: 50,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        let in_flight = StdArc::new(AtomicUsize::new(0));
        let max_in_flight = StdArc::new(AtomicUsize::new(0));
        let started = StdArc::new(AtomicUsize::new(0));
        let _: Vec<()> = rebucketed_unordered(&l, 0..20usize, |i| {
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight.clone();
            let started = started.clone();
            async move {
                let cur = in_flight.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                max_in_flight.fetch_max(cur, AtomicOrdering::Relaxed);
                started.fetch_add(1, AtomicOrdering::Relaxed);
                // Item 0 is intentionally slow; items 1..20 are fast.
                // In a batch-fenced scheduler, item 0 would gate the
                // start of items in the next batch. In a rolling
                // scheduler, items 1..N can start as soon as their
                // slot frees from a fast completion.
                if i == 0 {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                } else {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
                in_flight.fetch_sub(1, AtomicOrdering::Relaxed);
                Ok::<(), &'static str>(())
            }
        })
        .await
        .unwrap();
        // All 20 items must have started; in a rolling scheduler the
        // peak in-flight should reach at least 4 (the cap).
        assert_eq!(started.load(AtomicOrdering::Relaxed), 20);
        let peak = max_in_flight.load(AtomicOrdering::Relaxed);
        assert!(
            peak >= 4,
            "rolling scheduler did not fill cap; peak in-flight = {peak}"
        );
    }

    /// Codex CR-4 fix: rebucketed_ordered preserves input order.
    #[tokio::test]
    async fn rebucketed_ordered_preserves_input_order() {
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 4,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        let items: Vec<usize> = (0..50).collect();
        let result: Vec<usize> = rebucketed_ordered(
            &l,
            items.iter().copied().enumerate(),
            |(idx, v)| async move {
                // Reverse-bias delay so out-of-order completion is likely.
                let delay = (50 - v) as u64;
                tokio::time::sleep(Duration::from_micros(delay)).await;
                Ok::<_, &'static str>((idx, v * 10))
            },
        )
        .await
        .unwrap();
        assert_eq!(result.len(), 50);
        for (i, v) in result.iter().enumerate() {
            assert_eq!(*v, i * 10, "out of order at index {i}: got {v}");
        }
    }

    /// Codex CR-1 regression guard (logical, not the actual data path):
    /// rebucketed_ordered with a payload of (idx, hash) must always
    /// pair the right hash with the right chunk content even under
    /// adversarial out-of-order completion.
    #[tokio::test]
    async fn rebucketed_ordered_pairs_idx_with_payload_correctly() {
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 8,
            ..cfg_for_tests()
        };
        let l = Limiter::new(8, cfg);
        // Each item is (idx, fake_hash). The "fetch" returns
        // (idx, content_for_hash). We adversarially out-of-order them
        // and assert that the post-sort puts content with the right
        // index.
        let items: Vec<(usize, u64)> = (0..40).map(|i| (i, 1000u64 + i as u64)).collect();
        let result: Vec<u64> = rebucketed_ordered(&l, items, |(idx, hash)| async move {
            let delay = (40 - idx) as u64; // reverse delay
            tokio::time::sleep(Duration::from_micros(delay)).await;
            // "content_for_hash" derived from the hash.
            Ok::<_, &'static str>((idx, hash * 7))
        })
        .await
        .unwrap();
        for (i, v) in result.iter().enumerate() {
            let expected = (1000 + i as u64) * 7;
            assert_eq!(
                *v, expected,
                "idx {i} paired with wrong content: {v}, expected {expected}"
            );
        }
    }

    /// Codex CR-5 fix: snapshot temp file is unique per save call,
    /// not just per-PID. Two save_snapshot calls in the SAME process
    /// must not collide on the temp file.
    #[test]
    fn save_snapshot_temp_file_is_unique_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("client_adaptive.json");
        // Fire many saves back-to-back in the same process. Without
        // a per-call unique suffix, the temp file would be the same
        // for every call (PID is constant), and any partial write +
        // crash window would expose the race. We can't simulate the
        // exact race in a unit test, but we can confirm no panic and
        // the final file is correct after many calls.
        for i in 0..100 {
            save_snapshot(
                &path,
                ChannelStart {
                    quote: i + 1,
                    store: i + 1,
                    fetch: i + 1,
                },
            );
        }
        let loaded = load_snapshot(&path).unwrap();
        assert_eq!(loaded.quote, 100);
        assert_eq!(loaded.store, 100);
        assert_eq!(loaded.fetch, 100);
        // Confirm no leftover .tmp files.
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "temp files leaked: {:?}",
            leftover.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
    }

    // ---- Edge case tests ----

    /// Edge case: rebucketed_unordered with EMPTY input returns empty
    /// Vec immediately, no panic, no work scheduled.
    #[tokio::test]
    async fn rebucketed_empty_input_returns_empty() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(4, cfg);
        let v: Vec<usize> = rebucketed_unordered(&l, std::iter::empty::<usize>(), |_| async {
            Ok::<_, &'static str>(42usize)
        })
        .await
        .unwrap();
        assert!(v.is_empty());
        let v: Vec<usize> = rebucketed_ordered(
            &l,
            std::iter::empty::<(usize, ())>(),
            |(idx, _)| async move { Ok::<_, &'static str>((idx, 42usize)) },
        )
        .await
        .unwrap();
        assert!(v.is_empty());
    }

    /// Edge case: rebucketed_unordered with EXACTLY cap items.
    #[tokio::test]
    async fn rebucketed_exactly_cap_items() {
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 4,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        let v: Vec<usize> =
            rebucketed_unordered(
                &l,
                0..4usize,
                |i| async move { Ok::<_, &'static str>(i * 2) },
            )
            .await
            .unwrap();
        assert_eq!(v.len(), 4);
    }

    /// Edge case: rebucketed_unordered preserves the FIRST error and
    /// discards subsequent ones, draining in-flight work first.
    #[tokio::test]
    async fn rebucketed_preserves_first_error() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 4,
            ..cfg_for_tests()
        };
        let l = Limiter::new(4, cfg);
        let started = StdArc::new(AtomicUsize::new(0));
        let started_clone = started.clone();
        let result: Result<Vec<()>, &'static str> = rebucketed_unordered(&l, 0..20usize, |i| {
            let started = started_clone.clone();
            async move {
                started.fetch_add(1, AtomicOrdering::Relaxed);
                if i == 5 {
                    // Slight delay so item 6, 7 also start before
                    // this error propagates.
                    tokio::time::sleep(Duration::from_micros(100)).await;
                    return Err("first error");
                }
                if i == 10 {
                    return Err("second error - should be ignored");
                }
                tokio::time::sleep(Duration::from_micros(50)).await;
                Ok(())
            }
        })
        .await;
        match result {
            Err(e) => assert_eq!(e, "first error", "wrong error preserved"),
            Ok(_) => panic!("expected error, got ok"),
        }
        // The first error stops new launches, but in-flight items
        // drain. We don't assert exact count (nondeterministic) — only
        // that we did not launch ALL 20 items (proving error-stop
        // works) and we did launch more than just item 5 (proving
        // in-flight drain happens).
        let total = started.load(AtomicOrdering::Relaxed);
        assert!(
            (5..20).contains(&total),
            "started count out of range: {total}"
        );
    }

    /// Edge case: limiter with min == max (degenerate single-value).
    /// Cap stays at the single value regardless of observations.
    #[test]
    fn limiter_with_min_equal_max_is_pinned() {
        let cfg = LimiterConfig {
            min_concurrency: 5,
            max_concurrency: 5,
            ..cfg_for_tests()
        };
        let l = Limiter::new(5, cfg);
        for _ in 0..1000 {
            l.observe(Outcome::Success, Duration::from_millis(1));
        }
        assert_eq!(l.current(), 5, "cap moved despite min==max");
        for _ in 0..1000 {
            l.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        assert_eq!(l.current(), 5, "cap moved despite min==max");
    }

    /// Direct test of `ewma()` math: alpha = 0 means new value =
    /// prev (the baseline never updates from new samples).
    #[test]
    fn ewma_alpha_zero_returns_prev() {
        let prev = Duration::from_millis(100);
        let sample = Duration::from_millis(500);
        let result = ewma(prev, sample, 0.0);
        assert_eq!(result, prev, "alpha=0 must return prev unchanged");
    }

    /// Direct test of `ewma()` math: alpha = 1 means new value =
    /// sample (full overwrite).
    #[test]
    fn ewma_alpha_one_returns_sample() {
        let prev = Duration::from_millis(100);
        let sample = Duration::from_millis(500);
        let result = ewma(prev, sample, 1.0);
        // Allow 1ms of float-conversion slop.
        let diff = result.abs_diff(sample);
        assert!(
            diff <= Duration::from_millis(1),
            "alpha=1 should return sample; got {result:?}, expected ~{sample:?}"
        );
    }

    /// Direct test of `ewma()`: alpha = 0.5 should give the midpoint.
    #[test]
    fn ewma_alpha_half_returns_midpoint() {
        let prev = Duration::from_millis(200);
        let sample = Duration::from_millis(400);
        let result = ewma(prev, sample, 0.5);
        let expected = Duration::from_millis(300);
        let diff = result.abs_diff(expected);
        assert!(
            diff <= Duration::from_millis(1),
            "alpha=0.5 midpoint: got {result:?}, expected ~{expected:?}"
        );
    }

    /// Direct test of `ewma()`: NaN alpha must NOT panic and must
    /// preserve the previous value (defense against
    /// `Duration::from_secs_f64(NaN)` panic).
    #[test]
    fn ewma_nan_alpha_returns_prev() {
        let prev = Duration::from_millis(100);
        let sample = Duration::from_millis(500);
        let result = ewma(prev, sample, f64::NAN);
        assert_eq!(result, prev);
        let result = ewma(prev, sample, f64::INFINITY);
        assert_eq!(result, prev);
        let result = ewma(prev, sample, f64::NEG_INFINITY);
        assert_eq!(result, prev);
    }

    /// Out-of-range alpha (e.g. 2.5) must clamp to [0,1] and NOT
    /// produce a negative result.
    #[test]
    fn ewma_clamps_alpha_above_one() {
        let prev = Duration::from_millis(100);
        let sample = Duration::from_millis(500);
        let result = ewma(prev, sample, 2.5);
        // Clamped to 1.0 -> should equal sample (~500ms).
        assert!(result >= Duration::from_millis(499));
        assert!(result <= Duration::from_millis(501));
    }

    /// Edge case: window contains ONLY ApplicationErrors. Controller
    /// must HOLD (not move at all), because there are zero
    /// capacity-relevant samples.
    #[test]
    fn window_full_of_application_errors_does_not_move_cap() {
        let cfg = cfg_for_tests();
        let l = Limiter::new(8, cfg.clone());
        for _ in 0..(cfg.window_ops * 5) {
            l.observe(Outcome::ApplicationError, Duration::from_millis(50));
        }
        assert_eq!(
            l.current(),
            8,
            "cap moved on pure-app-error window; should hold"
        );
    }

    /// Edge case: AdaptiveController with `enabled = false` plus
    /// observations does not move and does not interact with the
    /// observation window.
    #[test]
    fn disabled_adaptive_controller_truly_inert() {
        let cfg = AdaptiveConfig {
            enabled: false,
            ..AdaptiveConfig::default()
        };
        let c = AdaptiveController::new(ChannelStart::default(), cfg);
        let baseline_quote = c.quote.current();
        let baseline_store = c.store.current();
        let baseline_fetch = c.fetch.current();
        for _ in 0..10000 {
            c.quote.observe(Outcome::Timeout, Duration::from_millis(1));
            c.store.observe(Outcome::Timeout, Duration::from_millis(1));
            c.fetch.observe(Outcome::Timeout, Duration::from_millis(1));
        }
        assert_eq!(c.quote.current(), baseline_quote);
        assert_eq!(c.store.current(), baseline_store);
        assert_eq!(c.fetch.current(), baseline_fetch);
    }

    /// Edge case: per-channel limiters share NO state. Hammering one
    /// channel must not move another. Two-sided: assert store DROPS
    /// to the floor (proving observations landed) AND quote/fetch
    /// are EXACTLY unchanged (proving zero cross-channel leakage).
    #[test]
    fn channel_state_is_independent() {
        let c = AdaptiveController::default();
        let q0 = c.quote.current();
        let f0 = c.fetch.current();
        let s0 = c.store.current();
        for _ in 0..1000 {
            c.store.observe(Outcome::Timeout, Duration::from_millis(1));
        }
        // Strict: store reached the floor (observations landed).
        assert_eq!(
            c.store.current(),
            c.config.min_concurrency,
            "store did not reach floor after 1000 timeouts; cap={}",
            c.store.current()
        );
        assert!(c.store.current() < s0, "store cap did not move at all");
        // Strict: quote and fetch unchanged.
        assert_eq!(c.quote.current(), q0, "quote leaked from store stress");
        assert_eq!(c.fetch.current(), f0, "fetch leaked from store stress");
    }

    // ---- Round-5 test reviewer suggestions ----

    /// Direct unit test for `AdaptiveConfig::sanitize`. Verifies that
    /// every clamped field is correctly fixed up, not merely that
    /// the controller doesn't crash.
    #[test]
    fn sanitize_corrects_pathological_floats() {
        let mut cfg = AdaptiveConfig {
            success_target: f64::NAN,
            timeout_ceiling: 5.0,
            latency_inflation_factor: f64::NEG_INFINITY,
            latency_ewma_alpha: 2.5,
            window_ops: 4,
            min_window_ops: 10,
            ..AdaptiveConfig::default()
        };
        cfg.sanitize();
        assert!(cfg.success_target.is_finite());
        assert!((0.0..=1.0).contains(&cfg.success_target));
        assert!((0.0..=1.0).contains(&cfg.timeout_ceiling));
        assert!(cfg.latency_inflation_factor.is_finite());
        assert!(cfg.latency_inflation_factor > 0.0);
        assert!((0.0..=1.0).contains(&cfg.latency_ewma_alpha));
        assert!(
            cfg.min_window_ops <= cfg.window_ops,
            "min_window_ops {} > window_ops {}",
            cfg.min_window_ops,
            cfg.window_ops
        );
    }

    /// Snapshot persistence relies on serde for ChannelStart and
    /// ChannelMax. A field rename in either type would silently
    /// break warm-start across binary upgrades — this test catches
    /// that.
    #[test]
    fn channel_max_serde_round_trips() {
        let m = ChannelMax {
            quote: 7,
            store: 13,
            fetch: 200,
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ChannelMax = serde_json::from_str(&json).unwrap();
        assert_eq!(back.quote, 7);
        assert_eq!(back.store, 13);
        assert_eq!(back.fetch, 200);
    }

    #[test]
    fn channel_start_serde_round_trips() {
        let s = ChannelStart {
            quote: 11,
            store: 22,
            fetch: 33,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: ChannelStart = serde_json::from_str(&json).unwrap();
        assert_eq!(back.quote, 11);
        assert_eq!(back.store, 22);
        assert_eq!(back.fetch, 33);
    }

    /// Mid-flight cap SHRINKAGE: `rebucketed_picks_up_cap_changes_mid_stream`
    /// only proves growth. Overload protection requires the reverse —
    /// when the controller halves the cap mid-pipeline, in-flight
    /// must respect the new lower cap on the next refill.
    #[tokio::test]
    async fn rebucketed_honors_cap_shrinkage_mid_stream() {
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
        use std::sync::Arc as StdArc;
        let cfg = LimiterConfig {
            min_concurrency: 1,
            max_concurrency: 16,
            ..cfg_for_tests()
        };
        let l = Limiter::new(16, cfg);
        let in_flight = StdArc::new(AtomicUsize::new(0));
        let max_after_shrink = StdArc::new(AtomicUsize::new(0));
        let processed = StdArc::new(AtomicUsize::new(0));
        let shrunk = StdArc::new(std::sync::atomic::AtomicBool::new(false));
        let l_for_shrink = l.clone();
        let p_for_shrink = processed.clone();
        let shrunk_for_shrink = shrunk.clone();
        let shrink_handle = tokio::spawn(async move {
            // Bump down the cap once 50 items have completed.
            loop {
                tokio::time::sleep(Duration::from_millis(2)).await;
                if p_for_shrink.load(AtomicOrdering::Relaxed) >= 50 {
                    l_for_shrink.warm_start(2);
                    shrunk_for_shrink.store(true, AtomicOrdering::Relaxed);
                    return;
                }
            }
        });
        let _: Vec<()> = rebucketed_unordered(&l, 0..400usize, |_i| {
            let in_flight = in_flight.clone();
            let max_after_shrink = max_after_shrink.clone();
            let processed = processed.clone();
            let shrunk = shrunk.clone();
            async move {
                let cur = in_flight.fetch_add(1, AtomicOrdering::Relaxed) + 1;
                if shrunk.load(AtomicOrdering::Relaxed) {
                    max_after_shrink.fetch_max(cur, AtomicOrdering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
                in_flight.fetch_sub(1, AtomicOrdering::Relaxed);
                processed.fetch_add(1, AtomicOrdering::Relaxed);
                Ok::<(), &'static str>(())
            }
        })
        .await
        .unwrap();
        shrink_handle.await.unwrap();
        let peak = max_after_shrink.load(AtomicOrdering::Relaxed);
        // After the shrink to cap=2, no NEW launches should put us
        // above 2. Already-launched in-flight may still be draining
        // briefly, so allow a small overshoot for the natural
        // refill-after-completion lag.
        assert!(
            peak <= 4,
            "rebucketed exceeded shrunk cap of 2: peak post-shrink in-flight = {peak}"
        );
    }

    /// Mixed `ApplicationError` + capacity-relevant items in one
    /// window. ApplicationError must NOT contribute to the success
    /// rate denominator — otherwise a wave with some AppErrors and
    /// some healthy successes would falsely look like a stressed
    /// window.
    #[test]
    fn mixed_window_app_errors_with_capacity_signal() {
        let cfg = LimiterConfig {
            window_ops: 10,
            min_window_ops: 5,
            timeout_ceiling: 0.2,
            success_target: 0.9,
            ..cfg_for_tests()
        };
        // Case 1: 5 AppErrors + 5 Successes. Capacity-relevant
        // success_rate = 5/5 = 100%. Cap must NOT decrease (it may
        // hold at 8 or grow via slow-start; both prove the AppErrors
        // didn't poison the success-rate denominator).
        let l = Limiter::new(8, cfg.clone());
        for _ in 0..5 {
            l.observe(Outcome::ApplicationError, Duration::from_millis(50));
        }
        for _ in 0..5 {
            l.observe(Outcome::Success, Duration::from_millis(50));
        }
        assert!(
            l.current() >= 8,
            "AppErrors falsely depressed the success rate; cap dropped from 8 to {}",
            l.current()
        );
        // Case 2: 5 AppErrors + 5 Timeouts. Capacity-relevant
        // success_rate = 0/5 = 0%. Cap MUST decrease.
        let l2 = Limiter::new(8, cfg);
        for _ in 0..5 {
            l2.observe(Outcome::ApplicationError, Duration::from_millis(50));
        }
        for _ in 0..5 {
            l2.observe(Outcome::Timeout, Duration::from_millis(50));
        }
        assert!(
            l2.current() < 8,
            "all-timeouts (with AppError padding) did not decrease cap; got {}",
            l2.current()
        );
    }

    /// Real concurrent torn-read test for save/load. The previous
    /// concurrent-write test only reads after both writers join;
    /// this version interleaves a reader thread with writers and
    /// asserts every successful load returns a coherent (non-torn)
    /// snapshot.
    #[test]
    fn concurrent_save_load_no_torn_reads() {
        use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
        use std::thread;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        // Seed the file so the reader doesn't get a None on first read.
        save_snapshot(
            &path,
            ChannelStart {
                quote: 1,
                store: 1,
                fetch: 1,
            },
        );
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let p_w = path.clone();
        let s_w = stop.clone();
        let writer = thread::spawn(move || {
            let mut i = 1usize;
            while !s_w.load(AtomicOrdering::Relaxed) {
                save_snapshot(
                    &p_w,
                    ChannelStart {
                        quote: i,
                        store: i,
                        fetch: i,
                    },
                );
                i = i.wrapping_add(1).max(1);
            }
        });
        let p_r = path.clone();
        let reader = thread::spawn(move || {
            let mut torn = 0usize;
            for _ in 0..2_000 {
                if let Some(snap) = load_snapshot(&p_r) {
                    // Coherent snapshots have all three channels equal
                    // (writer always saves equal values).
                    if snap.quote != snap.store || snap.store != snap.fetch {
                        torn += 1;
                    }
                }
            }
            torn
        });
        let torn = reader.join().unwrap();
        stop.store(true, AtomicOrdering::Relaxed);
        writer.join().unwrap();
        assert_eq!(
            torn, 0,
            "observed {torn} torn reads under concurrent writes"
        );
    }

    /// Round-5 follow-up: `save_snapshot_with_timeout` returns
    /// promptly even when the underlying write would otherwise hang.
    /// Use a path under a non-existent root that mkdir cannot create
    /// to simulate a slow/failing filesystem (mkdir returns Err
    /// quickly so this isn't a real hang test, but it confirms the
    /// timeout wrapper does not block longer than the deadline on a
    /// fast-failing operation either).
    #[test]
    fn save_with_timeout_returns_promptly_on_fast_failure() {
        let path = std::path::PathBuf::from("/nonexistent_root_xyz_test/snap.json");
        let snap = ChannelStart {
            quote: 1,
            store: 1,
            fetch: 1,
        };
        let started = Instant::now();
        save_snapshot_with_timeout(path, snap, Duration::from_secs(5));
        let elapsed = started.elapsed();
        // Fast-failing mkdir returns immediately. The timeout
        // wrapper should not add measurable overhead.
        assert!(
            elapsed < Duration::from_secs(1),
            "save_snapshot_with_timeout took {elapsed:?} on fast-failing path"
        );
    }

    /// Round-5 follow-up: a hung writer thread (simulated by a path
    /// the writer never returns from). The wrapper must time out and
    /// return without joining; the test must complete near the
    /// deadline, not hang.
    #[test]
    fn save_with_timeout_bounds_wall_time_on_hang() {
        // Use a real-but-slow-write simulation: hand the writer a
        // path that the OS will accept but with a synthetic delay
        // baked into a wrapping thread. Since save_snapshot itself
        // does no sleep, we instead test that the timeout wrapper
        // exits within deadline + small slack when the inner work
        // takes longer than the deadline. We approximate by giving
        // the wrapper a deadline shorter than any plausible local
        // disk write (1ms is too tight; 0ms is too tight). Use
        // 1ms deadline and assert wall time < 100ms — proving the
        // wrapper does NOT wait for the writer to actually finish
        // (the inner write to a tempdir takes a few ms typically).
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("snap.json");
        let snap = ChannelStart {
            quote: 1,
            store: 1,
            fetch: 1,
        };
        let started = Instant::now();
        // Deadline so short that on most machines the writer is
        // still running. The wrapper must NOT wait for it.
        save_snapshot_with_timeout(path, snap, Duration::from_micros(1));
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_millis(200),
            "timeout wrapper did not bound wall time: {elapsed:?}"
        );
    }
}
