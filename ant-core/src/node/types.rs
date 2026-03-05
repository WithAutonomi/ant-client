use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::config;

/// Configuration for the daemon process.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Address to listen on. Default: 127.0.0.1
    pub listen_addr: IpAddr,
    /// Port to listen on. None = pick a random available port.
    pub port: Option<u16>,
    /// Path to the node registry JSON file.
    pub registry_path: PathBuf,
    /// Path to the daemon log file.
    pub log_path: PathBuf,
    /// Where to write the chosen port so the CLI can discover it.
    pub port_file_path: PathBuf,
    /// Daemon's own PID file.
    pub pid_file_path: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let data = config::data_dir();
        let logs = config::log_dir();
        Self {
            listen_addr: IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
            port: None,
            registry_path: data.join("node_registry.json"),
            log_path: logs.join("daemon.log"),
            port_file_path: data.join("daemon.port"),
            pid_file_path: data.join("daemon.pid"),
        }
    }
}

/// Status information returned by the daemon.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct DaemonStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub uptime_secs: Option<u64>,
    pub nodes_total: u32,
    pub nodes_running: u32,
    pub nodes_stopped: u32,
    pub nodes_errored: u32,
}

/// Connection info returned by `daemon info`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub running: bool,
    pub pid: Option<u32>,
    pub port: Option<u16>,
    pub api_base: Option<String>,
}

/// Status of a single node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Errored,
}

/// Persisted configuration for a single node.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeConfig {
    pub id: u32,
    pub rewards_address: String,
    #[schema(value_type = String)]
    pub data_dir: PathBuf,
    #[schema(value_type = String)]
    pub log_dir: PathBuf,
    pub node_port: Option<u16>,
    pub metrics_port: Option<u16>,
    pub network_id: Option<u32>,
    #[schema(value_type = String)]
    pub binary_path: PathBuf,
    pub version: String,
    pub env_variables: HashMap<String, String>,
    pub bootstrap_peers: Vec<String>,
}

/// Runtime information for a running node (held in daemon memory only).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeInfo {
    #[serde(flatten)]
    pub config: NodeConfig,
    pub status: NodeStatus,
    pub pid: Option<u32>,
    pub uptime_secs: Option<u64>,
}

/// Result of a daemon start operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStartResult {
    /// Whether the daemon was already running.
    pub already_running: bool,
    /// PID of the daemon process.
    pub pid: u32,
    /// Port the daemon is listening on, if discovered.
    pub port: Option<u16>,
}

/// Result of a daemon stop operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStopResult {
    /// PID of the daemon that was stopped.
    pub pid: u32,
}

/// Options for adding a new node to the registry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddNodeOpts {
    pub rewards_address: String,
    pub data_dir: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub node_port: Option<u16>,
    pub metrics_port: Option<u16>,
    pub network_id: Option<u32>,
    pub binary_path: Option<PathBuf>,
    pub version: Option<String>,
    pub env_variables: HashMap<String, String>,
    pub bootstrap_peers: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_config_default_paths() {
        let cfg = DaemonConfig::default();
        assert_eq!(cfg.listen_addr, IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        assert!(cfg.port.is_none());
        assert!(cfg.registry_path.ends_with("node_registry.json"));
        assert!(cfg.port_file_path.ends_with("daemon.port"));
        assert!(cfg.pid_file_path.ends_with("daemon.pid"));
    }

    #[test]
    fn daemon_status_serializes() {
        let status = DaemonStatus {
            running: true,
            pid: Some(1234),
            port: Some(8080),
            uptime_secs: Some(3600),
            nodes_total: 3,
            nodes_running: 2,
            nodes_stopped: 1,
            nodes_errored: 0,
        };
        let json = serde_json::to_string(&status).unwrap();
        let deserialized: DaemonStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.pid, Some(1234));
        assert_eq!(deserialized.nodes_total, 3);
    }

    #[test]
    fn node_status_serializes_snake_case() {
        let json = serde_json::to_string(&NodeStatus::Running).unwrap();
        assert_eq!(json, "\"running\"");
    }
}
