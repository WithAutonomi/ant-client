//! Where documents go, and how hard the forwarder tries to get them there.
//!
//! This is logs-only telemetry, so the governing rule is that nothing here may grow without bound
//! or block waiting for an endpoint that is not answering. A user's node keeps running whatever the
//! ingest endpoint is doing; at worst they lose some log lines, which is a cost they can afford and
//! a stalled or memory-hungry daemon is not.
//!
//! Delivery is per-document rather than per-request. A bulk endpoint can accept most of a batch and
//! reject part of it, so a batch is retried by *position* — only the documents that asked to be
//! retried, never the whole thing. Whole-request replay is reserved for a transport failure, where
//! nothing is known about what landed, and is safe there only because every document carries a
//! deterministic `_id`.

use std::collections::VecDeque;
use std::time::Duration;

use futures::future::BoxFuture;

use super::document::ForwardDocument;

/// Maximum documents held in memory awaiting delivery.
///
/// At roughly a kilobyte per event this is a few megabytes — enough to ride out a short endpoint
/// outage, small enough that a long one costs the user nothing they would notice.
pub const DEFAULT_QUEUE_CAPACITY: usize = 10_000;

/// Maximum documents in one bulk request.
pub const DEFAULT_BATCH_DOCUMENTS: usize = 500;

/// Soft cap on a batch's serialized size. The endpoint's proxy rejects bodies over 50 MB and
/// Elasticsearch itself over 100 MB; a few megabytes stays far away from both.
pub const DEFAULT_BATCH_BYTES: usize = 4 * 1024 * 1024;

/// What happened to one submitted document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentOutcome {
    /// Indexed, or already present from an earlier attempt — either way, done with.
    Delivered,
    /// Worth another attempt: the endpoint was busy or briefly unavailable.
    Retryable,
    /// Rejected in a way that retrying cannot fix, e.g. a permissions or mapping error.
    Rejected,
}

/// Result of submitting one batch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BatchOutcome {
    /// Per-position outcomes, aligned with the submitted slice. Empty when the request itself
    /// failed and nothing can be said about individual documents.
    pub outcomes: Vec<DocumentOutcome>,
    /// Set when the request did not complete at all — connection refused, timed out, TLS failure.
    /// The whole batch may then be replayed, which the deterministic `_id`s make safe.
    pub transport_failure: bool,
    /// Human-readable description of the most relevant failure, for `status` output.
    pub error: Option<String>,
}

impl BatchOutcome {
    /// Every document accepted.
    #[must_use]
    pub fn all_delivered(count: usize) -> Self {
        Self {
            outcomes: vec![DocumentOutcome::Delivered; count],
            transport_failure: false,
            error: None,
        }
    }

    /// The request never completed.
    #[must_use]
    pub fn transport_failure(error: impl Into<String>) -> Self {
        Self {
            outcomes: Vec::new(),
            transport_failure: true,
            error: Some(error.into()),
        }
    }
}

/// Somewhere documents can be sent.
///
/// Boxed futures rather than `async fn` so the forwarder can hold a `dyn LogSink` and swap a mock
/// in under test without being generic over the sink everywhere.
pub trait LogSink: Send + Sync + 'static {
    fn send<'a>(&'a self, batch: &'a [ForwardDocument]) -> BoxFuture<'a, BatchOutcome>;

    /// Short description of the destination, for status output and logs.
    fn describe(&self) -> String;
}

/// A bounded in-memory queue of documents awaiting delivery.
///
/// When it fills, the **oldest** documents are dropped. Dropping the newest would be easier but
/// wrong: during an outage the recent events are the ones describing what is going wrong, and they
/// are what a beta debugger needs.
#[derive(Debug)]
pub struct DocumentQueue {
    documents: VecDeque<ForwardDocument>,
    capacity: usize,
    dropped: u64,
}

impl DocumentQueue {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            documents: VecDeque::new(),
            capacity: capacity.max(1),
            dropped: 0,
        }
    }

    /// Add a document, evicting the oldest if the queue is full.
    pub fn push(&mut self, document: ForwardDocument) {
        if self.documents.len() >= self.capacity {
            self.documents.pop_front();
            self.dropped += 1;
        }
        self.documents.push_back(document);
    }

    /// Take the next batch, bounded by both document count and approximate size.
    pub fn take_batch(&mut self, max_documents: usize, max_bytes: usize) -> Vec<ForwardDocument> {
        let mut batch = Vec::new();
        let mut bytes = 0;

        while batch.len() < max_documents {
            let Some(next) = self.documents.front() else {
                break;
            };
            let next_bytes = next.approx_bytes();
            // Always take at least one, so a single oversized document cannot wedge the queue.
            if !batch.is_empty() && bytes + next_bytes > max_bytes {
                break;
            }
            bytes += next_bytes;
            batch.push(self.documents.pop_front().expect("front was just observed"));
        }

        batch
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Documents discarded because the queue was full.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// How persistently a batch is retried before it is abandoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(30),
        }
    }
}

