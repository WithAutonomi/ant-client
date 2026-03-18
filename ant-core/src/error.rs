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

    #[error("Port range length ({range_len}) does not match node count ({count})")]
    PortRangeMismatch { range_len: u16, count: u16 },

    #[error("Binary not found at path: {0}")]
    BinaryNotFound(PathBuf),

    #[error("Binary resolution failed: {0}")]
    BinaryResolution(String),

    #[error("Invalid rewards address: {0}")]
    InvalidRewardsAddress(String),

    #[error("Failed to stop daemon: {0}")]
    DaemonStopFailed(String),

    #[error("Could not determine home directory (HOME/USERPROFILE not set)")]
    HomeDirNotFound,

    #[error("Node count {count} exceeds maximum of {max} per call")]
    InvalidNodeCount { count: u16, max: u16 },

    #[error(
        "Cannot reset while nodes are running ({0} node(s) still running). Stop all nodes first."
    )]
    NodesStillRunning(u32),
}

pub type Result<T> = std::result::Result<T, Error>;
