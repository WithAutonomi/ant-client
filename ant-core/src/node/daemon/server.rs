use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::node::binary::NoopProgress;
use crate::node::daemon::supervisor::Supervisor;
use crate::node::events::NodeEvent;
use crate::node::registry::NodeRegistry;
use crate::node::types::{
    AddNodeOpts, AddNodeResult, DaemonConfig, DaemonStatus, NodeStarted, NodeStatus,
    NodeStatusResult, NodeStatusSummary, NodeStopped, RemoveNodeResult, ResetResult,
    StartNodeResult, StopNodeResult,
};

/// Shared application state for the daemon HTTP server.
pub struct AppState {
    pub registry: RwLock<NodeRegistry>,
    pub supervisor: Arc<RwLock<Supervisor>>,
    pub event_tx: broadcast::Sender<NodeEvent>,
    pub start_time: Instant,
    pub config: DaemonConfig,
    /// The actual address the server bound to (resolves port 0 to real port).
    pub bound_port: u16,
}

/// Start the daemon HTTP server.
///
/// Returns the actual address the server bound to (useful when port is 0).
pub async fn start(
    config: DaemonConfig,
    registry: NodeRegistry,
    shutdown: CancellationToken,
) -> Result<SocketAddr> {
    let (event_tx, _) = broadcast::channel(256);

    let addr = SocketAddr::new(config.listen_addr, config.port.unwrap_or(0));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::Error::BindError(e.to_string()))?;
    let bound_addr = listener
        .local_addr()
        .map_err(|e| crate::error::Error::BindError(e.to_string()))?;

    let state = Arc::new(AppState {
        registry: RwLock::new(registry),
        supervisor: Arc::new(RwLock::new(Supervisor::new(event_tx.clone()))),
        event_tx,
        start_time: Instant::now(),
        config: config.clone(),
        bound_port: bound_addr.port(),
    });

    let app = build_router(state.clone());

    // Write port and PID files
    write_file(&config.port_file_path, &bound_addr.port().to_string())?;
    write_file(&config.pid_file_path, &std::process::id().to_string())?;

    let port_file = config.port_file_path.clone();
    let pid_file = config.pid_file_path.clone();

    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .ok();

        // Clean up port and PID files on shutdown
        let _ = std::fs::remove_file(&port_file);
        let _ = std::fs::remove_file(&pid_file);
    });

    Ok(bound_addr)
}

fn build_router(state: Arc<AppState>) -> Router {
    use axum::http::HeaderValue;
    use tower_http::cors::{Any, CorsLayer};

    // Restrict CORS to the daemon's own origin to prevent cross-origin CSRF
    // attacks from malicious webpages. Non-browser clients (CLI, AI agents)
    // don't send Origin headers so CORS doesn't affect them.
    let origin = format!("http://127.0.0.1:{}", state.bound_port);
    let cors = CorsLayer::new()
        .allow_origin(origin.parse::<HeaderValue>().unwrap())
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        .route("/console", get(get_console))
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/events", get(get_events))
        .route("/api/v1/nodes/status", get(get_nodes_status))
        .route("/api/v1/nodes", post(post_nodes))
        .route("/api/v1/nodes/{id}", delete(delete_node))
        .route("/api/v1/nodes/{id}/start", post(post_start_node))
        .route("/api/v1/nodes/start-all", post(post_start_all))
        .route("/api/v1/nodes/{id}/stop", post(post_stop_node))
        .route("/api/v1/nodes/stop-all", post(post_stop_all))
        .route("/api/v1/reset", post(post_reset))
        .route("/api/v1/openapi.json", get(get_openapi))
        .layer(cors)
        .with_state(state)
}

async fn get_status(State(state): State<Arc<AppState>>) -> Json<DaemonStatus> {
    let registry = state.registry.read().await;
    let supervisor = state.supervisor.read().await;
    let (running, stopped, errored) = supervisor.node_counts();

    Json(DaemonStatus {
        running: true,
        pid: Some(std::process::id()),
        port: Some(state.bound_port),
        uptime_secs: Some(state.start_time.elapsed().as_secs()),
        nodes_total: registry.len() as u32,
        nodes_running: running,
        nodes_stopped: stopped,
        nodes_errored: errored,
    })
}

