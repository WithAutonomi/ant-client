use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};

use fs2::FileExt;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::node::types::NodeConfig;

/// Persisted node registry (JSON file on disk).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeRegistry {
    pub schema_version: u32,
    pub nodes: HashMap<u32, NodeConfig>,
    pub next_id: u32,
    /// Path where this registry is persisted. Not serialized.
    #[serde(skip)]
    pub path: PathBuf,
}

impl NodeRegistry {
    /// Load the registry from disk, or create an empty one if the file doesn't exist.
    pub fn load(path: &Path) -> Result<Self> {
        if path.exists() {
            let contents = std::fs::read_to_string(path)?;
            let mut registry: Self = serde_json::from_str(&contents)?;
            registry.path = path.to_path_buf();
            Ok(registry)
        } else {
            Ok(Self {
                schema_version: 1,
                nodes: HashMap::new(),
                next_id: 1,
                path: path.to_path_buf(),
            })
        }
    }

    /// Load the registry with an exclusive file lock.
    ///
    /// Returns the registry and the lock file handle. The lock is held until the
    /// file handle is dropped, so callers should keep it alive for the duration of
    /// their read-modify-write cycle.
    pub fn load_locked(path: &Path) -> Result<(Self, File)> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let lock_path = path.with_extension("lock");
        let lock_file = File::create(&lock_path)?;
        lock_file.lock_exclusive()?;

        let registry = Self::load(path)?;
        Ok((registry, lock_file))
    }

    /// Save the registry to disk.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, contents)?;
        Ok(())
    }

    /// Get a node by ID.
    pub fn get(&self, id: u32) -> Result<&NodeConfig> {
        self.nodes.get(&id).ok_or(Error::NodeNotFound(id))
    }

    /// Add a node and return its assigned ID.
    pub fn add(&mut self, mut config: NodeConfig) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        config.id = id;
        self.nodes.insert(id, config);
        id
    }

    /// Add multiple nodes at once and return their assigned IDs.
    pub fn add_batch(&mut self, configs: Vec<NodeConfig>) -> Vec<u32> {
        configs.into_iter().map(|config| self.add(config)).collect()
    }

    /// Remove a node by ID.
    pub fn remove(&mut self, id: u32) -> Result<NodeConfig> {
        self.nodes.remove(&id).ok_or(Error::NodeNotFound(id))
    }

    /// List all nodes.
    pub fn list(&self) -> Vec<&NodeConfig> {
        let mut nodes: Vec<_> = self.nodes.values().collect();
        nodes.sort_by_key(|n| n.id);
        nodes
    }

    /// Number of registered nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Whether the registry is empty.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::NamedTempFile;

    fn make_config(id: u32) -> NodeConfig {
        NodeConfig {
            id,
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
        }
    }

    #[test]
    fn load_creates_empty_registry() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        // File doesn't exist at this path
        let reg = NodeRegistry::load(&path).unwrap();
        assert!(reg.is_empty());
        assert_eq!(reg.next_id, 1);
    }

    #[test]
    fn add_and_get() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let mut reg = NodeRegistry::load(&path).unwrap();
        let id = reg.add(make_config(0));
        assert_eq!(id, 1);
        assert_eq!(reg.get(id).unwrap().rewards_address, "0xtest");
    }

    #[test]
    fn save_and_reload() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let mut reg = NodeRegistry::load(&path).unwrap();
        reg.add(make_config(0));
        reg.save().unwrap();

        let reg2 = NodeRegistry::load(&path).unwrap();
        assert_eq!(reg2.len(), 1);
    }

    #[test]
    fn add_batch_assigns_sequential_ids() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let mut reg = NodeRegistry::load(&path).unwrap();
        let configs = vec![make_config(0), make_config(0), make_config(0)];
        let ids = reg.add_batch(configs);
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(reg.len(), 3);
        assert_eq!(reg.next_id, 4);
    }

    #[test]
    fn load_locked_creates_lock_file() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let (reg, _lock) = NodeRegistry::load_locked(&path).unwrap();
        assert!(reg.is_empty());
        assert!(path.with_extension("lock").exists());
    }

    #[test]
    fn remove_returns_config() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let mut reg = NodeRegistry::load(&path).unwrap();
        let id = reg.add(make_config(0));
        let removed = reg.remove(id).unwrap();
        assert_eq!(removed.rewards_address, "0xtest");
        assert!(reg.is_empty());
    }

    #[test]
    fn remove_missing_node_errors() {
        let tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().with_extension("json");
        let mut reg = NodeRegistry::load(&path).unwrap();
        let result = reg.remove(999);
        assert!(result.is_err());
    }
}
