use tokio::sync::broadcast;

use crate::error::{Error, Result};
use crate::node::events::NodeEvent;
use crate::node::types::NodeStatus;

/// Manages running node processes. Holds child process handles and runtime state.
///
/// Full supervision (restart, backoff, crash detection) will be implemented in a later task.
pub struct Supervisor {
    event_tx: broadcast::Sender<NodeEvent>,
    /// Runtime status of each node, keyed by node ID.
    node_states: std::collections::HashMap<u32, NodeRuntime>,
}

struct NodeRuntime {
    status: NodeStatus,
    pid: Option<u32>,
    started_at: Option<std::time::Instant>,
}

impl Supervisor {
    pub fn new(event_tx: broadcast::Sender<NodeEvent>) -> Self {
        Self {
            event_tx,
            node_states: std::collections::HashMap::new(),
        }
    }

    /// Start a node. Stub: will spawn the actual process in a later task.
    pub async fn start_node(&mut self, node_id: u32) -> Result<()> {
        if let Some(state) = self.node_states.get(&node_id) {
            if state.status == NodeStatus::Running {
                return Err(Error::NodeAlreadyRunning(node_id));
            }
        }

        let _ = self.event_tx.send(NodeEvent::NodeStarting { node_id });

        self.node_states.insert(
            node_id,
            NodeRuntime {
                status: NodeStatus::Running,
                pid: None,
                started_at: Some(std::time::Instant::now()),
            },
        );

        let _ = self
            .event_tx
            .send(NodeEvent::NodeStarted { node_id, pid: 0 });

        Ok(())
    }

    /// Stop a node. Stub: will signal the actual process in a later task.
    pub async fn stop_node(&mut self, node_id: u32) -> Result<()> {
        let state = self
            .node_states
            .get_mut(&node_id)
            .ok_or(Error::NodeNotFound(node_id))?;

        if state.status != NodeStatus::Running {
            return Err(Error::NodeNotRunning(node_id));
        }

        let _ = self.event_tx.send(NodeEvent::NodeStopping { node_id });

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
}
