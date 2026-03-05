use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, RwLock};

use crate::error::{Error, Result};
use crate::node::events::NodeEvent;
use crate::node::process::spawn::spawn_node;
use crate::node::types::{NodeConfig, NodeStarted, NodeStatus};

/// Maximum restart attempts before marking a node as errored.
const MAX_CRASHES_BEFORE_ERRORED: u32 = 5;

/// Window in which crashes are counted. If this many crashes happen within
/// this duration, the node is marked errored.
const CRASH_WINDOW: Duration = Duration::from_secs(300); // 5 minutes

/// If a node runs for this long without crashing, reset the crash counter.
const STABLE_DURATION: Duration = Duration::from_secs(300); // 5 minutes

/// Maximum backoff delay between restarts.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Manages running node processes. Holds child process handles and runtime state.
pub struct Supervisor {
    event_tx: broadcast::Sender<NodeEvent>,
    /// Runtime status of each node, keyed by node ID.
    node_states: HashMap<u32, NodeRuntime>,
}

struct NodeRuntime {
    status: NodeStatus,
    pid: Option<u32>,
    started_at: Option<Instant>,
    restart_count: u32,
    first_crash_at: Option<Instant>,
}

impl Supervisor {
    pub fn new(event_tx: broadcast::Sender<NodeEvent>) -> Self {
        Self {
            event_tx,
            node_states: HashMap::new(),
        }
    }

    /// Start a node by spawning the actual process.
    ///
    /// Returns `NodeStarted` on success. Spawns a background monitoring task
    /// that watches the child process and handles restart logic.
    pub async fn start_node(
        &mut self,
        config: &NodeConfig,
        supervisor_ref: Arc<RwLock<Supervisor>>,
    ) -> Result<NodeStarted> {
        let node_id = config.id;

        if let Some(state) = self.node_states.get(&node_id) {
            if state.status == NodeStatus::Running {
                return Err(Error::NodeAlreadyRunning(node_id));
            }
        }

        let _ = self.event_tx.send(NodeEvent::NodeStarting { node_id });

        let mut child = spawn_node_from_config(config).await?;
        let pid = child
            .id()
            .ok_or_else(|| Error::ProcessSpawn("Failed to get PID from spawned process".into()))?;

        // Brief health check: give the process a moment to start, then check if it
        // exited immediately. This catches errors like invalid CLI arguments or missing
        // shared libraries. We use timeout + wait() rather than try_wait() because
        // tokio's child reaper requires the wait future to be polled.
        match tokio::time::timeout(Duration::from_secs(1), child.wait()).await {
            Ok(Ok(exit_status)) => {
                // Process already exited — read stderr for details.
                // spawn_node always redirects stderr to a file in the log dir
                // (falling back to data_dir when no log dir is configured).
                let spawn_log_dir = config.log_dir.as_deref().unwrap_or(&config.data_dir);
                let stderr_path = spawn_log_dir.join("stderr.log");
                let stderr_msg = std::fs::read_to_string(&stderr_path).unwrap_or_default();
                let detail = if stderr_msg.trim().is_empty() {
                    format!("exit code: {exit_status}")
                } else {
                    stderr_msg.trim().to_string()
                };
                self.node_states.insert(
                    node_id,
                    NodeRuntime {
                        status: NodeStatus::Errored,
                        pid: None,
                        started_at: None,
                        restart_count: 0,
                        first_crash_at: None,
                    },
                );
                return Err(Error::ProcessSpawn(format!(
                    "Node {node_id} exited immediately: {detail}"
                )));
            }
            Ok(Err(e)) => {
                return Err(Error::ProcessSpawn(format!(
                    "Failed to check node process status: {e}"
                )));
            }
            Err(_) => {} // Timeout — process is still running after 1s, good
        }

        self.node_states.insert(
            node_id,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(pid),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
            },
        );

        let _ = self.event_tx.send(NodeEvent::NodeStarted { node_id, pid });