async fn get_events(
    State(state): State<Arc<AppState>>,
) -> Sse<impl futures_core::Stream<Item = std::result::Result<Event, std::convert::Infallible>>> {
    let mut rx = state.event_tx.subscribe();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let event_type = event.event_type().to_string();
                    if let Ok(data) = serde_json::to_string(&event) {
                        yield Ok(Event::default().event(event_type).data(data));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

/// GET /api/v1/nodes/status — Get status of all registered nodes.
async fn get_nodes_status(State(state): State<Arc<AppState>>) -> Json<NodeStatusResult> {
    let registry = state.registry.read().await;
    let supervisor = state.supervisor.read().await;

    let mut nodes = Vec::new();
    let mut total_running = 0u32;
    let mut total_stopped = 0u32;

    for config in registry.list() {
        let status = supervisor
            .node_status(config.id)
            .unwrap_or(NodeStatus::Stopped);

        match status {
            NodeStatus::Running | NodeStatus::Starting => total_running += 1,
            _ => total_stopped += 1,
        }

        nodes.push(NodeStatusSummary {
            node_id: config.id,
            name: config.service_name.clone(),
            version: config.version.clone(),
            status,
        });
    }

    Json(NodeStatusResult {
        nodes,
        total_running,
        total_stopped,
    })
}

/// POST /api/v1/nodes — Add one or more nodes to the registry.
async fn post_nodes(
    State(state): State<Arc<AppState>>,
    Json(opts): Json<AddNodeOpts>,
) -> std::result::Result<(StatusCode, Json<AddNodeResult>), (StatusCode, Json<serde_json::Value>)> {
    let registry_path = state.config.registry_path.clone();
    let progress = NoopProgress;

    match crate::node::add_nodes(opts, &registry_path, &progress).await {
        Ok(result) => {
            // Update the in-memory registry to stay in sync
            let mut registry = state.registry.write().await;
            if let Ok(fresh) = NodeRegistry::load(&registry_path) {
                *registry = fresh;
            }
            Ok((StatusCode::CREATED, Json(result)))
        }
        Err(e) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// DELETE /api/v1/nodes/:id — Remove a node from the registry.
async fn delete_node(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
) -> std::result::Result<Json<RemoveNodeResult>, (StatusCode, Json<serde_json::Value>)> {
    // Prevent removing a running node (would orphan the process)
    let supervisor = state.supervisor.read().await;
    if supervisor.is_running(id) {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Cannot remove node {id} while it is running. Stop it first."),
                "current_state": { "node_id": id, "status": "running" }
            })),
        ));
    }
    drop(supervisor);

    let registry_path = state.config.registry_path.clone();

    match crate::node::remove_node(id, &registry_path) {
        Ok(result) => {
            // Update the in-memory registry to stay in sync
            let mut registry = state.registry.write().await;
            if let Ok(fresh) = NodeRegistry::load(&registry_path) {
                *registry = fresh;
            }
            Ok(Json(result))
        }
        Err(crate::error::Error::NodeNotFound(id)) => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("Node not found: {id}") })),
        )),
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/v1/nodes/:id/start — Start a specific node.
async fn post_start_node(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
) -> std::result::Result<Json<NodeStarted>, (StatusCode, Json<serde_json::Value>)> {
    let registry = state.registry.read().await;
    let config = match registry.get(id) {
        Ok(config) => config.clone(),
        Err(_) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Node not found: {id}") })),
            ))
        }
    };
    drop(registry);

    let supervisor_ref = state.supervisor.clone();

    // Acquire write lock once for atomic check-and-act (avoids TOCTOU race)
    let mut supervisor = state.supervisor.write().await;
    if supervisor.is_running(id) {
        let pid = supervisor.node_pid(id);
        let uptime_secs = supervisor.node_uptime_secs(id);
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Node {id} is already running"),
                "current_state": {
                    "node_id": id,
                    "status": "running",
                    "pid": pid,
                    "uptime_secs": uptime_secs,
                }
            })),
        ));
    }

    match supervisor.start_node(&config, supervisor_ref).await {
        Ok(started) => Ok(Json(started)),
        Err(crate::error::Error::NodeAlreadyRunning(id)) => {
            let pid = supervisor.node_pid(id);
            let uptime_secs = supervisor.node_uptime_secs(id);
            Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Node {id} is already running"),
                    "current_state": {
                        "node_id": id,
                        "status": "running",
                        "pid": pid,
                        "uptime_secs": uptime_secs,
                    }
                })),
            ))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/v1/nodes/start-all — Start all registered nodes.
