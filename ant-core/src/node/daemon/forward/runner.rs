//! The background task that does the forwarding.
//!
//! It follows the shape of the daemon's other background workers (`spawn_eviction_monitor`,
//! `spawn_liveness_monitor`): spawned once, driven by a poll interval, stopped by a
//! [`CancellationToken`]. It additionally carries its own token so `disable` can stop forwarding
//! without touching the daemon.
//!
//! The ordering within a cycle is deliberate: **poll, deliver, then persist offsets.** Persisting
//! before delivery would mean a daemon killed mid-cycle had already promised never to re-read
//! events that never left the machine. Doing it after means a crash re-reads a little, which the
//! deterministic document ids turn into harmless duplicates that the endpoint rejects with a 409.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::config::LogForwardConfig;
use super::document::{ForwardDocument, NodeTags};
use super::offsets::OffsetStore;
use super::sink::{
    deliver, DocumentQueue, LogSink, RetryPolicy, DEFAULT_BATCH_BYTES, DEFAULT_BATCH_DOCUMENTS,
    DEFAULT_QUEUE_CAPACITY,
};
use super::tail::LogTailer;
use super::{ForwardStats, ForwardingNode, SkippedNode};
use crate::node::registry::NodeRegistry;

/// How often the forwarder looks for new log content.
///
/// Fast enough to satisfy "logs appear within a minute" with room to spare — including the one
/// extra cycle the tailer spends holding a growing file's last event so continuation lines can join
/// it — and slow enough that following a handful of quiet files costs nothing measurable.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Live view of what the forwarder is doing, shared with the status endpoint.
///
/// Counters only. Which nodes are being tailed is derived from the registry by
/// [`classify_nodes`] at the point of asking, so that status never reports a stale node list
/// from before the forwarder's first poll.
#[derive(Debug, Clone, Default)]
pub struct ForwarderSnapshot {
    pub stats: ForwardStats,
}

/// Handle to a running forwarder.
///
/// Dropping this does not stop the task — the daemon holds it for the process's lifetime, and
/// `disable` stops it explicitly.
pub struct ForwarderHandle {
    cancel: CancellationToken,
    shared: Arc<RwLock<ForwarderSnapshot>>,
    endpoint: String,
    /// Retained so that stopping can be *awaited*. Without this there is no way to tell a caller
    /// that the last request has actually finished, which is what `disable` needs to promise.
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl ForwarderHandle {
    /// Signal the forwarder to stop, without waiting for it. Idempotent.
    ///
    /// Prefer [`Self::stop_and_wait`] where the caller is about to tell a user that forwarding has
    /// stopped.
    pub fn stop(&self) {
        self.cancel.cancel();
    }

    /// Stop forwarding and wait until the task has actually exited.
    ///
    /// This is what makes `disable` a real revocation boundary rather than a request. Cancellation
    /// is observed inside the delivery loop, and dropping the in-flight future cancels the HTTP
    /// request with it, so this returns promptly rather than after the retry ladder plays out.
    pub async fn stop_and_wait(&self) {
        self.cancel.cancel();
        let task = self.task.lock().await.take();
        if let Some(task) = task {
            let _ = task.await;
        }
    }

    #[must_use]
    pub fn is_stopped(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Where this forwarder is shipping to, for status output.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Current counters and node lists.
    pub async fn snapshot(&self) -> ForwarderSnapshot {
        self.shared.read().await.clone()
    }
}

/// Start forwarding in the background.
pub fn spawn_log_forwarder(
    registry: Arc<RwLock<NodeRegistry>>,
    config: LogForwardConfig,
    sink: Arc<dyn LogSink>,
    offsets_path: PathBuf,
    poll_interval: Duration,
    shutdown: CancellationToken,
) -> ForwarderHandle {
    let cancel = CancellationToken::new();
    let shared = Arc::new(RwLock::new(ForwarderSnapshot::default()));

    let endpoint = sink.describe();
    let task_cancel = cancel.clone();
    let task_shared = shared.clone();

    let task = tokio::spawn(async move {
        let mut state = ForwarderRun {
            registry,
            config,
            sink,
            offsets: OffsetStore::load(&offsets_path),
            tailers: HashMap::new(),
            queue: DocumentQueue::new(DEFAULT_QUEUE_CAPACITY),
            stats: ForwardStats::default(),
            retry: RetryPolicy::default(),
            cancel: task_cancel.clone(),
        };

        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = task_cancel.cancelled() => break,
                () = tokio::time::sleep(poll_interval) => {}
            }

            let snapshot = state.run_cycle().await;
            *task_shared.write().await = snapshot;
        }

        // A clean shutdown persists what was read, so the next start resumes rather than replays.
        if let Err(error) = state.offsets.save() {
            tracing::warn!("log forwarding: could not persist tail offsets: {error}");
        }
        tracing::info!("log forwarding: stopped");
    });

