use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Node not found: {0}")]
    NodeNotFound(u32),

    #[error("No node found with service name '{0}'")]
    NodeNotFoundByName(String),

    #[error("Node already running: {0}")]
    NodeAlreadyRunning(u32),

    #[error("Node not running: {0}")]
    NodeNotRunning(u32),

    #[error("Node {0} has been evicted; its data directory was deleted to reclaim disk space. Dismiss it and add a new node instead of restarting.")]
    NodeEvicted(u32),

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

    #[error("Invalid port range: {0}")]
    InvalidPortRange(String),

    #[error("Invalid env variable format: '{0}'. Expected KEY=VALUE")]
    InvalidEnvVar(String),

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

    #[error("Update failed: {0}")]
    UpdateFailed(String),

    #[error("Failed to parse bootstrap_peers.toml: {0}")]
    BootstrapConfigParse(String),

    #[error(
        "No bootstrap peers available: pass peers explicitly, use a devnet manifest, or install bootstrap_peers.toml in the config directory"
    )]
    NoBootstrapPeers,

    #[error(
        "Devnet manifest contains EVM config but no EVM network was selected: pass 'local' to use the manifest's EVM config, or an explicit preset ('arbitrum-one', 'arbitrum-sepolia') to override it"
    )]
    EvmNetworkAmbiguous,

    #[error("Unsupported EVM network: {0}. Use 'arbitrum-one', 'arbitrum-sepolia', or 'local'.")]
    UnsupportedEvmNetwork(String),

    #[error("EVM network 'local' requires a devnet manifest with EVM info")]
    EvmManifestRequired,

    #[error("Invalid EVM info in devnet manifest: {0}")]
    InvalidEvmManifest(String),

    #[error("Node count {count} exceeds maximum of {max} per call")]
    InvalidNodeCount { count: u16, max: u16 },

    #[error(
        "Cannot reset while nodes are running ({0} node(s) still running). Stop all nodes first."
    )]
    NodesStillRunning(u32),
}

pub type Result<T> = std::result::Result<T, Error>;
