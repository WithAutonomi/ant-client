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
        let data = config::data_dir().expect("Could not determine data directory");
        let logs = config::log_dir().expect("Could not determine log directory");
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
    pub service_name: String,
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
                let port = start.checked_add(index)?;
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
    pub network_id: u32,
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

/// Options for resetting all node state.
#[derive(Debug, Clone, Default, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResetOpts {
    /// Skip confirmation (used by CLI layer; ignored by core).
    pub force: bool,
}

/// Result of resetting all node state.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ResetResult {
    /// Number of nodes that were cleared from the registry.
    pub nodes_cleared: u32,
    /// Data directories that were removed.
    #[schema(value_type = Vec<String>)]
    pub data_dirs_removed: Vec<PathBuf>,
    /// Log directories that were removed.
    #[schema(value_type = Vec<String>)]
    pub log_dirs_removed: Vec<PathBuf>,
}

/// Options for starting one or more nodes.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StartNodeOpts {
    /// Which node(s) to start.
    pub target: NodeTarget,
}

/// Which node(s) to target.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum NodeTarget {
    /// Start a specific node by service name.
    ServiceName(String),
    /// Start all registered nodes.
    All,
}

/// Result of starting node(s).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StartNodeResult {
    /// Nodes that were successfully started.
    pub started: Vec<NodeStarted>,
    /// Nodes that failed to start.
    pub failed: Vec<NodeStartFailed>,
    /// Node IDs that were already running.
    pub already_running: Vec<u32>,
}

/// A node that was successfully started.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStarted {
    pub node_id: u32,
    pub service_name: String,
    pub pid: u32,
}

/// A node that failed to start.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStartFailed {
    pub node_id: u32,
    pub service_name: String,
    pub error: String,
}

/// Result of stopping node(s).
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct StopNodeResult {
    /// Nodes that were successfully stopped.
    pub stopped: Vec<NodeStopped>,
    /// Nodes that failed to stop.
    pub failed: Vec<NodeStopFailed>,
    /// Node IDs that were already stopped.
    pub already_stopped: Vec<u32>,
}

/// A node that was successfully stopped.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStopped {
    pub node_id: u32,
    pub service_name: String,
}

/// A node that failed to stop.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStopFailed {
    pub node_id: u32,
    pub service_name: String,
    pub error: String,
}

/// Summary of a single node's status.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStatusSummary {
    pub node_id: u32,
    pub name: String,
    pub version: String,
    pub status: NodeStatus,
}

/// Result of querying node status across all registered nodes.
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
pub struct NodeStatusResult {
    pub nodes: Vec<NodeStatusSummary>,
    pub total_running: u32,
    pub total_stopped: u32,
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

    #[test]
    fn stop_node_result_serializes() {
        let result = StopNodeResult {
            stopped: vec![NodeStopped {
                node_id: 1,
                service_name: "node1".to_string(),
            }],
            failed: vec![NodeStopFailed {
                node_id: 2,
                service_name: "node2".to_string(),
                error: "process not found".to_string(),
            }],
            already_stopped: vec![3],
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: StopNodeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.stopped.len(), 1);
        assert_eq!(deserialized.stopped[0].node_id, 1);
        assert_eq!(deserialized.failed.len(), 1);
        assert_eq!(deserialized.already_stopped, vec![3]);
    }

    #[test]
    fn node_status_result_serializes() {
        let result = NodeStatusResult {
            nodes: vec![
                NodeStatusSummary {
                    node_id: 1,
                    name: "antnode-1".to_string(),
                    version: "0.110.0".to_string(),
                    status: NodeStatus::Running,
                },
                NodeStatusSummary {
                    node_id: 2,
                    name: "antnode-2".to_string(),
                    version: "0.110.0".to_string(),
                    status: NodeStatus::Stopped,
                },
            ],
            total_running: 1,
            total_stopped: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: NodeStatusResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.nodes.len(), 2);
        assert_eq!(deserialized.nodes[0].node_id, 1);
        assert_eq!(deserialized.nodes[0].status, NodeStatus::Running);
        assert_eq!(deserialized.nodes[1].status, NodeStatus::Stopped);
        assert_eq!(deserialized.total_running, 1);
        assert_eq!(deserialized.total_stopped, 1);
    }

    #[test]
    fn node_status_summary_serializes() {
        let summary = NodeStatusSummary {
            node_id: 1,
            name: "antnode-1".to_string(),
            version: "0.110.0".to_string(),
            status: NodeStatus::Running,
        };
        let json = serde_json::to_string(&summary).unwrap();
        assert!(json.contains("\"node_id\":1"));
        assert!(json.contains("\"name\":\"antnode-1\""));
        assert!(json.contains("\"version\":\"0.110.0\""));
        assert!(json.contains("\"status\":\"running\""));
    }

    #[test]
    fn node_stopped_serializes() {
        let stopped = NodeStopped {
            node_id: 1,
            service_name: "node1".to_string(),
        };
        let json = serde_json::to_string(&stopped).unwrap();
        assert!(json.contains("\"node_id\":1"));
        assert!(json.contains("\"service_name\":\"node1\""));
    }
}
