use std::collections::HashMap;
use std::fmt;
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
    #[schema(value_type = Option<String>)]
    pub log_dir: Option<PathBuf>,
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

/// Source for the node binary.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(tag = "type", content = "value")]
#[serde(rename_all = "snake_case")]
pub enum BinarySource {
    /// Download the latest release.
    #[default]
    Latest,
    /// Download a specific version.
    Version(String),
    /// Download from a URL (zip/tar.gz archive).
    Url(String),
    /// Use an existing local binary.
    #[schema(value_type = String)]
    LocalPath(PathBuf),
}

impl fmt::Display for BinarySource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Latest => write!(f, "latest"),
            Self::Version(v) => write!(f, "v{v}"),
            Self::Url(u) => write!(f, "{u}"),
            Self::LocalPath(p) => write!(f, "{}", p.display()),
        }
    }
}

/// A single port or a range of ports.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(untagged)]
pub enum PortRange {
    Single(u16),
    Range(u16, u16),
}

impl PortRange {
    /// Returns the number of ports in this range.
    pub fn len(&self) -> u16 {
        match self {
            Self::Single(_) => 1,
            Self::Range(start, end) => end.saturating_sub(*start) + 1,
        }
    }

    /// Whether this range is empty (should not happen in practice).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get the port at the given index within the range.
    pub fn port_at(&self, index: u16) -> Option<u16> {
        match self {
            Self::Single(p) if index == 0 => Some(*p),
            Self::Range(start, end) => {
                let port = start + index;
                if port <= *end {
                    Some(port)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Options for adding one or more nodes to the registry.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AddNodeOpts {
    /// Number of nodes to add. Default: 1.
    pub count: u16,
    /// Required. Wallet address for node earnings.
    pub rewards_address: String,
    /// Port or port range for node(s).
    pub node_port: Option<PortRange>,
    /// Metrics port or range.
    pub metrics_port: Option<PortRange>,
    /// Custom data directory prefix.
    #[schema(value_type = Option<String>)]
    pub data_dir_path: Option<PathBuf>,
    /// Custom log directory prefix.
    #[schema(value_type = Option<String>)]
    pub log_dir_path: Option<PathBuf>,
    /// Network ID. Default: 1 (mainnet).
    pub network_id: u8,
    /// Source for the node binary.
    pub binary_source: BinarySource,
    /// Bootstrap peer(s).
    pub bootstrap_peers: Vec<String>,
    /// Environment variables for the node.
    pub env_variables: Vec<(String, String)>,
}

impl Default for AddNodeOpts {
    fn default() -> Self {
        Self {
            count: 1,
            rewards_address: String::new(),
            node_port: None,
            metrics_port: None,
            data_dir_path: None,
            log_dir_path: None,
            network_id: 1,
            binary_source: BinarySource::default(),
            bootstrap_peers: Vec::new(),
            env_variables: Vec::new(),
        }
    }
}

/// Result of adding nodes to the registry.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct AddNodeResult {
    /// The nodes that were added.
    pub nodes_added: Vec<NodeConfig>,
}

/// Result of removing a node from the registry.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct RemoveNodeResult {
    /// The node that was removed.
    pub removed: NodeConfig,
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

    #[test]
    fn port_range_single_len() {
        let pr = PortRange::Single(8080);
        assert_eq!(pr.len(), 1);
        assert!(!pr.is_empty());
    }

    #[test]
    fn port_range_single_port_at() {
        let pr = PortRange::Single(8080);
        assert_eq!(pr.port_at(0), Some(8080));
        assert_eq!(pr.port_at(1), None);
    }

    #[test]
    fn port_range_range_len() {
        let pr = PortRange::Range(12000, 12004);
        assert_eq!(pr.len(), 5);
    }

    #[test]
    fn port_range_range_port_at() {
        let pr = PortRange::Range(12000, 12002);
        assert_eq!(pr.port_at(0), Some(12000));
        assert_eq!(pr.port_at(1), Some(12001));
        assert_eq!(pr.port_at(2), Some(12002));
        assert_eq!(pr.port_at(3), None);
    }

    #[test]
    fn binary_source_serializes_with_tag() {
        let src = BinarySource::Latest;
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"type\":\"latest\""));

        let src = BinarySource::Version("1.0.0".to_string());
        let json = serde_json::to_string(&src).unwrap();
        assert!(json.contains("\"type\":\"version\""));
        assert!(json.contains("1.0.0"));
    }

    #[test]
    fn add_node_opts_default() {
        let opts = AddNodeOpts::default();
        assert_eq!(opts.count, 1);
        assert_eq!(opts.network_id, 1);
        assert!(matches!(opts.binary_source, BinarySource::Latest));
    }
}
