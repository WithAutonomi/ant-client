//! Turning a line of an ant-node log file into a forwardable event.
//!
//! A node writes one of two layouts depending on `--log-format`, and the daemon does not set that
//! flag, so a user may have chosen either. Rather than force a format — which would mean adding an
//! argument to every node's command line and restarting them all just to switch forwarding on —
//! this module detects the layout per line:
//!
//! ```text
//! text: 2026-08-19T20:50:00.123456Z  INFO ant_node::node: connected peers=3
//! json: {"timestamp":"2026-08-19T20:50:00.123456Z","level":"INFO","target":"ant_node::node", …}
//! ```
//!
//! Lines that are neither — panic messages, backtrace frames, anything a dependency writes
//! straight to the file — are continuations of the event above them rather than events in their
//! own right, and are appended to it. That keeps a multi-line panic intact as one document instead
//! of scattering it across twenty timestamp-less ones.

use serde::{Deserialize, Serialize};

use super::config::LogLevel;

/// A single parsed log event, before it is tagged with the node's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogEvent {
    /// RFC3339 timestamp, forwarded as `@timestamp`.
    pub timestamp: String,
    pub level: LogLevel,
    /// Rust module path the event came from, when the layout carried one.
    pub target: Option<String>,
    /// The event text, including any continuation lines appended to it.
    pub message: String,
    /// Public protocol identifier, lifted opportunistically when the line happens to carry one.
    pub peer_id: Option<String>,
    /// Version ant-node reported for itself, present only on its startup line.
    pub version: Option<String>,
    /// Commit ant-node reported for itself, present only on its startup line.
    pub commit: Option<String>,
}

impl LogEvent {
    /// Append a continuation line to this event's message.
    pub fn push_continuation(&mut self, line: &str) {
        self.message.push('\n');
        self.message.push_str(line);
    }

    /// The daily index date for this event, as Elasticsearch wants it: `YYYY.MM.DD`.
    ///
    /// Derived from the event's own timestamp rather than the wall clock. That is not a stylistic
    /// choice: document `_id`s are unique per index, so a batch replayed after midnight must land
    /// in the same index its first attempt targeted or the deduplication silently stops working.
    #[must_use]
    pub fn index_date(&self) -> Option<String> {
        index_date_from_timestamp(&self.timestamp)
    }
}

/// Extract `YYYY.MM.DD` from an RFC3339 timestamp, validating the shape rather than trusting it.
#[must_use]
pub fn index_date_from_timestamp(timestamp: &str) -> Option<String> {
    let date = timestamp.get(..10)?;
    let bytes = date.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    if !bytes
        .iter()
        .enumerate()
        .all(|(i, b)| matches!(i, 4 | 7) || b.is_ascii_digit())
    {
        return None;
    }
    Some(format!("{}.{}.{}", &date[..4], &date[5..7], &date[8..10]))
}

/// Parse one line, returning `None` when it is a continuation of the event above it.
#[must_use]
pub fn parse_line(line: &str) -> Option<LogEvent> {
    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.trim().is_empty() {
        return None;
    }
    if trimmed.trim_start().starts_with('{') {
        parse_json_line(trimmed)
    } else {
        parse_text_line(trimmed)
    }
}

/// Parse the JSON layout produced by `fmt::layer().json().flatten_event(true)`.
///
/// `flatten_event` lifts the event's fields to the top level, so `message`, `peer_id`, `version`
/// and `commit` all sit beside `timestamp`, `level` and `target`.
fn parse_json_line(line: &str) -> Option<LogEvent> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let object = value.as_object()?;

    let level = LogLevel::parse(object.get("level")?.as_str()?)?;
    let timestamp = object.get("timestamp")?.as_str()?.to_string();
    // Held to the same standard as the text layout: an event whose timestamp cannot be read is an
    // event with no index to go to, so it is better treated as a continuation than shipped blind.
    index_date_from_timestamp(&timestamp)?;

    let message = object
        .get("message")
        .and_then(|m| {
            m.as_str()
                .map(str::to_string)
                .or_else(|| Some(m.to_string()))
        })
        .unwrap_or_default();

    Some(LogEvent {
        timestamp,
        level,
        target: object
            .get("target")
            .and_then(|t| t.as_str())
            .map(str::to_string),
        message,
        peer_id: json_string_field(object, "peer_id"),
        version: json_string_field(object, "version"),
        commit: json_string_field(object, "commit"),
    })
}

