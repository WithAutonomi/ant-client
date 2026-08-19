//! End-to-end tests for beta log forwarding (V2-1021), driven against a real HTTP endpoint that
//! speaks the V2-1016 Elasticsearch bulk contract.
//!
//! The endpoint itself (`logs.autonomi.com`) is still being provisioned, so these stand in for it:
//! an axum server that frames its replies exactly as the real one does — HTTP 200 even for failed
//! documents, per-position `items[].status`, and the forced
//! `filter_path=errors,items.*.status,items.*.error` response shape. Getting those wrong is the
//! failure mode the contract review specifically warned about, so they are what is asserted here.

use std::collections::HashMap;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::Router;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use ant_core::node::daemon::forward::{
    spawn_log_forwarder, ElasticsearchSink, LogForwardConfig, LogLevel,
};
use ant_core::node::registry::NodeRegistry;
use ant_core::node::types::{EvmNetwork, NodeConfig, UpgradeChannel};

/// One document as the endpoint received it: its bulk action line and its source line.
#[derive(Debug, Clone)]
struct ReceivedDocument {
    action: serde_json::Value,
    source: serde_json::Value,
}

impl ReceivedDocument {
    fn id(&self) -> String {
        self.action["create"]["_id"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn index(&self) -> String {
        self.action["create"]["_index"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }

    fn message(&self) -> String {
        self.source["message"].as_str().unwrap_or("").to_string()
    }
}

#[derive(Default)]
struct EndpointState {
    documents: Vec<ReceivedDocument>,
    auth_headers: Vec<String>,
    content_types: Vec<String>,
    bodies: Vec<String>,
    /// Number of the next request to fail outright, simulating a dropped connection.
    fail_request_numbers: Vec<usize>,
    request_count: usize,
}

/// A stand-in for the beta ingest endpoint.
struct MockEndpoint {
    addr: SocketAddr,
    state: Arc<Mutex<EndpointState>>,
    shutdown: CancellationToken,
}

impl MockEndpoint {
    async fn start(fail_request_numbers: Vec<usize>) -> Self {
        let state = Arc::new(Mutex::new(EndpointState {
            fail_request_numbers,
            ..EndpointState::default()
        }));
        let shutdown = CancellationToken::new();

        let app = Router::new()
            .route("/_bulk", post(handle_bulk))
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(serve_shutdown.cancelled_owned())
                .await
                .ok();
        });

        Self {
            addr,
            state,
            shutdown,
        }
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn documents(&self) -> Vec<ReceivedDocument> {
        self.state.lock().unwrap().documents.clone()
    }

    fn auth_headers(&self) -> Vec<String> {
        self.state.lock().unwrap().auth_headers.clone()
    }

    fn content_types(&self) -> Vec<String> {
        self.state.lock().unwrap().content_types.clone()
    }

    fn bodies(&self) -> Vec<String> {
        self.state.lock().unwrap().bodies.clone()
    }

    fn stop(&self) {
        self.shutdown.cancel();
    }
}

/// Parse an NDJSON bulk body and answer the way the real endpoint does.
async fn handle_bulk(
    State(state): State<Arc<Mutex<EndpointState>>>,
    headers: axum::http::HeaderMap,
    body: String,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let mut guard = state.lock().unwrap();
    guard.request_count += 1;
    let request_number = guard.request_count;

    guard.auth_headers.push(
        headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    guard.content_types.push(
        headers
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string(),
    );
    guard.bodies.push(body.clone());

    // Simulate a request that dies before the endpoint can answer.
    if guard.fail_request_numbers.contains(&request_number) {
        return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
    }

    let mut lines = body.lines();
    let mut statuses = Vec::new();

    while let (Some(action_line), Some(source_line)) = (lines.next(), lines.next()) {
        let action: serde_json::Value = serde_json::from_str(action_line).unwrap();
        let source: serde_json::Value = serde_json::from_str(source_line).unwrap();
        let document = ReceivedDocument { action, source };

        // `create` semantics: a document whose id is already present is a conflict, not an
        // overwrite. This is what makes a replayed batch idempotent.
        let already_present = guard.documents.iter().any(|d| d.id() == document.id());
        statuses.push(if already_present { 409 } else { 201 });

        if !already_present {
            guard.documents.push(document);
        }
    }

    let errors = statuses.iter().any(|s| *s != 201);
    let body = if errors {
        // The forced filter_path shape: one entry per submitted document, positions preserved.
        let items: Vec<serde_json::Value> = statuses
            .iter()
            .map(|status| serde_json::json!({ "create": { "status": status } }))
            .collect();
        serde_json::json!({ "errors": true, "items": items })
    } else {
        serde_json::json!({ "errors": false })
    };

    // Note the 200: a bulk response is a success at the HTTP layer even when documents failed.
    (axum::http::StatusCode::OK, axum::Json(body)).into_response()
}

/// A registry with one logging-enabled node, plus somewhere to write its log files.
struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    registry: Arc<RwLock<NodeRegistry>>,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let log_dir = root.join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let mut registry = NodeRegistry::load(&root.join("registry.json")).unwrap();
        registry.add(NodeConfig {
            id: 1,
            service_name: "node1".to_string(),
            rewards_address: "0x1234567890abcdef1234567890abcdef12345678".to_string(),
            data_dir: root.join("data"),
            log_dir: Some(log_dir),
            node_port: None,
            binary_path: root.join("antnode"),
            version: "0.17.2-beta.1".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: Vec::new(),
            upgrade_channel: Some(UpgradeChannel::Beta),
            evm_network: EvmNetwork::default(),
            eviction: None,
        });

        Self {
            _dir: dir,
            root,
            registry: Arc::new(RwLock::new(registry)),
        }
    }

    fn log_path(&self, day: &str) -> PathBuf {
        self.root.join("logs").join(format!("ant-node.{day}.log"))
    }

    fn append(&self, day: &str, contents: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path(day))
            .unwrap();
        file.write_all(contents.as_bytes()).unwrap();
    }