impl RetryPolicy {
    /// Backoff before the given attempt number, doubling and then holding at the cap.
    #[must_use]
    pub fn backoff_for(&self, attempt: u32) -> Duration {
        let exponent = attempt.saturating_sub(1).min(16);
        let scaled = self
            .initial_backoff
            .saturating_mul(2u32.saturating_pow(exponent));
        scaled.min(self.max_backoff)
    }
}

/// What became of one delivery attempt, including its retries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeliveryReport {
    pub delivered: u64,
    /// Documents the endpoint refused in a way retrying cannot fix.
    pub rejected: u64,
    /// Documents abandoned with retries exhausted.
    pub abandoned: u64,
    pub attempts: u32,
    pub error: Option<String>,
}

impl DeliveryReport {
    /// Whether every document in the batch was accounted for without loss.
    #[must_use]
    pub fn is_complete_success(&self) -> bool {
        self.rejected == 0 && self.abandoned == 0
    }
}

/// Deliver a batch, retrying only the positions that asked for it.
///
/// `sleep` is injected so tests exercise the retry ladder without waiting real seconds.
pub async fn deliver<S, F>(
    sink: &S,
    mut batch: Vec<ForwardDocument>,
    policy: RetryPolicy,
    mut sleep: F,
) -> DeliveryReport
where
    S: LogSink + ?Sized,
    F: FnMut(Duration) -> BoxFuture<'static, ()>,
{
    let mut report = DeliveryReport::default();

    for attempt in 1..=policy.max_attempts {
        report.attempts = attempt;
        let outcome = sink.send(&batch).await;

        if outcome.transport_failure {
            report.error = outcome.error;
            if attempt < policy.max_attempts {
                sleep(policy.backoff_for(attempt)).await;
                continue;
            }
            // Nothing is known about what landed. The batch is abandoned rather than replayed
            // forever; a later attempt at the same documents would be idempotent, but holding them
            // indefinitely is what unbounded memory looks like.
            report.abandoned += batch.len() as u64;
            return report;
        }

        if outcome.error.is_some() {
            report.error = outcome.error.clone();
        }

        let mut retry = Vec::new();
        for (index, document) in batch.into_iter().enumerate() {
            match outcome.outcomes.get(index) {
                Some(DocumentOutcome::Delivered) => report.delivered += 1,
                Some(DocumentOutcome::Rejected) => report.rejected += 1,
                Some(DocumentOutcome::Retryable) => retry.push(document),
                // A response shorter than the batch: treat the unexplained tail as retryable rather
                // than assume it landed.
                None => retry.push(document),
            }
        }

        if retry.is_empty() {
            return report;
        }

        batch = retry;
        if attempt < policy.max_attempts {
            sleep(policy.backoff_for(attempt)).await;
        }
    }

    report.abandoned += batch.len() as u64;
    report
}

/// A sink that records what it was given, for tests.
#[cfg(test)]
pub mod mock {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Records every batch and replies from a script of prepared outcomes.
    pub struct MockSink {
        pub batches: Mutex<Vec<Vec<ForwardDocument>>>,
        responses: Mutex<VecDeque<BatchOutcome>>,
        /// When set, `send` records the batch and then blocks until the notifier fires, standing in
        /// for a request that is in flight when forwarding is revoked.
        block_until: Option<Arc<tokio::sync::Notify>>,
        /// Batches whose `send` actually returned, as opposed to being dropped mid-flight.
        pub completed: Mutex<usize>,
    }