async fn post_start_all(State(state): State<Arc<AppState>>) -> Json<StartNodeResult> {
    let registry = state.registry.read().await;
    let configs: Vec<_> = registry.list().into_iter().cloned().collect();
    drop(registry);

    let mut started = Vec::new();
    let mut failed = Vec::new();
    let mut already_running = Vec::new();

    let supervisor_ref = state.supervisor.clone();

    for config in &configs {
        let mut supervisor = state.supervisor.write().await;
        if supervisor.is_running(config.id) {
            already_running.push(config.id);
            continue;
        }

        match supervisor.start_node(config, supervisor_ref.clone()).await {
            Ok(result) => started.push(result),
            Err(crate::error::Error::NodeAlreadyRunning(id)) => {
                already_running.push(id);
            }
            Err(e) => {
                failed.push(crate::node::types::NodeStartFailed {
                    node_id: config.id,
                    service_name: config.service_name.clone(),
                    error: e.to_string(),
                });
            }
        }
    }

    Json(StartNodeResult {
        started,
        failed,
        already_running,
    })
}

/// POST /api/v1/nodes/:id/stop — Stop a specific node.
async fn post_stop_node(
    State(state): State<Arc<AppState>>,
    Path(id): Path<u32>,
) -> std::result::Result<Json<NodeStopped>, (StatusCode, Json<serde_json::Value>)> {
    let registry = state.registry.read().await;
    let config = match registry.get(id) {
        Ok(config) => config.clone(),
        Err(_) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": format!("Node not found: {id}") })),
            ))
        }
    };
    drop(registry);

    // Acquire write lock once for atomic check-and-act (avoids TOCTOU race)
    let mut supervisor = state.supervisor.write().await;
    if !supervisor.is_running(id) {
        let status = supervisor
            .node_status(id)
            .unwrap_or(crate::node::types::NodeStatus::Stopped);
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Node {id} is not running"),
                "current_state": {
                    "node_id": id,
                    "status": status,
                }
            })),
        ));
    }

    match supervisor.stop_node(id).await {
        Ok(()) => Ok(Json(NodeStopped {
            node_id: id,
            service_name: config.service_name,
        })),
        Err(crate::error::Error::NodeNotRunning(id)) => {
            let status = supervisor
                .node_status(id)
                .unwrap_or(crate::node::types::NodeStatus::Stopped);
            Err((
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": format!("Node {id} is not running"),
                    "current_state": {
                        "node_id": id,
                        "status": status,
                    }
                })),
            ))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

/// POST /api/v1/nodes/stop-all — Stop all running nodes.
async fn post_stop_all(State(state): State<Arc<AppState>>) -> Json<StopNodeResult> {
    let registry = state.registry.read().await;
    let configs: Vec<(u32, String)> = registry
        .list()
        .into_iter()
        .map(|c| (c.id, c.service_name.clone()))
        .collect();
    drop(registry);

    let mut supervisor = state.supervisor.write().await;
    let result = supervisor.stop_all_nodes(&configs).await;

    Json(result)
}

/// POST /api/v1/reset — Reset all node state.
async fn post_reset(
    State(state): State<Arc<AppState>>,
) -> std::result::Result<Json<ResetResult>, (StatusCode, Json<serde_json::Value>)> {
    // Hold write lock for atomic check-and-act (prevents nodes being started
    // between the running check and the reset operation)
    let supervisor = state.supervisor.write().await;
    let (running, _, _) = supervisor.node_counts();
    if running > 0 {
        return Err((
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": format!("Cannot reset while nodes are running ({running} node(s) still running). Stop all nodes first."),
                "nodes_running": running,
            })),
        ));
    }
    drop(supervisor);

    let registry_path = state.config.registry_path.clone();

    match crate::node::reset(&registry_path) {
        Ok(result) => {
            // Update the in-memory registry to stay in sync
            let mut registry = state.registry.write().await;
            if let Ok(fresh) = NodeRegistry::load(&registry_path) {
                *registry = fresh;
            }
            Ok(Json(result))
        }
        Err(e) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )),
    }
}