    ForwarderHandle {
        cancel,
        shared,
        endpoint,
        task: tokio::sync::Mutex::new(Some(task)),
    }
}

/// Everything one running forwarder owns.
struct ForwarderRun {
    registry: Arc<RwLock<NodeRegistry>>,
    config: LogForwardConfig,
    sink: Arc<dyn LogSink>,
    offsets: OffsetStore,
    tailers: HashMap<u32, (LogTailer, NodeTags)>,
    queue: DocumentQueue,
    stats: ForwardStats,
    retry: RetryPolicy,
    /// Cancelled by `disable` or by daemon shutdown. Checked between batches and raced against each
    /// delivery, so neither an in-flight request nor a retry backoff outlives it.
    cancel: CancellationToken,
}

impl ForwarderRun {
    /// One poll-and-ship cycle.
    async fn run_cycle(&mut self) -> ForwarderSnapshot {
        self.refresh_tailers().await;

        // Files seen across *all* tailers this cycle, and whether that view is complete. Pruning
        // against a partial view would drop live positions, so a cycle that skipped a tailer --
        // because it errored or because cancellation cut the loop short -- does not prune at all.
        let mut live_files: Vec<String> = Vec::new();
        let mut complete_view = true;

        for (tailer, tags) in self.tailers.values_mut() {
            if self.cancel.is_cancelled() {
                complete_view = false;
                break;
            }
            let outcome = match tailer.poll(&mut self.offsets, self.config.min_level).await {
                Ok(outcome) => outcome,
                Err(error) => {
                    tracing::debug!(
                        "log forwarding: node {} could not be read this cycle: {error}",
                        tailer.node_id()
                    );
                    complete_view = false;
                    continue;
                }
            };

            self.stats.events_dropped_by_level += outcome.dropped_by_level;
            live_files.extend(outcome.live_files.iter().cloned());

            for event in &outcome.events {
                match ForwardDocument::build(
                    event,
                    tags,
                    &self.config.index_prefix,
                    &self.config.installation_id,
                ) {
                    Some(document) => self.queue.push(document),
                    // Parsing rejects unusable timestamps, so this is defensive rather than
                    // expected; counting it keeps the totals honest either way.
                    None => self.stats.events_dropped_by_level += 1,
                }
            }
        }

        // Once, against every tailer's files: a per-tailer prune would delete the other nodes'
        // positions and send them back to the start of their current file on the next cycle.
        if complete_view {
            self.offsets.prune(&live_files);
        }

        self.flush_queue().await;

        if let Err(error) = self.offsets.save() {
            tracing::warn!("log forwarding: could not persist tail offsets: {error}");
        }

        self.stats.events_dropped_by_overflow = self.queue.dropped();

        ForwarderSnapshot {
            stats: self.stats.clone(),
        }
    }

    /// Reconcile the tailer set against the registry, so nodes added or removed while forwarding is
    /// on are picked up without an enable/disable cycle.
    async fn refresh_tailers(&mut self) {
        let registry = self.registry.read().await;
        let (forwarding, _) = classify_nodes(&registry);

        let live: Vec<u32> = forwarding.iter().map(|node| node.node_id).collect();
        self.tailers.retain(|id, _| live.contains(id));

        for node in &registry.list() {
            let Some(log_dir) = node.log_dir.clone() else {
                continue;
            };
            let tags = NodeTags::from_config(node);

            match self.tailers.get_mut(&node.id) {
                // Identity can change under us: an auto-upgrade replaces the binary and bumps the
                // version, and events after that point should say so.
                Some((_, existing_tags)) => *existing_tags = tags,
                None => {
                    let mut tailer = LogTailer::new(node.id, log_dir.clone());
                    // A node whose files we already have positions for is being resumed, not newly
                    // adopted, so it must not skip forward to the end of its log.
                    if self.has_offsets_for(&log_dir) {
                        tailer.mark_primed();
                    }
                    self.tailers.insert(node.id, (tailer, tags));
                }
            }
        }
    }

