pub mod binary;
pub mod daemon;
pub mod events;
pub mod process;
pub mod registry;
pub mod types;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config;
use crate::error::{Error, Result};
use crate::node::binary::ProgressReporter;
use crate::node::registry::NodeRegistry;
use crate::node::types::{
    AddNodeOpts, AddNodeResult, NodeConfig, NodeStatus, NodeStatusResult, NodeStatusSummary,
    RemoveNodeResult, ResetResult,
};

/// Add one or more nodes to the registry.
///
/// This function:
/// 1. Resolves the binary (download if needed)
/// 2. Loads the registry (with file lock)
/// 3. Validates port ranges match count
/// 4. Creates data and log directories for each node
/// 5. Assigns IDs and saves the registry
///
/// Does NOT start the nodes. Does NOT require the daemon.
pub async fn add_nodes(
    opts: AddNodeOpts,
    registry_path: &Path,
    progress: &dyn ProgressReporter,
) -> Result<AddNodeResult> {
    // Validate and normalize rewards address
    validate_rewards_address(&opts.rewards_address)?;
    let rewards_address = opts.rewards_address.trim().to_string();

    // Cap the number of nodes per call to prevent accidental resource exhaustion
    const MAX_NODES_PER_CALL: u16 = 1000;
    if opts.count > MAX_NODES_PER_CALL {
        return Err(Error::InvalidNodeCount {
            count: opts.count,
            max: MAX_NODES_PER_CALL,
        });
    }

    // Validate port ranges match count
    if let Some(ref port_range) = opts.node_port {
        let range_len = port_range.len();
        if range_len != 1 && range_len != opts.count {
            return Err(Error::PortRangeMismatch {
                range_len,
                count: opts.count,
            });
        }
    }
    if let Some(ref port_range) = opts.metrics_port {
        let range_len = port_range.len();
        if range_len != 1 && range_len != opts.count {
            return Err(Error::PortRangeMismatch {
                range_len,
                count: opts.count,
            });
        }
    }

    // Resolve the binary (downloads to cache if needed)
    let install_dir = binary::binary_install_dir()?;
    let (cached_binary, version) =
        binary::resolve_binary(&opts.binary_source, &install_dir, progress).await?;

    // Load registry with file lock
    let (mut registry, _lock) = NodeRegistry::load_locked(registry_path)?;

    // Build node configs
    let mut nodes_added = Vec::with_capacity(opts.count as usize);
    let env_map: HashMap<String, String> = opts.env_variables.into_iter().collect();

    // Each node gets its own copy under the plain binary name
    let binary_file_name = binary::BINARY_NAME;

    for i in 0..opts.count {
        let node_port = resolve_port(&opts.node_port, i, opts.count);
        let metrics_port = resolve_port(&opts.metrics_port, i, opts.count);

        // We use a placeholder ID (0) here; the registry will assign the real one
        let placeholder_id = 0;

        let data_dir = node_data_dir(&opts.data_dir_path, placeholder_id);
        let log_dir = node_log_dir(&opts.log_dir_path, placeholder_id);

        let config = NodeConfig {
            id: placeholder_id,
            service_name: String::new(), // assigned by registry.add()
            rewards_address: rewards_address.clone(),
            data_dir,
            log_dir,
            node_port,
            metrics_port,
            network_id: Some(opts.network_id),
            binary_path: PathBuf::new(), // placeholder, updated below
            version: version.clone(),
            env_variables: env_map.clone(),
            bootstrap_peers: opts.bootstrap_peers.clone(),
        };

        let assigned_id = registry.add(config);

        // Now update paths with the actual assigned ID
        let node = registry.get_mut(assigned_id)?;
        node.data_dir = node_data_dir(&opts.data_dir_path, assigned_id);
        node.log_dir = node_log_dir(&opts.log_dir_path, assigned_id);

        // Create directories
        std::fs::create_dir_all(&node.data_dir)?;
        if let Some(ref log_dir) = node.log_dir {
            std::fs::create_dir_all(log_dir)?;
        }

        // Copy the binary into this node's data directory so each node
        // has its own copy. This allows safe per-node upgrades without
        // affecting running nodes.
        let node_binary = node.data_dir.join(binary_file_name);
        std::fs::copy(&cached_binary, &node_binary)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&node_binary, std::fs::Permissions::from_mode(0o755))?;
        }
        node.binary_path = node_binary;

        nodes_added.push(node.clone());
    }

    registry.save()?;

    Ok(AddNodeResult { nodes_added })
}

