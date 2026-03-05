use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::broadcast;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::error::Result;
use crate::node::daemon::supervisor::Supervisor;
use crate::node::events::NodeEvent;
use crate::node::registry::NodeRegistry;
use crate::node::types::{DaemonConfig, DaemonStatus};

/// Shared application state for the daemon HTTP server.
pub struct AppState {
    pub registry: RwLock<NodeRegistry>,
    pub supervisor: RwLock<Supervisor>,
    pub event_tx: broadcast::Sender<NodeEvent>,
    pub start_time: Instant,
    pub config: DaemonConfig,
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

    let state = Arc::new(AppState {
        registry: RwLock::new(registry),
        supervisor: RwLock::new(Supervisor::new(event_tx.clone())),
        event_tx,
        start_time: Instant::now(),
        config: config.clone(),
    });

    let app = build_router(state.clone());

    let addr = SocketAddr::new(config.listen_addr, config.port.unwrap_or(0));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::Error::BindError(e.to_string()))?;
    let bound_addr = listener
        .local_addr()
        .map_err(|e| crate::error::Error::BindError(e.to_string()))?;

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
    Router::new()
        .route("/api/v1/status", get(get_status))
        .route("/api/v1/events", get(get_events))
        .route("/api/v1/openapi.json", get(get_openapi))
        .with_state(state)
}

async fn get_status(State(state): State<Arc<AppState>>) -> Json<DaemonStatus> {
    let registry = state.registry.read().await;
    let supervisor = state.supervisor.read().await;
    let (running, stopped, errored) = supervisor.node_counts();

    Json(DaemonStatus {
        running: true,
        pid: Some(std::process::id()),
        port: Some(state.config.port.unwrap_or(0)),
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

async fn get_openapi() -> impl IntoResponse {
    // Minimal OpenAPI 3.1 spec - will be expanded with utoipa as endpoints grow
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

fn write_file(path: &PathBuf, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}