    /// Whether persisted offsets already mention a file in this directory.
    fn has_offsets_for(&self, log_dir: &std::path::Path) -> bool {
        let prefix = log_dir.display().to_string();
        self.offsets.keys().any(|key| key.starts_with(&prefix))
    }

    /// Ship everything currently queued, abandoning the moment forwarding is revoked.
    ///
    /// Both checks matter. The loop check stops a full queue from taking further batches after
    /// `disable`, and the `select!` drops the delivery future mid-flight — which cancels the HTTP
    /// request with it, since a dropped `reqwest` future cancels the request, and discards any
    /// pending retry backoff along with it. Without the second, a `disable` issued at the wrong
    /// moment would keep uploading for the length of the retry ladder.
    async fn flush_queue(&mut self) {
        while !self.queue.is_empty() {
            if self.cancel.is_cancelled() {
                return;
            }

            let batch = self
                .queue
                .take_batch(DEFAULT_BATCH_DOCUMENTS, DEFAULT_BATCH_BYTES);
            if batch.is_empty() {
                break;
            }

            let count = batch.len() as u64;
            let delivery = deliver(self.sink.as_ref(), batch, self.retry, |delay| {
                Box::pin(tokio::time::sleep(delay))
            });

            let report = tokio::select! {
                biased;
                () = self.cancel.cancelled() => {
                    tracing::debug!(
                        "log forwarding: revoked mid-delivery; abandoning {count} document(s)"
                    );
                    return;
                }
                report = delivery => report,
            };

            self.stats.events_forwarded += report.delivered;

            if report.is_complete_success() {
                self.stats.batches_sent += 1;
                self.stats.last_success_unix = Some(now_unix_secs());
                self.stats.last_error = None;
            } else {
                self.stats.batches_failed += 1;
                self.stats.last_error = report.error.clone();
                tracing::debug!(
                    "log forwarding: {} of {count} documents did not reach {}: {}",
                    report.rejected + report.abandoned,
                    self.sink.describe(),
                    report.error.as_deref().unwrap_or("no detail"),
                );
            }
        }
    }
}

