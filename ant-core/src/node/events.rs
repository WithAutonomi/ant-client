use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Structured events emitted by the daemon supervisor and long-running operations.
///
/// Serialized to JSON for SSE streaming at `GET /api/v1/events`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum NodeEvent {
    NodeStarting {
        node_id: u32,
    },
    NodeStarted {
        node_id: u32,
        pid: u32,
    },
    NodeStopping {
        node_id: u32,
    },
    NodeStopped {
        node_id: u32,
    },
    NodeCrashed {
        node_id: u32,
        exit_code: Option<i32>,
    },
    NodeRestarting {
        node_id: u32,
        attempt: u32,
    },
    NodeErrored {
        node_id: u32,
        message: String,
    },
    DownloadStarted {
        version: String,
    },
    DownloadProgress {
        bytes: u64,
        total: u64,
    },
    DownloadComplete {
        version: String,
        path: PathBuf,
    },
}

impl NodeEvent {
    /// Returns the SSE event type name for this event.
    pub fn event_type(&self) -> &'static str {
        match self {
            NodeEvent::NodeStarting { .. } => "node_starting",
            NodeEvent::NodeStarted { .. } => "node_started",
            NodeEvent::NodeStopping { .. } => "node_stopping",
            NodeEvent::NodeStopped { .. } => "node_stopped",
            NodeEvent::NodeCrashed { .. } => "node_crashed",
            NodeEvent::NodeRestarting { .. } => "node_restarting",
            NodeEvent::NodeErrored { .. } => "node_errored",
            NodeEvent::DownloadStarted { .. } => "download_started",
            NodeEvent::DownloadProgress { .. } => "download_progress",
            NodeEvent::DownloadComplete { .. } => "download_complete",
        }
    }
}

/// Trait for receiving node lifecycle events.
pub trait EventListener: Send + Sync {
    fn on_event(&self, event: NodeEvent);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_with_type_tag() {
        let event = NodeEvent::NodeStarted {
            node_id: 1,
            pid: 1234,
        };
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"type\":\"node_started\""));
        assert!(json.contains("\"node_id\":1"));
        assert!(json.contains("\"pid\":1234"));
    }

    #[test]
    fn event_type_matches_serde_tag() {
        let event = NodeEvent::NodeCrashed {
            node_id: 2,
            exit_code: Some(1),
        };
        assert_eq!(event.event_type(), "node_crashed");

        // Verify the serde tag matches
        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains(&format!("\"type\":\"{}\"", event.event_type())));
    }

    #[test]
    fn event_roundtrips() {
        let event = NodeEvent::DownloadProgress {
            bytes: 1024,
            total: 4096,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: NodeEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.event_type(), "download_progress");
    }
}