/// Read a field as a string whether it was logged as one or as a number/bool via `Display`.
fn json_string_field(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<String> {
    match object.get(key)? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// Parse the default text layout: timestamp, level, optional span scope, target, then the message.
///
/// The span-scope-and-target prefix is delimited from the message by `": "`, but so is any message
/// that happens to contain a colon. The prefix segments are therefore consumed only while they
/// still look like a span or module path — no whitespace, no stray punctuation — which is what
/// stops `connected to peer: 12D3Koo…` losing its first two words to a phantom target.
fn parse_text_line(line: &str) -> Option<LogEvent> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let timestamp = parts.next()?.to_string();
    // Cheapest available proof that this really is the start of an event rather than a stray line
    // that happens to begin with a word.
    index_date_from_timestamp(&timestamp)?;

    let rest = parts.next()?.trim_start();
    let (level_token, rest) = rest.split_once(char::is_whitespace)?;
    let level = LogLevel::parse(level_token)?;

    let (target, message) = split_target_and_message(rest.trim_start());

    Some(LogEvent {
        timestamp,
        level,
        target,
        peer_id: scan_field(line, "peer_id"),
        version: scan_field(line, "version"),
        commit: scan_field(line, "commit"),
        message,
    })
}

fn split_target_and_message(rest: &str) -> (Option<String>, String) {
    let mut remaining = rest;
    let mut target = None;

    while let Some(index) = remaining.find(": ") {
        let head = &remaining[..index];
        if !looks_like_span_or_target(head) {
            break;
        }
        if looks_like_target(head) {
            target = Some(head.to_string());
        }
        remaining = &remaining[index + 2..];
    }

    (target, remaining.to_string())
}

/// A span scope (`upload{id=1}`) or a module path — never a sentence.
fn looks_like_span_or_target(candidate: &str) -> bool {
    !candidate.is_empty() && !candidate.contains(char::is_whitespace)
}

/// A bare Rust module path, which is what the target always is.
fn looks_like_target(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
}

/// Find a `key=value` field in a text-format line.
///
/// Best-effort by design: the text layout does not delimit the message from the trailing fields, so
/// there is no way to do this exactly, and it is not worth a real parser. A false negative costs an
/// absent field on one document.
fn scan_field(line: &str, key: &str) -> Option<String> {
    let needle = format!("{key}=");
    let mut search_from = 0;

    while let Some(offset) = line[search_from..].find(&needle) {
        let start = search_from + offset;
        let preceded_by_boundary = start == 0
            || line[..start]
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);

        if preceded_by_boundary {
            let value = &line[start + needle.len()..];
            let value = value
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .trim_end_matches(',')
                .trim_matches('"');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        search_from = start + needle.len();
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEXT_LINE: &str =
        "2026-08-19T20:50:00.123456Z  INFO ant_node::node: connected to the network peers=3";

    #[test]
    fn parses_the_text_layout() {
        let event = parse_line(TEXT_LINE).unwrap();
        assert_eq!(event.timestamp, "2026-08-19T20:50:00.123456Z");
        assert_eq!(event.level, LogLevel::Info);
        assert_eq!(event.target.as_deref(), Some("ant_node::node"));
        assert_eq!(event.message, "connected to the network peers=3");
    }

    #[test]
    fn parses_the_json_layout() {
        let line = r#"{"timestamp":"2026-08-19T20:50:00.123456Z","level":"WARN","target":"ant_node::node","message":"peer unreachable","peer_id":"12D3KooWabc"}"#;
        let event = parse_line(line).unwrap();
        assert_eq!(event.timestamp, "2026-08-19T20:50:00.123456Z");
        assert_eq!(event.level, LogLevel::Warn);
        assert_eq!(event.target.as_deref(), Some("ant_node::node"));
        assert_eq!(event.message, "peer unreachable");
        assert_eq!(event.peer_id.as_deref(), Some("12D3KooWabc"));
    }

    #[test]
    fn every_level_round_trips_through_the_text_layout() {
        for (token, expected) in [
            ("TRACE", LogLevel::Trace),
            ("DEBUG", LogLevel::Debug),
            ("INFO", LogLevel::Info),
            ("WARN", LogLevel::Warn),
            ("ERROR", LogLevel::Error),
        ] {
            let line = format!("2026-08-19T20:50:00.123456Z {token} ant_node: hello");
            assert_eq!(parse_line(&line).unwrap().level, expected, "{token}");
        }
    }

    /// The text layer pads the level to five columns, so INFO and WARN arrive with two leading
    /// spaces where ERROR and TRACE arrive with one.
    #[test]
    fn tolerates_the_level_column_padding() {
        let padded = "2026-08-19T20:50:00.123456Z  INFO ant_node: hello";
        let unpadded = "2026-08-19T20:50:00.123456Z ERROR ant_node: hello";
        assert_eq!(parse_line(padded).unwrap().level, LogLevel::Info);
        assert_eq!(parse_line(unpadded).unwrap().level, LogLevel::Error);
    }

    /// The case the naive "split on the first colon" approach gets wrong.
    #[test]
    fn a_colon_in_the_message_does_not_become_a_target() {
        let line = "2026-08-19T20:50:00.123456Z  INFO ant_node: dialing peer: 12D3KooWabc";
        let event = parse_line(line).unwrap();
        assert_eq!(event.target.as_deref(), Some("ant_node"));
        assert_eq!(event.message, "dialing peer: 12D3KooWabc");
    }

    #[test]
    fn a_span_scope_before_the_target_is_skipped() {
        let line = "2026-08-19T20:50:00.123456Z  INFO upload{id=1}: ant_node::store: stored chunk";
        let event = parse_line(line).unwrap();
        assert_eq!(event.target.as_deref(), Some("ant_node::store"));
        assert_eq!(event.message, "stored chunk");
    }

    #[test]
    fn a_message_with_no_target_still_parses() {
        let line = "2026-08-19T20:50:00.123456Z  INFO started with no target at all";
        let event = parse_line(line).unwrap();
        assert_eq!(event.target, None);
        assert_eq!(event.message, "started with no target at all");
    }

    #[test]
    fn lines_without_a_timestamp_are_continuations() {
        assert!(parse_line("  at src/node.rs:42").is_none());
        assert!(parse_line("thread 'main' panicked").is_none());
        assert!(parse_line("").is_none());
        assert!(parse_line("   ").is_none());
    }

    #[test]
    fn a_line_with_an_unknown_level_is_treated_as_a_continuation() {
        let line = "2026-08-19T20:50:00.123456Z  NOISE ant_node: hello";
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn malformed_json_is_treated_as_a_continuation_rather_than_guessed_at() {
        assert!(parse_line(r#"{"level":"INFO""#).is_none());
        assert!(parse_line(r#"{"level":"INFO","target":"x"}"#).is_none());
    }

    /// An event with an unreadable timestamp has no index to be written to, in either layout.
    #[test]
    fn a_json_line_with_an_unusable_timestamp_is_rejected() {
        let line = r#"{"timestamp":"not-a-date","level":"INFO","message":"m"}"#;
        assert!(parse_line(line).is_none());
    }

    #[test]
    fn every_parsed_event_can_name_its_index() {
        for line in [
            TEXT_LINE,
            r#"{"timestamp":"2026-08-19T20:50:00.123456Z","level":"INFO","message":"m"}"#,
        ] {
            assert!(parse_line(line).unwrap().index_date().is_some(), "{line}");
        }
    }

    #[test]
    fn continuations_are_appended_to_the_event_above_them() {
        let mut event = parse_line(TEXT_LINE).unwrap();
        event.push_continuation("thread 'main' panicked");
        event.push_continuation("  at src/node.rs:42");
        assert_eq!(
            event.message,
            "connected to the network peers=3\nthread 'main' panicked\n  at src/node.rs:42"
        );
    }

    #[test]
    fn lifts_peer_id_version_and_commit_from_a_text_line() {
        let line = "2026-08-19T20:50:00.123456Z  INFO ant_node: starting version=0.17.2 commit=abc1234 peer_id=12D3KooWabc";
        let event = parse_line(line).unwrap();
        assert_eq!(event.version.as_deref(), Some("0.17.2"));
        assert_eq!(event.commit.as_deref(), Some("abc1234"));
        assert_eq!(event.peer_id.as_deref(), Some("12D3KooWabc"));
    }

    #[test]
    fn field_scanning_ignores_a_key_that_is_only_a_suffix_of_another() {
        let line = "2026-08-19T20:50:00.123456Z  INFO ant_node: hello node_version=9.9.9";
        assert_eq!(parse_line(line).unwrap().version, None);
    }

    #[test]
    fn field_scanning_strips_quotes_and_trailing_commas() {
        let line = r#"2026-08-19T20:50:00.123456Z  INFO ant_node: hello peer_id="12D3KooWabc","#;
        assert_eq!(
            parse_line(line).unwrap().peer_id.as_deref(),
            Some("12D3KooWabc")
        );
    }

    #[test]
    fn a_json_field_logged_as_a_number_still_reads_as_a_string() {
        let line = r#"{"timestamp":"2026-08-19T20:50:00.123456Z","level":"INFO","message":"m","peer_id":42}"#;
        assert_eq!(parse_line(line).unwrap().peer_id.as_deref(), Some("42"));
    }

    #[test]
    fn index_date_is_derived_from_the_events_own_timestamp() {
        let event = parse_line(TEXT_LINE).unwrap();
        assert_eq!(event.index_date().as_deref(), Some("2026.08.19"));
    }

    #[test]
    fn index_date_rejects_a_timestamp_it_cannot_trust() {
        assert_eq!(index_date_from_timestamp("nonsense"), None);
        assert_eq!(index_date_from_timestamp("2026/08/19T00:00:00Z"), None);
        assert_eq!(index_date_from_timestamp("20xx-08-19T00:00:00Z"), None);
        assert_eq!(index_date_from_timestamp("2026-08"), None);
    }

    #[test]
    fn trailing_newlines_are_stripped_from_the_message() {
        let event = parse_line(&format!("{TEXT_LINE}\r\n")).unwrap();
        assert_eq!(event.message, "connected to the network peers=3");
    }
}