/// Split the registry into nodes that can be forwarded and nodes that cannot.
///
/// Node file logging is off unless the user asked for it, so "cannot" is the common case on a
/// default install. Reporting it is what stops `enable` looking like it worked while shipping
/// nothing at all.
pub fn classify_nodes(registry: &NodeRegistry) -> (Vec<ForwardingNode>, Vec<SkippedNode>) {
    let mut forwarding = Vec::new();
    let mut skipped = Vec::new();

    let mut nodes = registry.list();
    nodes.sort_by_key(|node| node.id);

    for node in nodes {
        match &node.log_dir {
            Some(log_dir) => forwarding.push(ForwardingNode {
                node_id: node.id,
                service: node.service_name.clone(),
                log_dir: log_dir.display().to_string(),
            }),
            None => skipped.push(SkippedNode::no_logging(node.id, node.service_name.clone())),
        }
    }

    (forwarding, skipped)
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::daemon::forward::sink::mock::MockSink;
    use crate::node::types::{EvmNetwork, NodeConfig, UpgradeChannel};
    use std::collections::HashMap as StdHashMap;
    use std::io::Write;

    struct Harness {
        _dir: tempfile::TempDir,
        root: PathBuf,
        registry: Arc<RwLock<NodeRegistry>>,
    }

    impl Harness {
        /// One entry per node, in registry order: `true` means the node has logging enabled.
        ///
        /// Ids are not passed in because `NodeRegistry::add` assigns its own, starting at 1 — so
        /// the nth entry here is node `n + 1`.
        async fn new(nodes: &[bool]) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let mut registry = NodeRegistry::load(&root.join("node_registry.json")).unwrap();

            for (index, with_logging) in nodes.iter().enumerate() {
                let id = index as u32 + 1;
                let log_dir = with_logging.then(|| root.join(format!("logs-{id}")));
                if let Some(ref path) = log_dir {
                    std::fs::create_dir_all(path).unwrap();
                }
                registry.add(NodeConfig {
                    id,
                    service_name: format!("node{id}"),
                    rewards_address: "0xabc".to_string(),
                    data_dir: root.join(format!("data-{id}")),
                    log_dir,
                    node_port: None,
                    binary_path: root.join("antnode"),
                    version: "0.17.2-beta.1".to_string(),
                    env_variables: StdHashMap::new(),
                    bootstrap_peers: Vec::new(),
                    upgrade_channel: Some(UpgradeChannel::Beta),
                    evm_network: EvmNetwork::default(),
                    eviction: None,
                });
            }

            Self {
                _dir: dir,
                root,
                registry: Arc::new(RwLock::new(registry)),
            }
        }

        fn append(&self, node_id: u32, contents: &str) {
            let path = self
                .root
                .join(format!("logs-{node_id}"))
                .join("ant-node.2026-08-19.log");
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap();
            file.write_all(contents.as_bytes()).unwrap();
        }

        fn config(&self) -> LogForwardConfig {
            LogForwardConfig {
                enabled: true,
                token: "test-key".to_string(),
                ..LogForwardConfig::disabled()
            }
        }

        fn offsets_path(&self) -> PathBuf {
            self.root.join("offsets.json")
        }
    }

    fn line(level: &str, message: &str) -> String {
        format!("2026-08-19T20:50:00.123456Z  {level} ant_node::node: {message}\n")
    }

    /// Wait until `condition` holds, or give up after `deadline`.
    ///
    /// Preferred over a fixed sleep wherever a test waits on the forwarder making progress: the
    /// work is real I/O over megabytes of log, so a duration chosen on an idle machine turns into a
    /// flake on a loaded CI runner. Returns whether the condition was met, so the caller can still
    /// assert on the actual value and produce a useful message.
    async fn wait_until(deadline: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let started = tokio::time::Instant::now();
        while started.elapsed() < deadline {
            if condition() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        condition()
    }

    /// Drive the forwarder for long enough to observe several poll cycles.
    async fn run_briefly(handle: &ForwarderHandle) {
        tokio::time::sleep(Duration::from_millis(220)).await;
        handle.stop();
        tokio::time::sleep(Duration::from_millis(60)).await;
    }

    #[tokio::test]
    async fn forwards_a_nodes_log_lines_to_the_sink() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );

        // Written after the forwarder has joined the file at its end.
        tokio::time::sleep(Duration::from_millis(60)).await;
        harness.append(1, &line("INFO", "hello from node one"));
        run_briefly(&handle).await;

        let ids = sink.submitted_ids();
        assert!(!ids.is_empty(), "nothing was forwarded");
        assert!(handle.snapshot().await.stats.events_forwarded >= 1);
    }

    /// A node with no log directory must not stop the forwarder doing its job for the others.
    #[tokio::test]
    async fn a_node_without_a_log_directory_does_not_disturb_the_rest() {
        let harness = Harness::new(&[true, false]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        harness.append(1, &line("INFO", "from the node that does log"));
        run_briefly(&handle).await;

        assert!(!sink.submitted_ids().is_empty());
        assert!(handle.snapshot().await.stats.events_forwarded >= 1);
    }

    #[tokio::test]
    async fn stopping_the_handle_ends_forwarding() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        handle.stop();
        assert!(handle.is_stopped());
        tokio::time::sleep(Duration::from_millis(60)).await;

        let batches_after_stop = sink.batch_count();
        harness.append(1, &line("INFO", "written after disable"));
        tokio::time::sleep(Duration::from_millis(120)).await;

        assert_eq!(
            sink.batch_count(),
            batches_after_stop,
            "disable must stop the flow entirely"
        );
    }

    /// `disable` must be a revocation boundary, not a request: once `stop_and_wait` returns, no
    /// request may still be in flight.
    ///
    /// The sink here hangs mid-send, standing in for a slow endpoint. Before cancellation reached
    /// the delivery loop, `disable` returned immediately and that send carried on through the whole
    /// retry ladder — up to three 30s request timeouts plus backoff — while the CLI had already
    /// told the user forwarding had stopped.
    #[tokio::test]
    async fn stopping_returns_only_once_delivery_has_actually_stopped() {
        let harness = Harness::new(&[true]).await;
        let release = Arc::new(tokio::sync::Notify::new());
        let sink = Arc::new(MockSink::blocking(release.clone()));

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        harness.append(1, &line("INFO", "caught mid-flight"));

        // Wait until a send is genuinely in flight and stuck.
        let mut waited = 0;
        while sink.batch_count() == 0 && waited < 60 {
            tokio::time::sleep(Duration::from_millis(20)).await;
            waited += 1;
        }
        assert_eq!(sink.batch_count(), 1, "a send should be in flight");
        assert_eq!(sink.completed_count(), 0, "and still blocked");

        // The blocked send is never released; this must still return promptly.
        let stopped = tokio::time::timeout(Duration::from_secs(5), handle.stop_and_wait()).await;
        assert!(
            stopped.is_ok(),
            "stop_and_wait must not sit through the retry ladder"
        );

        assert_eq!(
            sink.completed_count(),
            0,
            "the in-flight request must have been dropped, not allowed to finish"
        );

        // And nothing new may be sent afterwards.
        let batches_at_stop = sink.batch_count();
        harness.append(1, &line("INFO", "written after disable"));
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            sink.batch_count(),
            batches_at_stop,
            "no request may start after disable has returned"
        );
    }

    #[tokio::test]
    async fn the_daemon_shutdown_token_also_stops_forwarding() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());
        let shutdown = CancellationToken::new();

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            shutdown.clone(),
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        shutdown.cancel();
        tokio::time::sleep(Duration::from_millis(60)).await;
        let batches = sink.batch_count();

        harness.append(1, &line("INFO", "after shutdown"));
        tokio::time::sleep(Duration::from_millis(120)).await;

        assert_eq!(sink.batch_count(), batches);
        drop(handle);
    }

    /// Offsets are written on the way out, so the next daemon resumes instead of replaying.
    #[tokio::test]
    async fn offsets_are_persisted_across_a_forwarder_restart() {
        let harness = Harness::new(&[true]).await;
        harness.append(1, &line("INFO", "before"));

        let sink = Arc::new(MockSink::accepting());
        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
        harness.append(1, &line("INFO", "first run"));
        run_briefly(&handle).await;

        let first_ids = sink.submitted_ids();
        assert!(harness.offsets_path().exists(), "offsets must be persisted");

        // A second forwarder over the same offsets file must not resend what the first shipped.
        let second_sink = Arc::new(MockSink::accepting());
        let second = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            second_sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );
        run_briefly(&second).await;

        let resent: Vec<String> = second_sink
            .submitted_ids()
            .into_iter()
            .filter(|id| first_ids.contains(id))
            .collect();
        assert!(
            resent.is_empty(),
            "the second run re-sent documents the first had already delivered: {resent:?}"
        );
    }

    #[tokio::test]
    async fn events_below_the_minimum_level_never_reach_the_sink() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(30),
            CancellationToken::new(),
        );

        tokio::time::sleep(Duration::from_millis(60)).await;
        harness.append(1, &line("DEBUG", "chatter"));
        harness.append(1, &line("TRACE", "more chatter"));
        run_briefly(&handle).await;

        assert!(sink.submitted_ids().is_empty());
        assert!(handle.snapshot().await.stats.events_dropped_by_level >= 2);
    }

    #[tokio::test]
    async fn classify_nodes_orders_by_id_and_separates_by_logging() {
        // Nodes 1 and 3 have logging; node 2 does not.
        let harness = Harness::new(&[true, false, true]).await;
        let registry = harness.registry.read().await;
        let (forwarding, skipped) = classify_nodes(&registry);

        assert_eq!(
            forwarding.iter().map(|n| n.node_id).collect::<Vec<_>>(),
            vec![1, 3],
            "forwarding nodes are listed in id order"
        );
        assert_eq!(
            skipped.iter().map(|n| n.node_id).collect::<Vec<_>>(),
            vec![2]
        );
    }

    /// Reproduction of the beta cohort stall: a node that has written several megabytes since the
    /// forwarder last caught up must have all of it shipped, not just the first chunk.
    #[tokio::test]
    async fn a_multi_megabyte_backlog_is_shipped_in_full() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(20),
            CancellationToken::new(),
        );

        // Let the forwarder join the (empty) file at its end first.
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Now write ~3MB, the way a busy node does over an hour.
        let mut blob = String::new();
        let mut expected = 0usize;
        while blob.len() < 3 * 1024 * 1024 {
            blob.push_str(&line("INFO", &format!("replication event {expected} aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")));
            expected += 1;
        }
        harness.append(1, &blob);

        // 3MB is three chunks, so a correct forwarder finishes in a handful of cycles.
        wait_until(Duration::from_secs(30), || {
            sink.submitted_ids().len() >= expected
        })
        .await;
        handle.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let stats = handle.snapshot().await.stats;
        let shipped = sink.submitted_ids().len();
        eprintln!(
            "blob_bytes={} expected_events={expected} shipped={shipped} forwarded={} dropped_overflow={} dropped_level={} batches_sent={} batches_failed={} last_error={:?}",
            blob.len(), stats.events_forwarded, stats.events_dropped_by_overflow,
            stats.events_dropped_by_level, stats.batches_sent, stats.batches_failed, stats.last_error
        );
        assert_eq!(
            shipped, expected,
            "the whole backlog must be shipped, not just the first chunk"
        );
    }

    /// Each tailer prunes the *shared* offset store against only its own files, so a second node
    /// wipes the first node's positions every cycle.
    #[tokio::test]
    async fn a_second_node_does_not_wipe_the_first_nodes_offsets() {
        let harness = Harness::new(&[true, true]).await;
        let sink = Arc::new(MockSink::accepting());

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(20),
            CancellationToken::new(),
        );
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Node 1 is busy; node 2 is registered with a log dir but has written nothing.
        let mut blob = String::new();
        let mut expected = 0usize;
        while blob.len() < 3 * 1024 * 1024 {
            blob.push_str(&line("INFO", &format!("event {expected} aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")));
            expected += 1;
        }
        harness.append(1, &blob);

        wait_until(Duration::from_secs(30), || {
            sink.submitted_ids()
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                >= expected
        })
        .await;
        handle.stop();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let ids = sink.submitted_ids();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        let stats = handle.snapshot().await.stats;
        eprintln!(
            "expected={expected} submitted={} unique={} forwarded={} batches={}",
            ids.len(),
            unique.len(),
            stats.events_forwarded,
            stats.batches_sent
        );
        assert_eq!(
            unique.len(),
            expected,
            "node 1's whole backlog must ship; it stalled after {} distinct events \
             (re-sent {} times over)",
            unique.len(),
            ids.len() / unique.len().max(1)
        );
    }

    /// Retention deleting a daily file must still drop its offset -- now done once per cycle by the
    /// runner, against every tailer's files rather than one node's.
    #[tokio::test]
    async fn offsets_for_retention_deleted_files_are_pruned() {
        let harness = Harness::new(&[true]).await;
        let sink = Arc::new(MockSink::accepting());
        let log_dir = harness.root.join("logs-1");

        for name in ["ant-node.2026-08-18.log", "ant-node.2026-08-19.log"] {
            std::fs::write(log_dir.join(name), line("INFO", "hello")).unwrap();
        }

        let handle = spawn_log_forwarder(
            harness.registry.clone(),
            harness.config(),
            sink.clone(),
            harness.offsets_path(),
            Duration::from_millis(20),
            CancellationToken::new(),
        );
        let offsets_path = harness.offsets_path();
        wait_until(Duration::from_secs(30), || {
            OffsetStore::load(&offsets_path).len() == 2
        })
        .await;
        assert_eq!(
            OffsetStore::load(&offsets_path).len(),
            2,
            "both dailies are tracked"
        );

        std::fs::remove_file(log_dir.join("ant-node.2026-08-18.log")).unwrap();
        wait_until(Duration::from_secs(30), || {
            OffsetStore::load(&offsets_path).len() == 1
        })
        .await;
        handle.stop();
        tokio::time::sleep(Duration::from_millis(80)).await;

        assert_eq!(
            OffsetStore::load(&offsets_path).len(),
            1,
            "the deleted daily is pruned"
        );
    }
}
