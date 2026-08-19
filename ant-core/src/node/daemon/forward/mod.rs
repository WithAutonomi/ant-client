//! Opt-in forwarding of managed nodes' log files to the beta-channel Elasticsearch.
//!
//! The daemon already knows the log directory of every node it manages, so forwarding needs no OS
//! service, no separate install, and nothing platform-specific: `ant node logs forward enable` is
//! the consent act, and from then on a background task tails those files and batch-ships their
//! events until the user runs `disable`.
//!
//! Three properties shape the whole design:
//!
//! - **It must never slow a node down.** The forwarder only *reads* log files. It never touches a
//!   node process, its stdio, or any lock on the node's path, and all of its work happens on its
//!   own task.
//! - **Delivery is best-effort.** This is logs-only telemetry, so a lost batch is acceptable and
//!   nothing here is allowed to grow without bound waiting for the endpoint to come back.
//! - **A daemon restart must not duplicate or lose events.** Tail offsets are persisted, and every
//!   document carries a deterministic `_id` so that replaying a batch after a transport failure is
//!   idempotent rather than duplicating whatever already landed.

pub mod config;
pub mod document;
pub mod es;
pub mod offsets;
pub mod parse;
pub mod runner;
pub mod sink;
pub mod tail;

use serde::{Deserialize, Serialize};

pub use config::{LogForwardConfig, LogLevel, DEFAULT_ENDPOINT, DEFAULT_INDEX_PREFIX};
pub use document::{ForwardDocument, NodeTags};
pub use es::ElasticsearchSink;
pub use offsets::OffsetStore;
pub use parse::{parse_line, LogEvent};
pub use runner::{classify_nodes, spawn_log_forwarder, ForwarderHandle, DEFAULT_POLL_INTERVAL};
pub use sink::{BatchOutcome, DocumentOutcome, DocumentQueue, LogSink, RetryPolicy};
pub use tail::{LogTailer, TailedEvent};

/// A node the forwarder is tailing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ForwardingNode {
    pub node_id: u32,
    pub service: String,
    /// Log directory being tailed.
    pub log_dir: String,
}

/// A node the forwarder cannot tail, and why.
///
/// The common case by far is a node added without `--log-dir-path`: node file logging is off by
/// default, so such a node writes no log files at all and there is nothing to forward. Surfacing
/// these explicitly is what stops `enable` looking like it succeeded while shipping nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct SkippedNode {
    pub node_id: u32,
    pub service: String,
    /// Human-readable explanation, suitable for printing directly.
    pub reason: String,
}

impl SkippedNode {
    /// The skip reason for a node that has no log directory configured.
    #[must_use]
    pub fn no_logging(node_id: u32, service: impl Into<String>) -> Self {
        Self {
            node_id,
            service: service.into(),
            reason: "logging is not enabled for this node — re-add it with --log-dir-path to \
                     forward its logs"
                .to_string(),
        }
    }
}

/// Counters describing what the forwarder has done since the daemon started.
///
/// Deliberately cheap to maintain and safe to lose: these are for answering "is it working?", not
/// for accounting. They reset when the daemon restarts.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ForwardStats {
    /// Events accepted by the endpoint.
    pub events_forwarded: u64,
    /// Events dropped locally for being below the configured minimum level.
    pub events_dropped_by_level: u64,
    /// Events dropped because the in-memory queue was full — the endpoint could not keep up and
    /// the forwarder chose to bound its memory rather than block.
    pub events_dropped_by_overflow: u64,
    /// Batches the endpoint accepted in full.
    pub batches_sent: u64,
    /// Batches abandoned after exhausting their retries.
    pub batches_failed: u64,
    /// Unix seconds of the last batch the endpoint accepted.
    pub last_success_unix: Option<u64>,
    /// Most recent delivery error, retained so `status` can explain a stalled flow.
    pub last_error: Option<String>,
}

/// Everything `ant node logs forward status` reports.
///
/// Carries a token *fingerprint*, never the token: the daemon serves this over HTTP, and handing
/// the write key back out would widen the blast radius of anything that can reach the API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LogForwardStatus {
    pub enabled: bool,
    pub endpoint: String,
    pub index_prefix: String,
    pub min_level: LogLevel,
    /// Short, non-reversible identifier for the configured token. `None` when none is set.
    pub token_fingerprint: Option<String>,
    /// Whether the background task is currently running. Distinguishes "enabled but the daemon has
    /// not been restarted yet" from "enabled and shipping".
    pub active: bool,
    pub nodes_forwarding: Vec<ForwardingNode>,
    pub nodes_skipped: Vec<SkippedNode>,
    pub stats: ForwardStats,
}

impl LogForwardStatus {
    /// Build a status from persisted config alone, for the case where no forwarder is running —
    /// either forwarding is disabled, or the CLI is reading the config with the daemon down.
    #[must_use]
    pub fn inactive(config: &LogForwardConfig) -> Self {
        Self {
            enabled: config.enabled,
            endpoint: config.endpoint.clone(),
            index_prefix: config.index_prefix.clone(),
            min_level: config.min_level,
            token_fingerprint: config.token_fingerprint(),
            active: false,
            nodes_forwarding: Vec::new(),
            nodes_skipped: Vec::new(),
            stats: ForwardStats::default(),
        }
    }
}

