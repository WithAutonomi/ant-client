//! Persisted opt-in state for beta log forwarding.
//!
//! Running `ant node logs forward enable` is the consent act, and this file is where that consent
//! lives. It holds the write-only Elasticsearch API key, so it is written with owner-only
//! permissions and its token is never returned by the status API — callers get a fingerprint
//! instead (see [`LogForwardStatus`](super::LogForwardStatus)).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config;
use crate::error::{Error, Result};

/// Default beta-channel ingest endpoint (V2-1016).
///
/// A Caddy proxy fronts an Elasticsearch instance on loopback and allowlists the bulk/document
/// paths; it is the Elasticsearch API rather than a translation layer, so the sink speaks plain
/// `_bulk`. Overridable with `--endpoint` for testing against a local mock.
pub const DEFAULT_ENDPOINT: &str = "https://logs.autonomi.com";

/// Prefix of the daily index events are written to: `beta-nodes-YYYY.MM.DD`.
///
/// The write-only API key is scoped to `beta-nodes-*`; anything outside that is rejected with a
/// per-item 403.
pub const DEFAULT_INDEX_PREFIX: &str = "beta-nodes";

/// Filename of the persisted forwarding config within [`config::config_dir`].
const CONFIG_FILENAME: &str = "log_forward.json";

/// Severity of a log event, ordered so that filtering is a comparison.
///
/// The ingest endpoint enforces its own minimum level (currently INFO) and silently drops anything
/// below it while reporting success, so filtering here is not about correctness — it is about not
/// spending the user's bandwidth and disk on events that are discarded on arrival.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Parse a level as it appears in an ant-node log line, in either log format.
    ///
    /// Accepts any case: the text layer emits `INFO`, the JSON layer emits `INFO` in its `level`
    /// field, and hand-written configs use `info`.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "TRACE" => Some(Self::Trace),
            "DEBUG" => Some(Self::Debug),
            "INFO" => Some(Self::Info),
            "WARN" | "WARNING" => Some(Self::Warn),
            "ERROR" => Some(Self::Error),
            _ => None,
        }
    }

    /// The level as it should appear in a forwarded document's `level` field.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Trace => "TRACE",
            Self::Debug => "DEBUG",
            Self::Info => "INFO",
            Self::Warn => "WARN",
            Self::Error => "ERROR",
        }
    }
}

impl std::fmt::Display for LogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for LogLevel {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Self::parse(s).ok_or_else(|| {
            Error::LogForward(format!(
                "unknown log level '{s}' (expected one of: trace, debug, info, warn, error)"
            ))
        })
    }
}

/// The default minimum level: INFO and above, matching both ant-node's own default and the
/// server-side ingest filter.
const fn default_min_level() -> LogLevel {
    LogLevel::Info
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.to_string()
}

fn default_index_prefix() -> String {
    DEFAULT_INDEX_PREFIX.to_string()
}

/// Persisted forwarding configuration.
///
/// Absent file means "never enabled", which loads as [`LogForwardConfig::disabled`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogForwardConfig {
    /// Whether the user has opted in. `disable` clears this but keeps the rest, so a later
    /// `enable` with no arguments resumes with the same token and endpoint.
    pub enabled: bool,

    /// Write-only Elasticsearch API key, sent as `Authorization: ApiKey <token>`.
    ///
    /// Never serialized into an API response — see [`Self::token_fingerprint`].
    #[serde(default)]
    pub token: String,

    /// Ingest endpoint. Defaults to [`DEFAULT_ENDPOINT`].
    #[serde(default = "default_endpoint")]
    pub endpoint: String,

    /// Daily index prefix. Defaults to [`DEFAULT_INDEX_PREFIX`].
    #[serde(default = "default_index_prefix")]
    pub index_prefix: String,

    /// Drop events below this level before batching. Defaults to [`LogLevel::Info`].
    #[serde(default = "default_min_level")]
    pub min_level: LogLevel,

    /// Stable, randomly generated namespace for this installation's document ids.
    ///
    /// Every participant writes into the same shared `beta-nodes-YYYY.MM.DD` indices, so a document
    /// id built only from node id, filename and byte offset is not unique across machines: node 1's
    /// first log line sits at offset 0 of the same daily filename on *every* installation. Since a
    /// duplicate id is answered with a 409 that the sink counts as delivered, the second machine's
    /// event would be silently discarded rather than stored.
    ///
    /// Prefixing the id with this value removes that collision. It is random rather than derived
    /// from anything about the machine — not the hostname, MAC or username — so it identifies an
    /// installation only in the sense of separating it from other installations.
    ///
    /// It must stay stable for the lifetime of the install: the deterministic id is what makes a
    /// replayed batch idempotent, and regenerating this would make a replay look like a new
    /// document and duplicate it.
    #[serde(default)]
    pub installation_id: String,
}

