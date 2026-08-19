//! Building the document that actually goes to Elasticsearch.
//!
//! Field names here are not ours to choose: they are the beta index's mapping (V2-1016), and a
//! mismatch means a field lands as dynamically-mapped text instead of the keyword the dashboards
//! aggregate on. Two in particular read wrong at a glance and are right:
//!
//! - the time field is `@timestamp`, not `timestamp`;
//! - the node's build is `binary_version`, while `version` and `commit` carry whatever ant-node
//!   said about *itself* on its startup line. Keeping them separate avoids two half-populated
//!   fields meaning the same thing with no rule for which wins.
//!
//! Two mapped fields are deliberately never sent. `host` is stripped by the ingest pipeline —
//! machine hostnames routinely contain someone's name — and `beta_user` is stamped server-side from
//! the authenticated API key, so anything we sent would be discarded and replaced anyway.

use serde::Serialize;

use super::tail::TailedEvent;
use crate::node::types::NodeConfig;

/// Value used for `channel` when a node has never been given an explicit upgrade channel.
///
/// Not folded into `"stable"`: the beta cohort is counted by aggregating this field, and claiming a
/// node is on stable when nobody ever said so would quietly distort that count.
const CHANNEL_UNSET: &str = "unset";

/// The identity fields every event from a given node carries.
///
/// Resolved once when the forwarder picks the node up, rather than per event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeTags {
    pub node_id: u32,
    pub service: String,
    pub binary_version: String,
    pub channel: String,
}

impl NodeTags {
    #[must_use]
    pub fn from_config(config: &NodeConfig) -> Self {
        Self {
            node_id: config.id,
            service: config.service_name.clone(),
            binary_version: config.version.clone(),
            channel: config
                .upgrade_channel
                .map_or_else(|| CHANNEL_UNSET.to_string(), |channel| channel.to_string()),
        }
    }
}

/// A document ready to be framed into a bulk request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForwardDocument {
    /// Deterministic `_id`, so replaying a batch is idempotent.
    pub id: String,
    /// Daily index, derived from this event's own timestamp.
    pub index: String,
    pub source: DocumentSource,
}