    fn offsets_path(&self) -> PathBuf {
        self.root.join("offsets.json")
    }

    fn config(&self, endpoint: &str) -> LogForwardConfig {
        LogForwardConfig {
            enabled: true,
            token: "beta-write-key".to_string(),
            endpoint: endpoint.to_string(),
            index_prefix: "beta-nodes".to_string(),
            min_level: LogLevel::Info,
        }
    }
}

fn line(day: &str, time: &str, level: &str, message: &str) -> String {
    format!("{day}T{time}.000000Z  {level} ant_node::node: {message}\n")
}

const POLL: Duration = Duration::from_millis(40);

/// Run a forwarder against the endpoint for long enough to see several cycles.
async fn forward_for(
    fixture: &Fixture,
    endpoint_base: &str,
    offsets_path: &Path,
    write: impl FnOnce(),
    settle: Duration,
) {
    let config = fixture.config(endpoint_base);
    let sink = Arc::new(ElasticsearchSink::new(config.endpoint_base(), &config.token).unwrap());

    let handle = spawn_log_forwarder(
        fixture.registry.clone(),
        config,
        sink,
        offsets_path.to_path_buf(),
        POLL,
        CancellationToken::new(),
    );

    // Let the forwarder join the file at its end before anything is written, so the test exercises
    // live tailing rather than the initial adoption path.
    tokio::time::sleep(Duration::from_millis(100)).await;
    write();
    tokio::time::sleep(settle).await;

    handle.stop();
    tokio::time::sleep(Duration::from_millis(120)).await;
}