        let result = NodeStarted {
            node_id,
            service_name: config.service_name.clone(),
            pid,
        };

        // Spawn monitoring task
        let event_tx = self.event_tx.clone();
        let config = config.clone();
        tokio::spawn(async move {
            monitor_node(child, config, supervisor_ref, event_tx).await;
        });

        Ok(result)
    }

    /// Stop a node by killing its process.
    pub async fn stop_node(&mut self, node_id: u32) -> Result<()> {
        let state = self
            .node_states
            .get_mut(&node_id)
            .ok_or(Error::NodeNotFound(node_id))?;

        if state.status != NodeStatus::Running {
            return Err(Error::NodeNotRunning(node_id));
        }

        let _ = self.event_tx.send(NodeEvent::NodeStopping { node_id });

        state.status = NodeStatus::Stopping;

        // The actual kill is handled by the monitor task detecting the status change,
        // or we could kill it here. For now, mark as stopped.
        state.status = NodeStatus::Stopped;
        state.pid = None;
        state.started_at = None;

        let _ = self.event_tx.send(NodeEvent::NodeStopped { node_id });

        Ok(())
    }

    /// Get the status of a node.
    pub fn node_status(&self, node_id: u32) -> Result<NodeStatus> {
        self.node_states
            .get(&node_id)
            .map(|s| s.status)
            .ok_or(Error::NodeNotFound(node_id))
    }

    /// Get the PID of a running node.
    pub fn node_pid(&self, node_id: u32) -> Option<u32> {
        self.node_states.get(&node_id).and_then(|s| s.pid)
    }

    /// Get the uptime of a running node in seconds.
    pub fn node_uptime_secs(&self, node_id: u32) -> Option<u64> {
        self.node_states
            .get(&node_id)
            .and_then(|s| s.started_at.map(|t| t.elapsed().as_secs()))
    }

    /// Check whether a node is running.
    pub fn is_running(&self, node_id: u32) -> bool {
        self.node_states
            .get(&node_id)
            .is_some_and(|s| s.status == NodeStatus::Running)
    }

    /// Get counts of nodes in each state: (running, stopped, errored).
    pub fn node_counts(&self) -> (u32, u32, u32) {
        let mut running = 0u32;
        let mut stopped = 0u32;
        let mut errored = 0u32;
        for state in self.node_states.values() {
            match state.status {
                NodeStatus::Running | NodeStatus::Starting => running += 1,
                NodeStatus::Stopped | NodeStatus::Stopping => stopped += 1,
                NodeStatus::Errored => errored += 1,
            }
        }
        (running, stopped, errored)
    }

    /// Update the runtime state for a node (used by the monitor task).
    fn update_state(&mut self, node_id: u32, status: NodeStatus, pid: Option<u32>) {
        if let Some(state) = self.node_states.get_mut(&node_id) {
            state.status = status;
            state.pid = pid;
            if status == NodeStatus::Running {
                state.started_at = Some(Instant::now());
            }
        }
    }

    /// Record a crash and determine if the node should be restarted or marked errored.
    /// Returns (should_restart, attempt_number, backoff_duration).
    fn record_crash(&mut self, node_id: u32) -> (bool, u32, Duration) {
        let state = match self.node_states.get_mut(&node_id) {
            Some(s) => s,
            None => return (false, 0, Duration::ZERO),
        };

        let now = Instant::now();

        // Check if we were stable long enough to reset crash counter
        if let Some(started_at) = state.started_at {
            if started_at.elapsed() >= STABLE_DURATION {
                state.restart_count = 0;
                state.first_crash_at = None;
            }
        }

        state.restart_count += 1;
        let attempt = state.restart_count;

        if state.first_crash_at.is_none() {
            state.first_crash_at = Some(now);
        }

        // Check if too many crashes in the window
        if let Some(first_crash) = state.first_crash_at {
            if attempt >= MAX_CRASHES_BEFORE_ERRORED
                && now.duration_since(first_crash) < CRASH_WINDOW
            {
                state.status = NodeStatus::Errored;
                state.pid = None;
                state.started_at = None;
                return (false, attempt, Duration::ZERO);
            }
        }

        // Exponential backoff: 1s, 2s, 4s, 8s, 16s, 32s, 60s cap
        let backoff_secs = 1u64 << (attempt - 1).min(5);
        let backoff = Duration::from_secs(backoff_secs).min(MAX_BACKOFF);

        (true, attempt, backoff)
    }
}