/// The `_source` body of a forwarded document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DocumentSource {
    #[serde(rename = "@timestamp")]
    pub timestamp: String,
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub message: String,

    /// Keyword in the mapping, so it is sent as a string rather than a number.
    pub node_id: String,
    pub service: String,
    pub binary_version: String,
    pub channel: String,
    pub os: String,
    pub arch: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl ForwardDocument {
    /// Build a document, or `None` if the event's timestamp cannot name an index.
    ///
    /// Parsing already rejects unusable timestamps in both layouts, so `None` here is a
    /// belt-and-braces case rather than an expected one.
    #[must_use]
    pub fn build(tailed: &TailedEvent, tags: &NodeTags, index_prefix: &str) -> Option<Self> {
        let index = format!("{index_prefix}-{}", tailed.event.index_date()?);

        Some(Self {
            id: tailed.document_id(),
            index,
            source: DocumentSource {
                timestamp: tailed.event.timestamp.clone(),
                level: tailed.event.level.as_str().to_string(),
                target: tailed.event.target.clone(),
                message: tailed.event.message.clone(),
                node_id: tags.node_id.to_string(),
                service: tags.service.clone(),
                binary_version: tags.binary_version.clone(),
                channel: tags.channel.clone(),
                os: std::env::consts::OS.to_string(),
                arch: std::env::consts::ARCH.to_string(),
                peer_id: tailed.event.peer_id.clone(),
                version: tailed.event.version.clone(),
                commit: tailed.event.commit.clone(),
            },
        })
    }

    /// Approximate serialized size, used to keep a batch well under the endpoint's body cap.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        // The action line plus the source line, give or take the JSON punctuation.
        self.id.len() + self.index.len() + self.source.message.len() + 256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::daemon::forward::parse::parse_line;
    use crate::node::types::{EvmNetwork, UpgradeChannel};
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn node_config(channel: Option<UpgradeChannel>) -> NodeConfig {
        NodeConfig {
            id: 7,
            service_name: "node7".to_string(),
            rewards_address: "0xabc".to_string(),
            data_dir: PathBuf::from("/data/node-7"),
            log_dir: Some(PathBuf::from("/logs/node-7")),
            node_port: None,
            binary_path: PathBuf::from("/bin/antnode"),
            version: "0.17.2-beta.1".to_string(),
            env_variables: HashMap::new(),
            bootstrap_peers: Vec::new(),
            upgrade_channel: channel,
            evm_network: EvmNetwork::default(),
            eviction: None,
        }
    }

    fn tailed(line: &str) -> TailedEvent {
        TailedEvent {
            node_id: 7,
            file_name: "ant-node.2026-08-19.log".to_string(),
            byte_offset: 4096,
            event: parse_line(line).unwrap(),
        }
    }

    const LINE: &str =
        "2026-08-19T20:50:00.123456Z  INFO ant_node::node: connected peer_id=12D3KooWabc";

    #[test]
    fn builds_a_document_with_the_mapped_field_names() {
        let tags = NodeTags::from_config(&node_config(Some(UpgradeChannel::Beta)));
        let document = ForwardDocument::build(&tailed(LINE), &tags, "beta-nodes").unwrap();

        assert_eq!(document.id, "7-ant-node.2026-08-19.log-4096");
        assert_eq!(document.index, "beta-nodes-2026.08.19");

        let json: serde_json::Value = serde_json::to_value(&document.source).unwrap();
        assert_eq!(json["@timestamp"], "2026-08-19T20:50:00.123456Z");
        assert_eq!(json["level"], "INFO");
        assert_eq!(json["target"], "ant_node::node");
        assert_eq!(json["node_id"], "7");
        assert_eq!(json["service"], "node7");
        assert_eq!(json["binary_version"], "0.17.2-beta.1");
        assert_eq!(json["channel"], "beta");
        assert_eq!(json["peer_id"], "12D3KooWabc");
        assert_eq!(json["os"], std::env::consts::OS);
        assert_eq!(json["arch"], std::env::consts::ARCH);
    }

    /// The time field is `@timestamp`; a document using `timestamp` would not be searchable by time.
    #[test]
    fn the_time_field_is_at_timestamp_and_nothing_else() {
        let tags = NodeTags::from_config(&node_config(None));
        let document = ForwardDocument::build(&tailed(LINE), &tags, "beta-nodes").unwrap();
        let json = serde_json::to_value(&document.source).unwrap();

        assert!(json.get("@timestamp").is_some());
        assert!(json.get("timestamp").is_none());
    }

    /// Both are stamped or stripped server-side; sending them is at best pointless.
    #[test]
    fn host_and_beta_user_are_never_sent() {
        let tags = NodeTags::from_config(&node_config(Some(UpgradeChannel::Beta)));
        let document = ForwardDocument::build(&tailed(LINE), &tags, "beta-nodes").unwrap();
        let json = serde_json::to_value(&document.source).unwrap();

        assert!(json.get("host").is_none());
        assert!(json.get("beta_user").is_none());
    }

    #[test]
    fn an_unspecified_channel_is_not_reported_as_stable() {
        let tags = NodeTags::from_config(&node_config(None));
        assert_eq!(tags.channel, "unset");

        let stable = NodeTags::from_config(&node_config(Some(UpgradeChannel::Stable)));
        assert_eq!(stable.channel, "stable");
    }

    #[test]
    fn the_index_comes_from_the_events_timestamp_not_the_wall_clock() {
        let tags = NodeTags::from_config(&node_config(None));

        let yesterday = ForwardDocument::build(
            &tailed("2026-08-19T23:59:59.000000Z  INFO ant_node: late"),
            &tags,
            "beta-nodes",
        )
        .unwrap();
        let today = ForwardDocument::build(
            &tailed("2026-08-20T00:00:01.000000Z  INFO ant_node: early"),
            &tags,
            "beta-nodes",
        )
        .unwrap();

        assert_eq!(yesterday.index, "beta-nodes-2026.08.19");
        assert_eq!(today.index, "beta-nodes-2026.08.20");
    }

    /// The property that makes a replayed batch idempotent: same event in, same `_id` and index out,
    /// no matter when the replay happens.
    #[test]
    fn rebuilding_the_same_event_yields_the_same_id_and_index() {
        let tags = NodeTags::from_config(&node_config(Some(UpgradeChannel::Beta)));
        let first = ForwardDocument::build(&tailed(LINE), &tags, "beta-nodes").unwrap();
        let second = ForwardDocument::build(&tailed(LINE), &tags, "beta-nodes").unwrap();

        assert_eq!(first.id, second.id);
        assert_eq!(first.index, second.index);
    }

    #[test]
    fn absent_optional_fields_are_omitted_rather_than_sent_as_null() {
        let tags = NodeTags::from_config(&node_config(None));
        let document = ForwardDocument::build(
            &tailed("2026-08-19T20:50:00.123456Z  INFO plain message with no fields"),
            &tags,
            "beta-nodes",
        )
        .unwrap();
        let json = serde_json::to_value(&document.source).unwrap();

        assert!(json.get("peer_id").is_none());
        assert!(json.get("version").is_none());
        assert!(json.get("commit").is_none());
        assert!(json.get("target").is_none());
    }

    #[test]
    fn a_custom_index_prefix_is_honoured() {
        let tags = NodeTags::from_config(&node_config(None));
        let document = ForwardDocument::build(&tailed(LINE), &tags, "my-test-index").unwrap();
        assert_eq!(document.index, "my-test-index-2026.08.19");
    }
}