#[tokio::test]
async fn node_logs_reach_the_endpoint_correctly_framed_and_tagged() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &line("2026-08-19", "20:50:00", "INFO", "connected to the network"),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let documents = endpoint.documents();
    assert_eq!(documents.len(), 1, "expected exactly one document");
    let document = &documents[0];

    // Framing: the action must be `create`; `index` is refused with a per-item 403.
    assert!(
        document.action.get("create").is_some(),
        "the bulk action must be `create`, got {:?}",
        document.action
    );
    assert_eq!(document.index(), "beta-nodes-2026.08.19");
    assert_eq!(document.id(), "1-ant-node.2026-08-19.log-0");

    // Tagging, using the index's own field names.
    assert_eq!(document.source["@timestamp"], "2026-08-19T20:50:00.000000Z");
    assert!(
        document.source.get("timestamp").is_none(),
        "the time field is @timestamp, not timestamp"
    );
    assert_eq!(document.source["level"], "INFO");
    assert_eq!(document.source["message"], "connected to the network");
    assert_eq!(document.source["node_id"], "1");
    assert_eq!(document.source["service"], "node1");
    assert_eq!(document.source["binary_version"], "0.17.2-beta.1");
    assert_eq!(document.source["channel"], "beta");
    assert_eq!(document.source["os"], std::env::consts::OS);
    assert_eq!(document.source["arch"], std::env::consts::ARCH);

    // Both are server-side concerns; sending either would be discarded or overwritten.
    assert!(document.source.get("host").is_none());
    assert!(document.source.get("beta_user").is_none());

    // Transport details the contract fixes.
    assert_eq!(endpoint.auth_headers()[0], "ApiKey beta-write-key");
    assert_eq!(endpoint.content_types()[0], "application/x-ndjson");
    assert!(
        endpoint.bodies()[0].ends_with('\n'),
        "the bulk body must end with a newline"
    );

    endpoint.stop();
}

#[tokio::test]
async fn events_below_info_are_never_shipped() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &format!(
                    "{}{}{}",
                    line("2026-08-19", "20:50:00", "DEBUG", "chatter"),
                    line("2026-08-19", "20:50:01", "TRACE", "more chatter"),
                    line("2026-08-19", "20:50:02", "WARN", "worth keeping"),
                ),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let messages: Vec<String> = endpoint.documents().iter().map(|d| d.message()).collect();
    assert_eq!(
        messages,
        vec!["worth keeping"],
        "the endpoint drops sub-INFO events anyway; sending them wastes the user's bandwidth"
    );

    endpoint.stop();
}

/// The V2-1021 acceptance criterion: a daemon restart must not duplicate or lose events.
#[tokio::test]
async fn a_restart_neither_duplicates_nor_loses_events() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();
    let offsets = fixture.offsets_path();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &offsets,
        || {
            fixture.append(
                "2026-08-19",
                &line("2026-08-19", "20:50:00", "INFO", "before the restart"),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    assert_eq!(endpoint.documents().len(), 1);

    // Written while no forwarder is running — the "gap" half of the criterion.
    fixture.append(
        "2026-08-19",
        &line(
            "2026-08-19",
            "20:51:00",
            "INFO",
            "while the daemon was down",
        ),
    );

    // A second forwarder over the same persisted offsets: a restart, not a fresh enable.
    forward_for(
        &fixture,
        &endpoint.base_url(),
        &offsets,
        || {
            fixture.append(
                "2026-08-19",
                &line("2026-08-19", "20:52:00", "INFO", "after the restart"),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let messages: Vec<String> = endpoint.documents().iter().map(|d| d.message()).collect();
    assert_eq!(
        messages,
        vec![
            "before the restart",
            "while the daemon was down",
            "after the restart"
        ],
        "every event exactly once, in order"
    );

    let ids: Vec<String> = endpoint.documents().iter().map(|d| d.id()).collect();
    let unique: std::collections::HashSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len(), "no document was written twice");

    endpoint.stop();
}

/// A request that dies in flight is replayed wholesale, which is only safe because the ids are
/// deterministic: the endpoint answers the replay with 409s rather than storing a second copy.
#[tokio::test]
async fn a_failed_request_is_replayed_without_duplicating_documents() {
    // Fail the first request outright.
    let endpoint = MockEndpoint::start(vec![1]).await;
    let fixture = Fixture::new();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &line("2026-08-19", "20:50:00", "INFO", "survives a failed send"),
            );
        },
        // Long enough to cover the retry policy's 2s initial backoff. The wait is the point of the
        // test: a shorter one would pass by never reaching the retry at all.
        Duration::from_millis(3_500),
    )
    .await;

    let documents = endpoint.documents();
    assert_eq!(
        documents.len(),
        1,
        "the replay must not store a second copy: {:?}",
        documents
            .iter()
            .map(ReceivedDocument::id)
            .collect::<Vec<_>>()
    );
    assert_eq!(documents[0].message(), "survives a failed send");
    assert!(
        endpoint.bodies().len() >= 2,
        "the batch should have been retried after the failure"
    );

    endpoint.stop();
}