/// Build CLI arguments for the node binary from a NodeConfig.
pub fn build_node_args(config: &NodeConfig) -> Vec<String> {
    let mut args = vec![
        "--rewards-address".to_string(),
        config.rewards_address.clone(),
        "--root-dir".to_string(),
        config.data_dir.display().to_string(),
    ];

    if let Some(ref log_dir) = config.log_dir {
        args.push("--log-dir".to_string());
        args.push(log_dir.display().to_string());
    }

    if let Some(port) = config.node_port {
        args.push("--port".to_string());
        args.push(port.to_string());
    }

    if let Some(port) = config.metrics_port {
        args.push("--metrics-port".to_string());
        args.push(port.to_string());
    }

    for peer in &config.bootstrap_peers {
        args.push("--bootstrap".to_string());
        args.push(peer.clone());
    }

    args
}

/// Spawn a node process from a NodeConfig.
async fn spawn_node_from_config(config: &NodeConfig) -> Result<tokio::process::Child> {
    let args = build_node_args(config);
    let env_vars: Vec<(String, String)> = config.env_variables.clone().into_iter().collect();

    let log_dir = config
        .log_dir
        .as_deref()
        .unwrap_or(config.data_dir.as_path());

    spawn_node(&config.binary_path, &args, &env_vars, log_dir).await
}