/// Request body for enabling forwarding.
///
/// Every field is optional so that re-enabling after a `disable` needs no arguments: the stored
/// token, endpoint and level are reused unless the caller overrides them. Only the very first
/// `enable` on a machine has to supply a token.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LogForwardEnableRequest {
    /// Write-only Elasticsearch API key. Reuses the stored one when omitted.
    #[serde(default)]
    pub token: Option<String>,
    /// Ingest endpoint override, for testing against a local sink.
    #[serde(default)]
    pub endpoint: Option<String>,
    /// Minimum level to forward. Defaults to INFO on first enable.
    #[serde(default)]
    pub min_level: Option<LogLevel>,
}

/// Merge a request onto the stored config and validate the result.
///
/// Kept out of both the HTTP handler and the CLI so the two paths cannot drift: `enable` means the
/// same thing whether it arrives over the daemon's API or is written straight to disk with the
/// daemon stopped.
pub fn apply_enable(
    stored: &LogForwardConfig,
    request: &LogForwardEnableRequest,
) -> crate::error::Result<LogForwardConfig> {
    let mut config = stored.clone();
    config.enabled = true;

    if let Some(token) = &request.token {
        config.token = token.trim().to_string();
    }
    if let Some(endpoint) = &request.endpoint {
        config.endpoint = endpoint.trim().to_string();
    }
    if let Some(level) = request.min_level {
        config.min_level = level;
    }

    config.validate()?;
    Ok(config)
}

/// Outcome of `enable` or `disable`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct LogForwardResult {
    /// Whether forwarding is enabled after the call.
    pub enabled: bool,
    /// True when the call found the setting already in the requested state.
    pub already_in_state: bool,
    pub endpoint: String,
    pub min_level: LogLevel,
    /// Nodes that will be tailed.
    pub nodes_forwarding: Vec<ForwardingNode>,
    /// Nodes that cannot be tailed, with reasons — most often because they have no log directory.
    pub nodes_skipped: Vec<SkippedNode>,
    /// Set when the config was persisted but no forwarder could be started because the daemon is
    /// not running; forwarding begins when it next starts.
    pub pending_daemon_start: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stored_with_token() -> LogForwardConfig {
        LogForwardConfig {
            enabled: false,
            token: "stored-key".to_string(),
            ..LogForwardConfig::disabled()
        }
    }

    #[test]
    fn enabling_for_the_first_time_requires_a_token() {
        let error = apply_enable(
            &LogForwardConfig::disabled(),
            &LogForwardEnableRequest::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("token"), "{error}");
    }

    /// Re-enabling after `disable` must not make the user find their key again.
    #[test]
    fn re_enabling_reuses_the_stored_token_and_settings() {
        let stored = LogForwardConfig {
            endpoint: "http://127.0.0.1:9999".to_string(),
            min_level: LogLevel::Warn,
            ..stored_with_token()
        };

        let config = apply_enable(&stored, &LogForwardEnableRequest::default()).unwrap();

        assert!(config.enabled);
        assert_eq!(config.token, "stored-key");
        assert_eq!(config.endpoint, "http://127.0.0.1:9999");
        assert_eq!(config.min_level, LogLevel::Warn);
    }

    #[test]
    fn a_supplied_token_endpoint_and_level_override_what_was_stored() {
        let config = apply_enable(
            &stored_with_token(),
            &LogForwardEnableRequest {
                token: Some("  rotated-key  ".to_string()),
                endpoint: Some("http://localhost:8080".to_string()),
                min_level: Some(LogLevel::Error),
            },
        )
        .unwrap();

        assert_eq!(config.token, "rotated-key", "surrounding space is trimmed");
        assert_eq!(config.endpoint, "http://localhost:8080");
        assert_eq!(config.min_level, LogLevel::Error);
    }

    #[test]
    fn an_invalid_endpoint_is_rejected_before_anything_is_persisted() {
        let error = apply_enable(
            &stored_with_token(),
            &LogForwardEnableRequest {
                endpoint: Some("logs.autonomi.com".to_string()),
                ..LogForwardEnableRequest::default()
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("http(s) URL"), "{error}");
    }

    #[test]
    fn inactive_status_mirrors_the_config_without_exposing_the_token() {
        let config = LogForwardConfig {
            enabled: true,
            token: "secret-api-key".to_string(),
            ..LogForwardConfig::disabled()
        };

        let status = LogForwardStatus::inactive(&config);

        assert!(status.enabled);
        assert!(!status.active);
        assert_eq!(status.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(status.min_level, LogLevel::Info);
        assert_eq!(status.token_fingerprint, config.token_fingerprint());

        let json = serde_json::to_string(&status).unwrap();
        assert!(
            !json.contains("secret-api-key"),
            "status must never carry the token: {json}"
        );
    }

    #[test]
    fn inactive_status_of_a_disabled_config_has_no_fingerprint() {
        let status = LogForwardStatus::inactive(&LogForwardConfig::disabled());
        assert!(!status.enabled);
        assert_eq!(status.token_fingerprint, None);
    }

    #[test]
    fn skip_reason_points_at_the_flag_that_fixes_it() {
        let skipped = SkippedNode::no_logging(3, "node3");
        assert_eq!(skipped.node_id, 3);
        assert_eq!(skipped.service, "node3");
        assert!(skipped.reason.contains("--log-dir-path"));
    }

    #[test]
    fn stats_start_at_zero() {
        let stats = ForwardStats::default();
        assert_eq!(stats.events_forwarded, 0);
        assert_eq!(stats.batches_failed, 0);
        assert_eq!(stats.last_success_unix, None);
        assert_eq!(stats.last_error, None);
    }
}