/// Remove a node from the registry.
///
/// Does NOT stop the node. Does NOT require the daemon.
pub fn remove_node(node_id: u32, registry_path: &Path) -> Result<RemoveNodeResult> {
    let (mut registry, _lock) = NodeRegistry::load_locked(registry_path)?;
    let removed = registry.remove(node_id)?;
    registry.save()?;
    Ok(RemoveNodeResult { removed })
}

/// Reset all node state: remove all data directories, log directories, and clear the registry.
///
/// This function:
/// 1. Loads the registry (with file lock)
/// 2. Iterates over all registered nodes
/// 3. Removes each node's data directory
/// 4. Removes each node's log directory (if set)
/// 5. Clears the registry (empties nodes, resets next_id to 1)
///
/// Does NOT check if nodes are running — callers must verify that first.
pub fn reset(registry_path: &Path) -> Result<ResetResult> {
    let (mut registry, _lock) = NodeRegistry::load_locked(registry_path)?;

    let mut data_dirs_removed = Vec::new();
    let mut log_dirs_removed = Vec::new();
    let nodes_cleared = registry.len() as u32;

    for node in registry.list() {
        if node.data_dir.exists() {
            std::fs::remove_dir_all(&node.data_dir)?;
            data_dirs_removed.push(node.data_dir.clone());
        }
        if let Some(ref log_dir) = node.log_dir {
            if log_dir.exists() {
                std::fs::remove_dir_all(log_dir)?;
                log_dirs_removed.push(log_dir.clone());
            }
        }
    }

    registry.clear();
    registry.save()?;

    Ok(ResetResult {
        nodes_cleared,
        data_dirs_removed,
        log_dirs_removed,
    })
}

/// Get the status of all registered nodes without the daemon.
///
/// Since the daemon is not running, all nodes are reported as `Stopped`.
pub fn node_status_offline(registry_path: &Path) -> Result<NodeStatusResult> {
    let registry = NodeRegistry::load(registry_path)?;
    let nodes: Vec<NodeStatusSummary> = registry
        .list()
        .iter()
        .map(|config| NodeStatusSummary {
            node_id: config.id,
            name: config.service_name.clone(),
            version: config.version.clone(),
            status: NodeStatus::Stopped,
        })
        .collect();
    let total_stopped = nodes.len() as u32;
    Ok(NodeStatusResult {
        nodes,
        total_running: 0,
        total_stopped,
    })
}

/// Validate that a rewards address is a valid Ethereum-style address.
///
/// Must be `0x` followed by exactly 40 hexadecimal characters.
fn validate_rewards_address(address: &str) -> Result<()> {
    let address = address.trim();
    if address.is_empty() {
        return Err(Error::InvalidRewardsAddress(
            "rewards address cannot be empty".to_string(),
        ));
    }
    if !address.starts_with("0x") && !address.starts_with("0X") {
        return Err(Error::InvalidRewardsAddress(format!(
            "rewards address must start with '0x', got '{address}'"
        )));
    }
    let hex_part = &address[2..];
    if hex_part.len() != 40 {
        return Err(Error::InvalidRewardsAddress(format!(
            "rewards address must be 42 characters (0x + 40 hex), got {} characters",
            address.len()
        )));
    }
    if !hex_part.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(Error::InvalidRewardsAddress(format!(
            "rewards address contains non-hex characters: '{address}'"
        )));
    }
    Ok(())
}

/// Determine the data directory for a node.
fn node_data_dir(custom_prefix: &Option<PathBuf>, node_id: u32) -> PathBuf {
    match custom_prefix {
        Some(prefix) => prefix.join(format!("node-{node_id}")),
        None => config::data_dir()
            .expect("Could not determine data directory")
            .join("nodes")
            .join(format!("node-{node_id}")),
    }
}

/// Determine the log directory for a node.
/// Returns `None` when no custom log dir prefix was provided (no logging by default).
fn node_log_dir(custom_prefix: &Option<PathBuf>, node_id: u32) -> Option<PathBuf> {
    custom_prefix
        .as_ref()
        .map(|prefix| prefix.join(format!("node-{node_id}")).join("logs"))
}