/// Monitor a node process. On exit, handle restart logic.
async fn monitor_node(
    mut child: tokio::process::Child,
    config: NodeConfig,
    supervisor: Arc<RwLock<Supervisor>>,
    event_tx: broadcast::Sender<NodeEvent>,
) {
    let node_id = config.id;

    loop {
        // Wait for the process to exit
        let exit_status = child.wait().await;

        // Check if the node was intentionally stopped
        {
            let sup = supervisor.read().await;
            if let Ok(status) = sup.node_status(node_id) {
                if status == NodeStatus::Stopped || status == NodeStatus::Stopping {
                    return;
                }
            }
        }

        let exit_code = exit_status.ok().and_then(|s| s.code());

        if exit_code == Some(0) {
            // Clean exit
            let mut sup = supervisor.write().await;
            sup.update_state(node_id, NodeStatus::Stopped, None);
            let _ = event_tx.send(NodeEvent::NodeStopped { node_id });
            return;
        }

        // Crash
        let _ = event_tx.send(NodeEvent::NodeCrashed { node_id, exit_code });

        let (should_restart, attempt, backoff) = {
            let mut sup = supervisor.write().await;
            sup.record_crash(node_id)
        };

        if !should_restart {
            let _ = event_tx.send(NodeEvent::NodeErrored {
                node_id,
                message: format!(
                    "Node crashed {} times within {} seconds, giving up",
                    MAX_CRASHES_BEFORE_ERRORED,
                    CRASH_WINDOW.as_secs()
                ),
            });
            return;
        }

        let _ = event_tx.send(NodeEvent::NodeRestarting { node_id, attempt });

        tokio::time::sleep(backoff).await;

        // Try to restart
        match spawn_node_from_config(&config).await {
            Ok(new_child) => {
                let pid = new_child.id().unwrap_or(0);
                {
                    let mut sup = supervisor.write().await;
                    sup.update_state(node_id, NodeStatus::Running, Some(pid));
                }
                let _ = event_tx.send(NodeEvent::NodeStarted { node_id, pid });
                child = new_child;
            }
            Err(e) => {
                let _ = event_tx.send(NodeEvent::NodeErrored {
                    node_id,
                    message: format!("Failed to restart node: {e}"),
                });
                let mut sup = supervisor.write().await;
                sup.update_state(node_id, NodeStatus::Errored, None);
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_node_args_basic() {
        let config = NodeConfig {
            id: 1,
            service_name: "node1".to_string(),
            rewards_address: "0xabc123".to_string(),
            data_dir: "/data/node-1".into(),
            log_dir: Some("/logs/node-1".into()),
            node_port: Some(12000),
            metrics_port: Some(13000),
            network_id: Some(1),
            binary_path: "/bin/node".into(),
            version: "0.1.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec!["peer1".to_string(), "peer2".to_string()],
        };

        let args = build_node_args(&config);

        assert!(args.contains(&"--rewards-address".to_string()));
        assert!(args.contains(&"0xabc123".to_string()));
        assert!(args.contains(&"--root-dir".to_string()));
        assert!(args.contains(&"/data/node-1".to_string()));
        assert!(args.contains(&"--log-dir".to_string()));
        assert!(args.contains(&"/logs/node-1".to_string()));
        assert!(args.contains(&"--port".to_string()));
        assert!(args.contains(&"12000".to_string()));
        assert!(args.contains(&"--metrics-port".to_string()));
        assert!(args.contains(&"13000".to_string()));
        assert!(args.contains(&"--bootstrap".to_string()));
        assert!(args.contains(&"peer1".to_string()));
        assert!(args.contains(&"peer2".to_string()));
    }

    #[test]
    fn build_node_args_minimal() {
        let config = NodeConfig {
            id: 1,
            service_name: "node1".to_string(),
            rewards_address: "0xabc".to_string(),
            data_dir: "/data/node-1".into(),
            log_dir: None,
            node_port: None,
            metrics_port: None,
            network_id: None,
            binary_path: "/bin/node".into(),
            version: "0.1.0".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: vec![],
        };

        let args = build_node_args(&config);

        assert!(args.contains(&"--rewards-address".to_string()));
        assert!(args.contains(&"--root-dir".to_string()));
        assert!(!args.contains(&"--log-dir".to_string()));
        assert!(!args.contains(&"--port".to_string()));
        assert!(!args.contains(&"--metrics-port".to_string()));
        assert!(!args.contains(&"--bootstrap".to_string()));
    }

    #[test]
    fn record_crash_backoff_increases() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        // Insert a running node
        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(100),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
            },
        );

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 1);
        assert_eq!(backoff, Duration::from_secs(1));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 2);
        assert_eq!(backoff, Duration::from_secs(2));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 3);
        assert_eq!(backoff, Duration::from_secs(4));

        let (should_restart, attempt, backoff) = sup.record_crash(1);
        assert!(should_restart);
        assert_eq!(attempt, 4);
        assert_eq!(backoff, Duration::from_secs(8));

        // 5th crash within window → errored
        let (should_restart, attempt, _) = sup.record_crash(1);
        assert!(!should_restart);
        assert_eq!(attempt, 5);
        assert_eq!(sup.node_states[&1].status, NodeStatus::Errored);
    }

    #[test]
    fn node_counts_tracks_states() {
        let (tx, _rx) = broadcast::channel(16);
        let mut sup = Supervisor::new(tx);

        sup.node_states.insert(
            1,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: Some(100),
                started_at: Some(Instant::now()),
                restart_count: 0,
                first_crash_at: None,
            },
        );
        sup.node_states.insert(
            2,
            NodeRuntime {
                status: NodeStatus::Stopped,
                pid: None,
                started_at: None,
                restart_count: 0,
                first_crash_at: None,
            },
        );
        sup.node_states.insert(
            3,
            NodeRuntime {
                status: NodeStatus::Errored,
                pid: None,
                started_at: None,
                restart_count: 5,
                first_crash_at: None,
            },
        );

        let (running, stopped, errored) = sup.node_counts();
        assert_eq!(running, 1);
        assert_eq!(stopped, 1);
        assert_eq!(errored, 1);
    }
}
