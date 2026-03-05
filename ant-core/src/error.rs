use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Node not found: {0}")]
    NodeNotFound(u32),

    #[error("Node already running: {0}")]
    NodeAlreadyRunning(u32),

    #[error("Node not running: {0}")]
    NodeNotRunning(u32),

    #[error("Daemon already running (pid: {0})")]
    DaemonAlreadyRunning(u32),

    #[error("Daemon not running")]
    DaemonNotRunning,

    #[error("Failed to bind to address: {0}")]
    BindError(String),

    #[error("Port file not found: {0}")]
    PortFileNotFound(PathBuf),

    #[error("PID file not found: {0}")]
    PidFileNotFound(PathBuf),

    #[error("HTTP request error: {0}")]
    HttpRequest(String),

    #[error("Process spawn failed: {0}")]
    ProcessSpawn(String),
}

pub type Result<T> = std::result::Result<T, Error>;