    impl MockSink {
        /// A sink that accepts everything.
        #[must_use]
        pub fn accepting() -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                block_until: None,
                completed: Mutex::new(0),
            }
        }

        /// A sink that replies with each prepared outcome in turn, accepting everything after.
        #[must_use]
        pub fn scripted(responses: Vec<BatchOutcome>) -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                responses: Mutex::new(responses.into()),
                block_until: None,
                completed: Mutex::new(0),
            }
        }

        /// A sink whose sends hang until `release` is notified.
        #[must_use]
        pub fn blocking(release: Arc<tokio::sync::Notify>) -> Self {
            Self {
                batches: Mutex::new(Vec::new()),
                responses: Mutex::new(VecDeque::new()),
                block_until: Some(release),
                completed: Mutex::new(0),
            }
        }

        /// How many sends ran to completion rather than being dropped mid-flight.
        #[must_use]
        pub fn completed_count(&self) -> usize {
            *self.completed.lock().unwrap()
        }

        /// Ids of every document submitted, in submission order, across all batches.
        #[must_use]
        pub fn submitted_ids(&self) -> Vec<String> {
            self.batches
                .lock()
                .unwrap()
                .iter()
                .flat_map(|batch| batch.iter().map(|d| d.id.clone()))
                .collect()
        }

        #[must_use]
        pub fn batch_count(&self) -> usize {
            self.batches.lock().unwrap().len()
        }
    }

    impl LogSink for MockSink {
        fn send<'a>(&'a self, batch: &'a [ForwardDocument]) -> BoxFuture<'a, BatchOutcome> {
            Box::pin(async move {
                self.batches.lock().unwrap().push(batch.to_vec());

                if let Some(release) = &self.block_until {
                    // Dropping this future here is what a cancelled delivery looks like: the
                    // completion counter below is never reached.
                    release.notified().await;
                }

                *self.completed.lock().unwrap() += 1;
                self.responses
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| BatchOutcome::all_delivered(batch.len()))
            })
        }

        fn describe(&self) -> String {
            "mock".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::MockSink;
    use super::*;
    use crate::node::daemon::forward::document::DocumentSource;

    fn document(id: &str) -> ForwardDocument {
        ForwardDocument {
            id: id.to_string(),
            index: "beta-nodes-2026.08.19".to_string(),
            source: DocumentSource {
                timestamp: "2026-08-19T20:50:00.000000Z".to_string(),
                level: "INFO".to_string(),
                target: None,
                message: "m".to_string(),
                node_id: "7".to_string(),
                service: "node7".to_string(),
                binary_version: "0.17.2".to_string(),
                channel: "beta".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                peer_id: None,
                version: None,
                commit: None,
            },
        }
    }

    fn documents(count: usize) -> Vec<ForwardDocument> {
        (0..count).map(|i| document(&format!("doc-{i}"))).collect()
    }

    /// Sleep that returns instantly, recording nothing — the retry ladder is exercised, not waited on.
    fn no_sleep() -> impl FnMut(Duration) -> BoxFuture<'static, ()> {
        |_| Box::pin(async {})
    }

    #[test]
    fn a_full_queue_drops_the_oldest_not_the_newest() {
        let mut queue = DocumentQueue::new(3);
        for i in 0..5 {
            queue.push(document(&format!("doc-{i}")));
        }

        assert_eq!(queue.len(), 3);
        assert_eq!(queue.dropped(), 2);

        let batch = queue.take_batch(10, usize::MAX);
        let ids: Vec<&str> = batch.iter().map(|d| d.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["doc-2", "doc-3", "doc-4"],
            "the recent events are the ones worth keeping"
        );
    }

    #[test]
    fn a_batch_is_bounded_by_document_count() {
        let mut queue = DocumentQueue::new(100);
        for doc in documents(10) {
            queue.push(doc);
        }

        assert_eq!(queue.take_batch(4, usize::MAX).len(), 4);
        assert_eq!(queue.len(), 6);
    }

    #[test]
    fn a_batch_is_bounded_by_approximate_size() {
        let mut queue = DocumentQueue::new(100);
        for doc in documents(10) {
            queue.push(doc);
        }

        let one = document("sizing").approx_bytes();
        let batch = queue.take_batch(100, one * 3);
        assert_eq!(batch.len(), 3);
    }

    /// A single document larger than the whole size budget must still get out.
    #[test]
    fn an_oversized_document_cannot_wedge_the_queue() {
        let mut queue = DocumentQueue::new(10);
        queue.push(document("huge"));

        let batch = queue.take_batch(100, 1);
        assert_eq!(batch.len(), 1);
        assert!(queue.is_empty());
    }

    #[tokio::test]
    async fn a_clean_batch_is_delivered_in_one_attempt() {
        let sink = MockSink::accepting();
        let report = deliver(&sink, documents(3), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.delivered, 3);
        assert_eq!(report.attempts, 1);
        assert!(report.is_complete_success());
        assert_eq!(sink.batch_count(), 1);
    }

    /// The behaviour the ingest contract specifically warned about: a 200 response can still carry
    /// per-document failures, and only the failing positions may be resent.
    #[tokio::test]
    async fn only_the_retryable_positions_are_resent() {
        let sink = MockSink::scripted(vec![BatchOutcome {
            outcomes: vec![
                DocumentOutcome::Delivered,
                DocumentOutcome::Retryable,
                DocumentOutcome::Delivered,
                DocumentOutcome::Retryable,
            ],
            transport_failure: false,
            error: None,
        }]);

        let report = deliver(&sink, documents(4), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.delivered, 4, "2 first time, 2 on the retry");
        assert_eq!(report.attempts, 2);

        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches[0].len(), 4);
        assert_eq!(batches[1].len(), 2, "only the failures are resent");
        let resent: Vec<&str> = batches[1].iter().map(|d| d.id.as_str()).collect();
        assert_eq!(resent, vec!["doc-1", "doc-3"]);
    }

    #[tokio::test]
    async fn a_permanently_rejected_document_is_not_retried() {
        let sink = MockSink::scripted(vec![BatchOutcome {
            outcomes: vec![DocumentOutcome::Rejected, DocumentOutcome::Delivered],
            transport_failure: false,
            error: Some("403 forbidden".to_string()),
        }]);

        let report = deliver(&sink, documents(2), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.rejected, 1);
        assert_eq!(report.delivered, 1);
        assert_eq!(report.attempts, 1, "no point trying again");
        assert!(!report.is_complete_success());
        assert_eq!(report.error.as_deref(), Some("403 forbidden"));
    }

    #[tokio::test]
    async fn retries_are_abandoned_once_the_policy_is_exhausted() {
        let always_busy = BatchOutcome {
            outcomes: vec![DocumentOutcome::Retryable, DocumentOutcome::Retryable],
            transport_failure: false,
            error: Some("429 too many requests".to_string()),
        };
        let sink = MockSink::scripted(vec![always_busy.clone(), always_busy.clone(), always_busy]);

        let policy = RetryPolicy {
            max_attempts: 3,
            ..RetryPolicy::default()
        };
        let report = deliver(&sink, documents(2), policy, no_sleep()).await;

        assert_eq!(report.attempts, 3);
        assert_eq!(report.abandoned, 2);
        assert_eq!(report.delivered, 0);
        assert_eq!(sink.batch_count(), 3);
    }

    /// The whole batch may be replayed after a transport failure precisely because every document
    /// carries a stable `_id`, so the second attempt cannot duplicate the first.
    #[tokio::test]
    async fn a_transport_failure_replays_the_whole_batch_with_identical_ids() {
        let sink = MockSink::scripted(vec![BatchOutcome::transport_failure("connection reset")]);

        let report = deliver(&sink, documents(3), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.delivered, 3);
        assert_eq!(report.attempts, 2);

        let batches = sink.batches.lock().unwrap();
        let first: Vec<&str> = batches[0].iter().map(|d| d.id.as_str()).collect();
        let second: Vec<&str> = batches[1].iter().map(|d| d.id.as_str()).collect();
        assert_eq!(first, second, "a replay must reuse the same document ids");
    }

    #[tokio::test]
    async fn a_batch_is_abandoned_when_transport_failures_persist() {
        let sink = MockSink::scripted(vec![
            BatchOutcome::transport_failure("refused"),
            BatchOutcome::transport_failure("refused"),
            BatchOutcome::transport_failure("refused"),
        ]);

        let report = deliver(&sink, documents(2), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.abandoned, 2);
        assert_eq!(report.delivered, 0);
        assert_eq!(report.error.as_deref(), Some("refused"));
    }

    /// A response with fewer entries than the batch says nothing about the tail; assuming success
    /// there would silently lose documents.
    #[tokio::test]
    async fn an_unexplained_tail_is_retried_rather_than_assumed_delivered() {
        let sink = MockSink::scripted(vec![BatchOutcome {
            outcomes: vec![DocumentOutcome::Delivered],
            transport_failure: false,
            error: None,
        }]);

        let report = deliver(&sink, documents(3), RetryPolicy::default(), no_sleep()).await;

        assert_eq!(report.delivered, 3);
        let batches = sink.batches.lock().unwrap();
        assert_eq!(batches[1].len(), 2);
    }

    #[test]
    fn backoff_doubles_then_holds_at_the_ceiling() {
        let policy = RetryPolicy {
            max_attempts: 10,
            initial_backoff: Duration::from_secs(2),
            max_backoff: Duration::from_secs(10),
        };

        assert_eq!(policy.backoff_for(1), Duration::from_secs(2));
        assert_eq!(policy.backoff_for(2), Duration::from_secs(4));
        assert_eq!(policy.backoff_for(3), Duration::from_secs(8));
        assert_eq!(policy.backoff_for(4), Duration::from_secs(10));
        assert_eq!(policy.backoff_for(9), Duration::from_secs(10));
    }
}