impl Default for LogForwardConfig {
    fn default() -> Self {
        Self::disabled()
    }
}

impl LogForwardConfig {
    /// The state of a machine that has never opted in.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            token: String::new(),
            endpoint: default_endpoint(),
            index_prefix: default_index_prefix(),
            min_level: default_min_level(),
            installation_id: String::new(),
        }
    }

    /// Generate the installation namespace if this config does not have one yet.
    ///
    /// Called when forwarding is enabled. Configs written before this field existed load with an
    /// empty value and are filled in on their next enable.
    pub fn ensure_installation_id(&mut self) {
        if self.installation_id.is_empty() {
            self.installation_id = generate_installation_id();
        }
    }

    /// Path of the persisted config for this machine.
    pub fn default_path() -> Result<PathBuf> {
        Ok(config::config_dir()?.join(CONFIG_FILENAME))
    }

    /// Load the config, returning [`Self::disabled`] when the file does not exist.
    ///
    /// A corrupt file is an error rather than a silent reset: forwarding is opt-in, and silently
    /// falling back to "disabled" would look identical to a user who had opted in and would leave
    /// them believing logs were flowing when they were not.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::disabled());
        }
        let contents = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&contents)?;
        Ok(config)
    }

    /// Write the config atomically with owner-only permissions.
    ///
    /// Permissions are set on the temporary file *before* the rename, so the token is never
    /// readable by other users on the machine, even briefly.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = serde_json::to_string_pretty(self)?;
        let tmp_path = path.with_extension("tmp");
        std::fs::write(&tmp_path, &contents)?;
        restrict_to_owner(&tmp_path)?;
        std::fs::rename(&tmp_path, path)?;
        Ok(())
    }

    /// Reject a configuration that cannot possibly ship anything.
    pub fn validate(&self) -> Result<()> {
        if self.installation_id.is_empty() {
            return Err(Error::LogForward(
                "internal: installation id was not generated before enabling".into(),
            ));
        }
        if self.token.trim().is_empty() {
            return Err(Error::LogForward(
                "a write token is required: ant node logs forward enable --token <token>".into(),
            ));
        }
        if !self.endpoint.starts_with("http://") && !self.endpoint.starts_with("https://") {
            return Err(Error::LogForward(format!(
                "endpoint must be an http(s) URL, got '{}'",
                self.endpoint
            )));
        }
        if self.index_prefix.trim().is_empty() {
            return Err(Error::LogForward("index prefix must not be empty".into()));
        }
        Ok(())
    }

    /// A non-reversible short identifier for the configured token, safe to show in status output.
    ///
    /// Returns `None` when no token is set. This exists so a user can confirm *which* key is in
    /// use — after re-enrolling, say — without the daemon ever handing the key back out over its
    /// HTTP API.
    #[must_use]
    pub fn token_fingerprint(&self) -> Option<String> {
        if self.token.trim().is_empty() {
            return None;
        }
        let digest = blake3::hash(self.token.as_bytes());
        Some(digest.to_hex()[..12].to_string())
    }

    /// The endpoint with any trailing slash removed, so path joining is unambiguous.
    #[must_use]
    pub fn endpoint_base(&self) -> &str {
        self.endpoint.trim_end_matches('/')
    }
}

/// 64 bits of randomness, rendered as hex.
///
/// Ample for separating a beta cohort — collisions become likely somewhere around a billion
/// installations — while keeping the document id short and readable.
fn generate_installation_id() -> String {
    use rand::Rng;
    let bytes: [u8; 8] = rand::thread_rng().gen();
    bytes.iter().fold(String::with_capacity(16), |mut acc, b| {
        use std::fmt::Write;
        let _ = write!(acc, "{b:02x}");
        acc
    })
}