/// Resolve a port from a PortRange for a given node index.
fn resolve_port(range: &Option<types::PortRange>, index: u16, _count: u16) -> Option<u16> {
    range.as_ref().and_then(|r| r.port_at(index))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::binary::NoopProgress;
    use crate::node::types::{BinarySource, PortRange};

    /// A valid Ethereum address for use in tests.
    const TEST_ADDR: &str = "0x1234567890abcdef1234567890abcdef12345678";

    fn test_registry_path(dir: &std::path::Path) -> PathBuf {
        dir.join("node_registry.json")
    }

    /// Create a fake binary that responds to --version.
    /// On Windows, uses a .cmd extension so the shell can execute it.
    fn create_fake_binary(dir: &std::path::Path) -> PathBuf {
        #[cfg(unix)]
        {
            let binary_path = dir.join("fake-antnode");
            std::fs::write(&binary_path, "#!/bin/sh\necho \"antnode 0.1.0-test\"\n").unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&binary_path, std::fs::Permissions::from_mode(0o755)).unwrap();
            binary_path
        }
        #[cfg(windows)]
        {
            let binary_path = dir.join("fake-antnode.cmd");
            std::fs::write(&binary_path, "@echo off\r\necho antnode 0.1.0-test\r\n").unwrap();
            binary_path
        }
    }

    #[tokio::test]
    async fn add_single_node_with_local_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        let opts = AddNodeOpts {
            count: 1,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(tmp.path().join("data")),
            log_dir_path: Some(tmp.path().join("logs")),
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();
        assert_eq!(result.nodes_added.len(), 1);
        assert_eq!(result.nodes_added[0].rewards_address, TEST_ADDR);
        assert_eq!(result.nodes_added[0].id, 1);
        assert!(result.nodes_added[0].data_dir.exists());
        assert!(result.nodes_added[0].log_dir.as_ref().unwrap().exists());

        // Verify registry was saved
        let reg = NodeRegistry::load(&reg_path).unwrap();
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn add_multiple_nodes_with_port_range() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        let opts = AddNodeOpts {
            count: 3,
            rewards_address: TEST_ADDR.to_string(),
            node_port: Some(PortRange::Range(12000, 12002)),
            data_dir_path: Some(tmp.path().join("data")),
            log_dir_path: Some(tmp.path().join("logs")),
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();
        assert_eq!(result.nodes_added.len(), 3);
        assert_eq!(result.nodes_added[0].node_port, Some(12000));
        assert_eq!(result.nodes_added[1].node_port, Some(12001));
        assert_eq!(result.nodes_added[2].node_port, Some(12002));
        assert_eq!(result.nodes_added[0].id, 1);
        assert_eq!(result.nodes_added[1].id, 2);
        assert_eq!(result.nodes_added[2].id, 3);
    }

    #[tokio::test]
    async fn add_nodes_rejects_port_range_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        let opts = AddNodeOpts {
            count: 3,
            rewards_address: TEST_ADDR.to_string(),
            node_port: Some(PortRange::Range(12000, 12001)), // 2 ports, 3 nodes
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::PortRangeMismatch { .. }
        ));
    }

    #[tokio::test]
    async fn add_nodes_rejects_empty_rewards_address() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        let opts = AddNodeOpts {
            count: 1,
            rewards_address: "  ".to_string(),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            Error::InvalidRewardsAddress(_)
        ));
    }

    #[test]
    fn validate_rewards_address_rejects_missing_prefix() {
        let result = validate_rewards_address("1234567890abcdef1234567890abcdef12345678");
        assert!(result.is_err());
    }

    #[test]
    fn validate_rewards_address_rejects_short_address() {
        let result = validate_rewards_address("0xabc123");
        assert!(result.is_err());
    }

    #[test]
    fn validate_rewards_address_rejects_non_hex() {
        let result = validate_rewards_address("0xGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGGG");
        assert!(result.is_err());
    }

    #[test]
    fn validate_rewards_address_accepts_valid() {
        let result = validate_rewards_address(TEST_ADDR);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_rewards_address_accepts_uppercase_hex() {
        let result = validate_rewards_address("0xABCDEF1234567890ABCDEF1234567890ABCDEF12");
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn add_nodes_with_custom_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());
        let custom_data = tmp.path().join("custom-data");

        let opts = AddNodeOpts {
            count: 1,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(custom_data.clone()),
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();
        assert!(result.nodes_added[0].data_dir.starts_with(&custom_data));
    }

    #[tokio::test]
    async fn add_nodes_without_log_dir_sets_none() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        let opts = AddNodeOpts {
            count: 1,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(tmp.path().join("data")),
            // log_dir_path not set — defaults to None
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();
        assert!(result.nodes_added[0].log_dir.is_none());
    }

    #[test]
    fn remove_node_from_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        // First add a node directly to the registry
        let (mut registry, _lock) = NodeRegistry::load_locked(&reg_path).unwrap();
        registry.add(NodeConfig {
            id: 0,
            service_name: String::new(),
            rewards_address: "0xtest".to_string(),
            data_dir: PathBuf::from("/tmp/test"),
            log_dir: None,
            node_port: None,
            metrics_port: None,
            network_id: None,
            binary_path: PathBuf::from("/usr/bin/antnode"),
            version: "0.1.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec![],
        });
        registry.save().unwrap();
        drop(_lock);

        let result = remove_node(1, &reg_path).unwrap();
        assert_eq!(result.removed.rewards_address, "0xtest");

        let reg = NodeRegistry::load(&reg_path).unwrap();
        assert!(reg.is_empty());
    }

    #[test]
    fn remove_nonexistent_node_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        let result = remove_node(999, &reg_path);
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), Error::NodeNotFound(999)));
    }

    #[tokio::test]
    async fn reset_clears_all_nodes_and_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        // Add 2 nodes
        let opts = AddNodeOpts {
            count: 2,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(tmp.path().join("data")),
            log_dir_path: Some(tmp.path().join("logs")),
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };

        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();
        assert_eq!(result.nodes_added.len(), 2);

        // Verify directories exist
        for node in &result.nodes_added {
            assert!(node.data_dir.exists());
            assert!(node.log_dir.as_ref().unwrap().exists());
        }

        // Reset
        let reset_result = reset(&reg_path).unwrap();
        assert_eq!(reset_result.nodes_cleared, 2);
        assert_eq!(reset_result.data_dirs_removed.len(), 2);
        assert_eq!(reset_result.log_dirs_removed.len(), 2);

        // Verify directories were removed
        for node in &result.nodes_added {
            assert!(!node.data_dir.exists());
            assert!(!node.log_dir.as_ref().unwrap().exists());
        }

        // Verify registry is empty and next_id reset
        let reg = NodeRegistry::load(&reg_path).unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.next_id, 1);
    }

    #[test]
    fn node_status_offline_shows_all_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        // Add two nodes directly to the registry
        let (mut registry, _lock) = NodeRegistry::load_locked(&reg_path).unwrap();
        registry.add(NodeConfig {
            id: 0,
            service_name: String::new(),
            rewards_address: "0xtest".to_string(),
            data_dir: PathBuf::from("/tmp/test1"),
            log_dir: None,
            node_port: None,
            metrics_port: None,
            network_id: None,
            binary_path: PathBuf::from("/usr/bin/antnode"),
            version: "0.110.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec![],
        });
        registry.add(NodeConfig {
            id: 0,
            service_name: String::new(),
            rewards_address: "0xtest".to_string(),
            data_dir: PathBuf::from("/tmp/test2"),
            log_dir: None,
            node_port: None,
            metrics_port: None,
            network_id: None,
            binary_path: PathBuf::from("/usr/bin/antnode"),
            version: "0.110.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec![],
        });
        registry.save().unwrap();
        drop(_lock);

        let result = node_status_offline(&reg_path).unwrap();
        assert_eq!(result.nodes.len(), 2);
        assert_eq!(result.total_running, 0);
        assert_eq!(result.total_stopped, 2);
        for node in &result.nodes {
            assert_eq!(node.status, NodeStatus::Stopped);
        }
    }

    #[test]
    fn node_status_offline_empty_registry() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        let result = node_status_offline(&reg_path).unwrap();
        assert!(result.nodes.is_empty());
        assert_eq!(result.total_running, 0);
        assert_eq!(result.total_stopped, 0);
    }

    #[test]
    fn reset_empty_registry_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let reg_path = test_registry_path(tmp.path());

        let result = reset(&reg_path).unwrap();
        assert_eq!(result.nodes_cleared, 0);
        assert!(result.data_dirs_removed.is_empty());
        assert!(result.log_dirs_removed.is_empty());
    }

    #[tokio::test]
    async fn reset_then_add_starts_fresh_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let binary = create_fake_binary(tmp.path());
        let reg_path = test_registry_path(tmp.path());

        // Add 2 nodes
        let opts = AddNodeOpts {
            count: 2,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(tmp.path().join("data")),
            log_dir_path: Some(tmp.path().join("logs")),
            binary_source: BinarySource::LocalPath(binary.clone()),
            ..Default::default()
        };
        add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();

        // Reset
        reset(&reg_path).unwrap();

        // Add again — IDs should restart from 1
        let opts = AddNodeOpts {
            count: 1,
            rewards_address: TEST_ADDR.to_string(),
            data_dir_path: Some(tmp.path().join("data")),
            log_dir_path: Some(tmp.path().join("logs")),
            binary_source: BinarySource::LocalPath(binary),
            ..Default::default()
        };
        let result = add_nodes(opts, &reg_path, &NoopProgress).await.unwrap();

        assert_eq!(result.nodes_added[0].id, 1);
        assert_eq!(result.nodes_added[0].rewards_address, TEST_ADDR);
    }
}