/// Each document is filed by its own timestamp, so an event written just before midnight lands in
/// yesterday's index even though it ships today. This is what keeps a replayed batch landing on the
/// same `_id` in the same index.
#[tokio::test]
async fn documents_are_indexed_by_their_own_date_not_the_wall_clock() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &format!(
                    "{}{}",
                    line("2026-08-19", "23:59:59", "INFO", "just before midnight"),
                    line("2026-08-20", "00:00:01", "INFO", "just after midnight"),
                ),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let documents = endpoint.documents();
    assert_eq!(documents.len(), 2);
    assert_eq!(documents[0].index(), "beta-nodes-2026.08.19");
    assert_eq!(documents[1].index(), "beta-nodes-2026.08.20");

    endpoint.stop();
}

/// Daily rotation is a new file appearing beside the old one, and the tailer must pick it up
/// without being restarted.
#[tokio::test]
async fn a_days_rotation_is_followed_into_the_new_file() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    let config = fixture.config(&endpoint.base_url());
    let sink = Arc::new(ElasticsearchSink::new(config.endpoint_base(), &config.token).unwrap());
    let handle = spawn_log_forwarder(
        fixture.registry.clone(),
        config,
        sink,
        fixture.offsets_path(),
        POLL,
        CancellationToken::new(),
    );

    tokio::time::sleep(Duration::from_millis(100)).await;
    fixture.append(
        "2026-08-19",
        &line("2026-08-19", "23:59:00", "INFO", "end of the day"),
    );
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Rotation: a new file, not a moved cursor.
    fixture.append(
        "2026-08-20",
        &line("2026-08-20", "00:00:30", "INFO", "start of the next"),
    );
    tokio::time::sleep(Duration::from_millis(400)).await;

    handle.stop();
    tokio::time::sleep(Duration::from_millis(120)).await;

    let documents = endpoint.documents();
    let messages: Vec<String> = documents.iter().map(|d| d.message()).collect();
    assert_eq!(messages, vec!["end of the day", "start of the next"]);
    assert!(documents[1].id().contains("ant-node.2026-08-20.log"));

    endpoint.stop();
}

/// A panic and its backtrace must arrive as one document, not as an event followed by orphan lines.
#[tokio::test]
async fn a_multi_line_event_arrives_as_a_single_document() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &format!(
                    "{}thread 'main' panicked at src/node.rs:42\n  stack frame one\n",
                    line("2026-08-19", "20:50:00", "ERROR", "the node fell over"),
                ),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let documents = endpoint.documents();
    assert_eq!(
        documents.len(),
        1,
        "the backtrace must not become its own document"
    );
    let message = documents[0].message();
    assert!(message.contains("the node fell over"), "{message}");
    assert!(message.contains("thread 'main' panicked"), "{message}");
    assert!(message.contains("stack frame one"), "{message}");

    endpoint.stop();
}

/// Enabling forwarding is forward-looking consent: it must not upload the retained backlog.
#[tokio::test]
async fn enabling_does_not_upload_the_existing_backlog() {
    let endpoint = MockEndpoint::start(Vec::new()).await;
    let fixture = Fixture::new();

    fixture.append(
        "2026-08-19",
        &line("2026-08-19", "10:00:00", "INFO", "logged before consent"),
    );

    forward_for(
        &fixture,
        &endpoint.base_url(),
        &fixture.offsets_path(),
        || {
            fixture.append(
                "2026-08-19",
                &line("2026-08-19", "20:50:00", "INFO", "logged after consent"),
            );
        },
        Duration::from_millis(400),
    )
    .await;

    let messages: Vec<String> = endpoint.documents().iter().map(|d| d.message()).collect();
    assert_eq!(messages, vec!["logged after consent"]);

    endpoint.stop();
}