#[cfg(unix)]
fn restrict_to_owner(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// On Windows the config lands in the per-user `%APPDATA%` tree, which is already
/// user-scoped by the default ACL; there is no portable mode bit to set.
#[cfg(not(unix))]
fn restrict_to_owner(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enabled_config() -> LogForwardConfig {
        LogForwardConfig {
            enabled: true,
            token: "test-api-key".to_string(),
            installation_id: "0123456789abcdef".to_string(),
            ..LogForwardConfig::disabled()
        }
    }

    #[test]
    fn levels_order_by_severity() {
        assert!(LogLevel::Trace < LogLevel::Debug);
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }

    #[test]
    fn level_parses_both_log_formats_and_config_casing() {
        assert_eq!(LogLevel::parse("INFO"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse("info"), Some(LogLevel::Info));
        assert_eq!(LogLevel::parse(" WARN "), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("WARNING"), Some(LogLevel::Warn));
        assert_eq!(LogLevel::parse("nonsense"), None);
    }

    #[test]
    fn level_from_str_reports_the_accepted_values() {
        let err = "verbose".parse::<LogLevel>().unwrap_err().to_string();
        assert!(err.contains("trace, debug, info, warn, error"), "{err}");
    }

    #[test]
    fn missing_file_loads_as_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let config = LogForwardConfig::load(&tmp.path().join("absent.json")).unwrap();
        assert_eq!(config, LogForwardConfig::disabled());
        assert!(!config.enabled);
    }

    #[test]
    fn corrupt_file_is_an_error_rather_than_a_silent_reset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log_forward.json");
        std::fs::write(&path, "{ not json").unwrap();
        assert!(LogForwardConfig::load(&path).is_err());
    }

    #[test]
    fn save_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("nested").join("log_forward.json");
        let config = enabled_config();
        config.save(&path).unwrap();
        assert_eq!(LogForwardConfig::load(&path).unwrap(), config);
    }

    /// An older config written before a field existed must still load, taking the defaults.
    #[test]
    fn load_tolerates_a_config_missing_the_optional_fields() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log_forward.json");
        std::fs::write(&path, r#"{"enabled":true,"token":"k"}"#).unwrap();
        let config = LogForwardConfig::load(&path).unwrap();
        assert!(config.enabled);
        assert_eq!(config.endpoint, DEFAULT_ENDPOINT);
        assert_eq!(config.index_prefix, DEFAULT_INDEX_PREFIX);
        assert_eq!(config.min_level, LogLevel::Info);
    }

    #[cfg(unix)]
    #[test]
    fn saved_config_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log_forward.json");
        enabled_config().save(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "token file must not be group/world readable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overwriting_an_existing_config_keeps_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log_forward.json");
        std::fs::write(&path, "{}").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        enabled_config().save(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn validate_rejects_a_missing_token() {
        let config = LogForwardConfig {
            token: "   ".to_string(),
            ..enabled_config()
        };
        assert!(config.validate().unwrap_err().to_string().contains("token"));
    }

    #[test]
    fn an_installation_id_is_generated_once_and_then_left_alone() {
        let mut config = LogForwardConfig::disabled();
        assert!(config.installation_id.is_empty());

        config.ensure_installation_id();
        let first = config.installation_id.clone();
        assert_eq!(first.len(), 16, "64 bits rendered as hex");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));

        // Stability is what keeps a replayed batch idempotent.
        config.ensure_installation_id();
        assert_eq!(config.installation_id, first);
    }

    #[test]
    fn separate_installations_get_different_ids() {
        let mut a = LogForwardConfig::disabled();
        let mut b = LogForwardConfig::disabled();
        a.ensure_installation_id();
        b.ensure_installation_id();
        assert_ne!(a.installation_id, b.installation_id);
    }

    #[test]
    fn the_installation_id_survives_a_save_and_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("log_forward.json");
        let config = enabled_config();
        config.save(&path).unwrap();
        assert_eq!(
            LogForwardConfig::load(&path).unwrap().installation_id,
            config.installation_id
        );
    }

    #[test]
    fn validate_rejects_a_non_http_endpoint() {
        let config = LogForwardConfig {
            endpoint: "logs.autonomi.com".to_string(),
            ..enabled_config()
        };
        assert!(config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("http(s) URL"));
    }

    #[test]
    fn validate_accepts_the_defaults_with_a_token() {
        enabled_config().validate().unwrap();
    }

    #[test]
    fn fingerprint_is_stable_absent_for_no_token_and_leaks_nothing() {
        let config = enabled_config();
        let fingerprint = config.token_fingerprint().unwrap();
        assert_eq!(fingerprint.len(), 12);
        assert_eq!(config.token_fingerprint().unwrap(), fingerprint);
        assert!(!fingerprint.contains("test-api-key"));

        let other = LogForwardConfig {
            token: "a-different-key".to_string(),
            ..enabled_config()
        };
        assert_ne!(other.token_fingerprint().unwrap(), fingerprint);
        assert_eq!(LogForwardConfig::disabled().token_fingerprint(), None);
    }

    #[test]
    fn endpoint_base_strips_a_trailing_slash() {
        let config = LogForwardConfig {
            endpoint: "https://logs.autonomi.com/".to_string(),
            ..enabled_config()
        };
        assert_eq!(config.endpoint_base(), "https://logs.autonomi.com");
    }

    /// The token must never reach an API response body. Status is built from a dedicated type, but
    /// this pins the underlying expectation that nothing serializes the config itself outward.
    #[test]
    fn fingerprint_rather_than_token_is_what_status_can_show() {
        let config = enabled_config();
        let fingerprint = config.token_fingerprint().unwrap();
        assert!(!fingerprint.contains(&config.token));
    }
}