async fn get_openapi() -> impl IntoResponse {
    // TODO: Migrate to utoipa-generated OpenAPI spec. Types already derive
    // utoipa::ToSchema but this spec is still hand-written JSON.
    let spec = serde_json::json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Ant Daemon API",
            "version": "0.1.0",
            "description": "REST API for the ant node management daemon"
        },
        "paths": {
            "/api/v1/status": {
                "get": {
                    "summary": "Daemon status",
                    "description": "Returns daemon health, uptime, and node count summary",
                    "responses": {
                        "200": {
                            "description": "Daemon status",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/DaemonStatus" }
                                }
                            }
                        }
                    }
                }
            },
            "/api/v1/events": {
                "get": {
                    "summary": "Event stream",
                    "description": "SSE stream of real-time node events",
                    "responses": {
                        "200": {
                            "description": "SSE event stream"
                        }
                    }
                }
            },
            "/api/v1/nodes": {
                "post": {
                    "summary": "Add nodes",
                    "description": "Add one or more nodes to the registry",
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": {
                                "schema": { "$ref": "#/components/schemas/AddNodeOpts" }
                            }
                        }
                    },
                    "responses": {
                        "201": {
                            "description": "Nodes added",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/AddNodeResult" }
                                }
                            }
                        },
                        "400": {
                            "description": "Invalid request"
                        }
                    }
                }
            },
            "/api/v1/nodes/{id}": {
                "delete": {
                    "summary": "Remove node",
                    "description": "Remove a node from the registry",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "integer" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Node removed",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/RemoveNodeResult" }
                                }
                            }
                        },
                        "404": {
                            "description": "Node not found"
                        }
                    }
                }
            },
            "/api/v1/nodes/{id}/start": {
                "post": {
                    "summary": "Start a node",
                    "description": "Start a specific node by ID. Returns 409 if already running with current_state.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "integer" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Node started",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/NodeStarted" }
                                }
                            }
                        },
                        "404": {
                            "description": "Node not found"
                        },
                        "409": {
                            "description": "Node already running (includes current_state)"
                        },
                        "500": {
                            "description": "Failed to start node"
                        }
                    }
                }
            },
            "/api/v1/nodes/start-all": {
                "post": {
                    "summary": "Start all nodes",
                    "description": "Start all registered nodes. Returns per-node results.",
                    "responses": {
                        "200": {
                            "description": "Start results",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/StartNodeResult" }
                                }
                            }
                        }
                    }
                }
            },
            "/api/v1/nodes/{id}/stop": {
                "post": {
                    "summary": "Stop a node",
                    "description": "Stop a specific node by ID. Returns 409 if already stopped with current_state.",
                    "parameters": [{
                        "name": "id",
                        "in": "path",
                        "required": true,
                        "schema": { "type": "integer" }
                    }],
                    "responses": {
                        "200": {
                            "description": "Node stopped",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/NodeStopped" }
                                }
                            }
                        },
                        "404": {
                            "description": "Node not found"
                        },
                        "409": {
                            "description": "Node not running (includes current_state)"
                        },
                        "500": {
                            "description": "Failed to stop node"
                        }
                    }
                }
            },
            "/api/v1/nodes/stop-all": {
                "post": {
                    "summary": "Stop all nodes",
                    "description": "Stop all running nodes. Returns per-node results.",
                    "responses": {
                        "200": {
                            "description": "Stop results",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/StopNodeResult" }
                                }
                            }
                        }
                    }
                }
            },
            "/api/v1/reset": {
                "post": {
                    "summary": "Reset all node state",
                    "description": "Remove all node data directories, log directories, and clear the registry. Fails if any nodes are running.",
                    "responses": {
                        "200": {
                            "description": "Reset successful",
                            "content": {
                                "application/json": {
                                    "schema": { "$ref": "#/components/schemas/ResetResult" }
                                }
                            }
                        },
                        "409": {
                            "description": "Nodes still running"
                        }
                    }
                }
            }
        },
        "components": {
            "schemas": {
                "DaemonStatus": {
                    "type": "object",
                    "properties": {
                        "running": { "type": "boolean" },
                        "pid": { "type": "integer", "nullable": true },
                        "port": { "type": "integer", "nullable": true },
                        "uptime_secs": { "type": "integer", "nullable": true },
                        "nodes_total": { "type": "integer" },
                        "nodes_running": { "type": "integer" },
                        "nodes_stopped": { "type": "integer" },
                        "nodes_errored": { "type": "integer" }
                    }
                }
            }
        }
    });
    Json(spec)
}

async fn get_console() -> Html<&'static str> {
    Html(include_str!("console.html"))
}

fn write_file(path: &PathBuf, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}
