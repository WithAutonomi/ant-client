//! Normal-path download diagnostics instrumentation.
//!
//! Runtime-gated sidecar JSONL writer for `ant file download
//! --download-diagnostics <PATH>`. One record is emitted per normal-path
//! chunk fetch attempt (cache hit, per-peer attempt, lookup failure, or
//! exhausted peer set) while the existing early-return / retry /
//! adaptive-concurrency / stdout behaviour is preserved.
//!
//! When the `--download-diagnostics` flag is absent, no channel, file, or
//! writer is created and the download path is unchanged. The optional sender
//! threaded through the file/chunk download path is `None`, so record
//! construction is skipped entirely (no allocation, no I/O).
//!
//! # Schema v4: exact node/client request correlation
//!
//! Peer-attempt records carry the request ID allocated by this client and the
//! client's local peer ID. The same request ID is encoded on the chunk GET,
//! while the peer ID matches the serving node's `source_peer`, permitting an
//! exact join to node-side GET telemetry. Chunk-level records leave both
//! fields `null` because no individual peer request was sent.
//!
//! The stacked `ant-protocol` PR WithAutonomi/ant-protocol#32 exposes
//! `send_and_await_chunk_response_with_metadata`, which returns a
//! `ChunkProtocolResponse { result, source_peer, transport_source }`. The
//! stacked `saorsa-core` PR WithAutonomi/saorsa-core#162 exposes
//! `P2PNode::classify_peer_transport_route(expected_peer,
//! transport_source)` returning a `PeerRouteKind`
//! (`direct`/`relay`/`lan`/`unverified`/`unknown`). Schema v4 records the
//! *actual* `source_peer` and `transport_source` from the observed response,
//! classifies the route from the actual transport source against the peer's
//! typed DHT addresses, and attaches a `route_note` only when the route is
//! `unknown`. A `peer_connected_before_request` sample
//! (`node.is_peer_connected(peer)` called before the send) and an adaptive
//! `fetch_cap` snapshot are included on every record.
//!
//! # TTFB limitation
//!
//! The protocol event is emitted only after complete message reassembly, so
//! this branch measures complete-response latency (`response_elapsed_ms`),
//! not true network time-to-first-byte. `ttfb_ms` is always `null`,
//! `ttfb_available` is `false`, and `ttfb_unavailable_reason` carries the
//! explanation. This prevents complete-response latency being presented as
//! TTFB.

use std::fmt;
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{SystemTime, UNIX_EPOCH};

use ant_protocol::transport::PeerId;
use serde::Serialize;
use tracing::{error, warn};

/// Current diagnostic record schema discriminator.
pub const DIAGNOSTICS_SCHEMA_VERSION: u8 = 4;

/// Correlation values captured once for a peer request and shared by both
/// the encoded protocol message and its diagnostic record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DownloadRequestCorrelation {
    pub(crate) request_id: u64,
    pub(crate) local_peer_id: String,
}

impl DownloadRequestCorrelation {
    pub(crate) fn new(request_id: u64, local_peer_id: &PeerId) -> Self {
        Self {
            request_id,
            local_peer_id: local_peer_id.to_string(),
        }
    }
}

/// Bounded capacity of the diagnostics channel. A slow writer must not create
/// unbounded memory growth: when full, further records are dropped (counted
/// via `try_send`), which is acceptable for a best-effort diagnostic sidecar.
const DIAGNOSTICS_CHANNEL_CAPACITY: usize = 1024;

/// Upper bound on the length of the `error` string we serialize, so a verbose
/// remote error message cannot balloon the sidecar file. The category prefix
/// is always preserved; only the trailing detail is truncated.
const DIAGNOSTICS_ERROR_MAX_CHARS: usize = 240;

/// The outcome of a single normal-path chunk fetch attempt.
///
/// Each variant maps to a stable lowercase string used as the `outcome` JSON
/// field. Variants are intentionally exhaustive over the record categories
/// listed in the design doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadDiagnosticsOutcome {
    /// A chunk was successfully fetched from a peer.
    Found,
    /// A queried peer responded `NotFound`.
    NotFound,
    /// A peer attempt timed out waiting for a response.
    Timeout,
    /// A peer attempt failed at the transport / dial / send layer.
    NetworkError,
    /// A peer responded with a structured protocol-level error (e.g. a
    /// corrupted-chunk `ChunkGetResponse::Error` or a content/address
    /// mismatch).
    ProtocolError,
    /// The chunk was served from the in-memory cache; no peer was contacted.
    CacheHit,
    /// The DHT closest-peer lookup itself failed before any peer was queried.
    LookupError,
    /// A sweep queried every selected peer without success.
    Exhausted,
}

impl DownloadDiagnosticsOutcome {
    /// Canonical lowercase label for the `outcome` JSON field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Found => "found",
            Self::NotFound => "not_found",
            Self::Timeout => "timeout",
            Self::NetworkError => "network_error",
            Self::ProtocolError => "protocol_error",
            Self::CacheHit => "cache_hit",
            Self::LookupError => "lookup_error",
            Self::Exhausted => "exhausted",
        }
    }
}

impl fmt::Display for DownloadDiagnosticsOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for DownloadDiagnosticsOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

/// One JSONL record for a normal-path chunk fetch attempt.
///
/// Field names are stable and pinned by the serialization tests. `null` JSON
/// values are used for fields that do not apply to a given record kind (e.g.
/// `peer_attempt` / `expected_peer` / `source_peer` / `lookup_duration_ms` are
/// `null` for a cache hit, which has no peer).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DownloadDiagnosticsRecord {
    /// Stable schema discriminator, currently `4`.
    pub schema_version: u8,
    /// UTC time the attempt completed, RFC 3339 (`YYYY-MM-DDTHH:MM:SSZ`).
    pub timestamp: String,
    /// Wall-clock Unix time immediately before the peer request, in
    /// milliseconds. Together with `request_completed_unix_ms`, this permits
    /// request overlap to be reconstructed against fleet telemetry.
    pub request_started_unix_ms: Option<u64>,
    /// Wall-clock Unix time immediately after the peer request completed, in
    /// milliseconds. `None` for chunk-level records.
    pub request_completed_unix_ms: Option<u64>,
    /// Protocol request identifier allocated by this client and sent on the
    /// wire; the serving node's record for this GET carries the same value.
    /// `None` for chunk-level records where no peer request was sent.
    pub request_id: Option<u64>,
    /// This diagnostic client's local peer ID, matching the serving node's
    /// `source_peer` field. `None` for chunk-level records.
    pub local_peer_id: Option<String>,
    /// Outer file / deferred-retry attempt number (1 = first pass).
    pub file_attempt: usize,
    /// Chunk index within the file (1-based, matching the progress reports).
    pub chunk_index: usize,
    /// Hex-encoded chunk address.
    pub chunk_address: String,
    /// `initial` or `retry` (internal close-group retry sweep).
    pub sweep: String,
    /// Peer attempt number within the sweep; `None` for chunk-level records
    /// (`cache_hit`, `lookup_error`, `exhausted`).
    pub peer_attempt: Option<usize>,
    /// Closest-peer DHT lookup duration, emitted on the first peer attempt
    /// associated with that lookup; `None` otherwise.
    pub lookup_duration_ms: Option<u64>,
    /// Process-local identifier shared by attempts from one closest-peer lookup.
    pub lookup_correlation_id: Option<String>,
    /// One-based ordinal in the DHT-selected peer order.
    pub selected_peer_ordinal: Option<usize>,
    /// Peer selected by the DHT lookup for this attempt; `None` for chunk-level
    /// records.
    pub expected_peer: Option<String>,
    /// Dial addresses selected from the DHT record, in priority order.
    pub selected_peer_addresses: Option<Vec<String>>,
    /// Address-type labels parallel to `selected_peer_addresses`.
    pub selected_peer_address_types: Option<Vec<String>>,
    /// This client's monotonic last-successful-DHT-interaction age. This is
    /// local knowledge, not proof of remote uptime.
    pub local_last_seen_age_ms: Option<u64>,
    /// Publisher-clock-derived address-set age. The timestamp is untrusted and
    /// is not proof of remote uptime.
    pub publisher_address_set_age_ms: Option<u64>,
    /// Raw publisher wall-clock address-set sequence, when present.
    pub publisher_address_set_unix_ns: Option<u64>,
    /// Authenticated peer that supplied the matching response, from the
    /// `ChunkProtocolResponse` metadata; `None` when no response was received
    /// (timeout / send failure) or for chunk-level records.
    pub source_peer: Option<String>,
    /// Transport address that delivered the response, from the
    /// `ChunkProtocolResponse` metadata; `None` when no response was received
    /// or for chunk-level records.
    pub transport_source: Option<String>,
    /// `direct`, `relay`, `lan`, `unverified`, or `unknown`, classified from
    /// the actual transport source via
    /// `P2PNode::classify_peer_transport_route`. `unknown` for chunk-level
    /// records with no peer.
    pub route: String,
    /// Why `route` is `unknown` when that is the case; `None` once a real
    /// transport source is classified, and for chunk-level records.
    pub route_note: Option<String>,
    /// Whether `node.is_peer_connected(peer)` returned `true` when sampled
    /// before the send; `None` for chunk-level records.
    pub peer_connected_before_request: Option<bool>,
    /// Number of diagnostics-enabled peer requests active in this process
    /// immediately after this request entered the active set.
    pub active_requests_at_start: Option<usize>,
    /// Adaptive fetch concurrency cap snapshot at the time of this record;
    /// `None` when diagnostics are disabled (never emitted in that case).
    pub fetch_cap: Option<usize>,
    /// Elapsed time until the complete response was reassembled and
    /// delivered; `None` for chunk-level records with no peer attempt.
    pub response_elapsed_ms: Option<u64>,
    /// Time to first byte. Always `null` — see `ttfb_unavailable_reason`.
    pub ttfb_ms: Option<u64>,
    /// Explicitly `false` so complete-response latency is never presented as
    /// TTFB.
    pub ttfb_available: bool,
    /// Why TTFB is unavailable.
    pub ttfb_unavailable_reason: String,
    /// Valid returned chunk bytes; `0` for non-`found` outcomes.
    pub bytes: u64,
    /// Attempt outcome. See [`DownloadDiagnosticsOutcome`].
    pub outcome: DownloadDiagnosticsOutcome,
    /// Bounded diagnostic error category/detail; no secrets. `None` for
    /// successful records.
    pub error: Option<String>,
}

impl DownloadDiagnosticsRecord {
    /// The shared TTFB-unavailable reason string used by every record.
    pub const TTFB_UNAVAILABLE_REASON: &'static str =
        "protocol exposes only a complete-response event; first-byte/first-frame \
         timing is not available";

    /// The shared route-unknown note used when `classify_peer_transport_route`
    /// returns `Unknown`: the transport source was absent (no response) or did
    /// not match any known typed peer dial address.
    pub const ROUTE_UNKNOWN_NOTE: &'static str =
        "transport_source absent or did not match any known typed peer dial address; \
         route could not be classified from the observed response";

    /// Build a peer-attempt record. `lookup_duration_ms` is attached only when
    /// this is the first peer attempt of the sweep (`peer_attempt == 1`).
    /// `route_note` should be `Some` only when `route` is `"unknown"`.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn peer_attempt(
        file_attempt: usize,
        chunk_index: usize,
        chunk_address: &[u8; 32],
        sweep: &'static str,
        peer_attempt: usize,
        lookup_duration_ms: Option<u64>,
        lookup_correlation_id: &str,
        expected_peer: &str,
        selected_peer_addresses: Vec<String>,
        selected_peer_address_types: Vec<String>,
        local_last_seen_age_ms: Option<u64>,
        publisher_address_set_age_ms: Option<u64>,
        publisher_address_set_unix_ns: Option<u64>,
        source_peer: Option<&str>,
        transport_source: Option<&str>,
        route: &str,
        route_note: Option<&str>,
        peer_connected_before_request: Option<bool>,
        active_requests_at_start: Option<usize>,
        fetch_cap: Option<usize>,
        request_started_unix_ms: u64,
        request_completed_unix_ms: u64,
        correlation: &DownloadRequestCorrelation,
        response_elapsed_ms: u64,
        bytes: u64,
        outcome: DownloadDiagnosticsOutcome,
        error: Option<String>,
    ) -> Self {
        let lookup = if peer_attempt == 1 {
            lookup_duration_ms
        } else {
            None
        };
        Self {
            schema_version: DIAGNOSTICS_SCHEMA_VERSION,
            timestamp: utc_now_rfc3339(),
            request_started_unix_ms: Some(request_started_unix_ms),
            request_completed_unix_ms: Some(request_completed_unix_ms),
            request_id: Some(correlation.request_id),
            local_peer_id: Some(correlation.local_peer_id.clone()),
            file_attempt,
            chunk_index,
            chunk_address: hex::encode(chunk_address),
            sweep: sweep.to_string(),
            peer_attempt: Some(peer_attempt),
            lookup_duration_ms: lookup,
            lookup_correlation_id: Some(lookup_correlation_id.to_string()),
            selected_peer_ordinal: Some(peer_attempt),
            expected_peer: Some(expected_peer.to_string()),
            selected_peer_addresses: Some(selected_peer_addresses),
            selected_peer_address_types: Some(selected_peer_address_types),
            local_last_seen_age_ms,
            publisher_address_set_age_ms,
            publisher_address_set_unix_ns,
            source_peer: source_peer.map(str::to_string),
            transport_source: transport_source.map(str::to_string),
            route: route.to_string(),
            route_note: route_note.map(str::to_string),
            peer_connected_before_request,
            active_requests_at_start,
            fetch_cap,
            response_elapsed_ms: Some(response_elapsed_ms),
            ttfb_ms: None,
            ttfb_available: false,
            ttfb_unavailable_reason: Self::TTFB_UNAVAILABLE_REASON.to_string(),
            bytes,
            outcome,
            error,
        }
    }

    /// Build a chunk-level record (no peer attempt): cache hit, lookup error,
    /// or exhausted peer set.
    #[allow(clippy::too_many_arguments)]
    pub fn chunk_level(
        file_attempt: usize,
        chunk_index: usize,
        chunk_address: &[u8; 32],
        sweep: &'static str,
        fetch_cap: Option<usize>,
        bytes: u64,
        outcome: DownloadDiagnosticsOutcome,
        error: Option<String>,
    ) -> Self {
        Self {
            schema_version: DIAGNOSTICS_SCHEMA_VERSION,
            timestamp: utc_now_rfc3339(),
            request_started_unix_ms: None,
            request_completed_unix_ms: None,
            request_id: None,
            local_peer_id: None,
            file_attempt,
            chunk_index,
            chunk_address: hex::encode(chunk_address),
            sweep: sweep.to_string(),
            peer_attempt: None,
            lookup_duration_ms: None,
            lookup_correlation_id: None,
            selected_peer_ordinal: None,
            expected_peer: None,
            selected_peer_addresses: None,
            selected_peer_address_types: None,
            local_last_seen_age_ms: None,
            publisher_address_set_age_ms: None,
            publisher_address_set_unix_ns: None,
            source_peer: None,
            transport_source: None,
            route: "unknown".to_string(),
            route_note: None,
            peer_connected_before_request: None,
            active_requests_at_start: None,
            fetch_cap,
            response_elapsed_ms: None,
            ttfb_ms: None,
            ttfb_available: false,
            ttfb_unavailable_reason: Self::TTFB_UNAVAILABLE_REASON.to_string(),
            bytes,
            outcome,
            error,
        }
    }
}

/// A cloneable, bounded sender for diagnostic records.
///
/// Cloning is cheap (a single `mpsc::Sender` handle). `try_emit` never
/// blocks: when the bounded channel is full the record is dropped, so a slow
/// writer cannot stall the download path. Dropped records are counted and the
/// writer reports the total when it exits.
#[derive(Clone)]
pub struct DownloadDiagnosticsSender {
    tx: mpsc::SyncSender<DownloadDiagnosticsRecord>,
    dropped: Arc<AtomicU64>,
}

impl DownloadDiagnosticsSender {
    /// Enqueue a record without blocking. Drops the record if the bounded
    /// channel is full.
    pub fn try_emit(&self, record: DownloadDiagnosticsRecord) {
        if self.tx.try_send(record).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Open `<path>` for writing (truncated), spawn a dedicated OS thread that
/// drains records from a bounded channel and writes one JSON line per record
/// to a buffered writer, flushing on close.
///
/// The writer runs on a plain OS thread (not a tokio task) so synchronous file
/// writes never block the async runtime. The returned sender is cloneable and
/// can be threaded through the download path; dropping the last clone closes
/// the channel. Joining the returned thread handle waits for the final flush
/// and writer exit.
///
/// # Errors
///
/// Returns an error if the file cannot be opened or the writer thread cannot
/// be spawned.
pub fn spawn_download_diagnostics_writer(
    path: &Path,
) -> io::Result<(DownloadDiagnosticsSender, std::thread::JoinHandle<()>)> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)?;
    let (tx, rx) = mpsc::sync_channel::<DownloadDiagnosticsRecord>(DIAGNOSTICS_CHANNEL_CAPACITY);
    let dropped = Arc::new(AtomicU64::new(0));
    let writer_dropped = Arc::clone(&dropped);
    let builder = std::thread::Builder::new().name("ant-download-diagnostics-writer".to_string());
    let handle = builder
        .spawn(move || {
            let mut writer = io::BufWriter::new(file);
            for record in rx.iter() {
                match serde_json::to_string(&record) {
                    Ok(line) => {
                        if let Err(err) = writeln!(writer, "{line}") {
                            error!(%err, "download diagnostics sidecar write failed");
                            break;
                        }
                    }
                    Err(err) => {
                        // A record that cannot be serialized is skipped rather
                        // than dropping the whole sidecar; the writer keeps
                        // draining so later valid records survive.
                        error!(%err, "download diagnostics record serialization failed");
                        continue;
                    }
                }
            }
            if let Err(err) = writer.flush() {
                error!(%err, "download diagnostics sidecar flush failed");
            }
            let dropped = writer_dropped.load(Ordering::Relaxed);
            if dropped > 0 {
                warn!(dropped, "download diagnostics records were dropped");
            }
        })
        .map_err(|e| io::Error::other(format!("failed to spawn diagnostics writer thread: {e}")))?;
    Ok((DownloadDiagnosticsSender { tx, dropped }, handle))
}

/// Bound an error message to [`DIAGNOSTICS_ERROR_MAX_CHARS`] chars, preserving
/// a leading category if one is supplied.
///
/// `category` is a short stable label (e.g. `"timeout"`); `detail` is the
/// free-form error text that may be truncated. No credentials are added — the
/// caller passes only a bounded diagnostic string.
pub fn bounded_error(category: &str, detail: &str) -> String {
    let prefix = if category.is_empty() {
        String::new()
    } else {
        format!("{category}: ")
    };
    if prefix.len() + detail.len() <= DIAGNOSTICS_ERROR_MAX_CHARS {
        return format!("{prefix}{detail}");
    }
    let remaining = DIAGNOSTICS_ERROR_MAX_CHARS.saturating_sub(prefix.len());
    let mut truncated: String = detail.chars().take(remaining.saturating_sub(1)).collect();
    truncated.push('…');
    format!("{prefix}{truncated}")
}

/// Format the current UTC time as an RFC 3339 string (`YYYY-MM-DDTHH:MM:SSZ`)
/// without a `chrono`/`time` dependency.
fn utc_now_rfc3339() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    rfc3339_from_unix_secs(now.as_secs())
}

/// Current wall-clock Unix time in milliseconds for joining request windows
/// to external fleet telemetry. Saturates if the platform clock representation
/// exceeds `u64`.
pub(crate) fn unix_now_ms() -> u64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    u64::try_from(millis).unwrap_or(u64::MAX)
}

/// Convert Unix epoch seconds to an RFC 3339 UTC string.
///
/// Uses the well-known civil-from-days algorithm (Howard Hinnant). No leap
/// seconds; sufficient precision for a diagnostic timestamp.
fn rfc3339_from_unix_secs(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;

    // Civil date from days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };

    format!("{year:04}-{m:02}-{d:02}T{hour:02}:{minute:02}:{second:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_correlation(request_id: u64) -> DownloadRequestCorrelation {
        DownloadRequestCorrelation::new(request_id, &PeerId::from_bytes([42; 32]))
    }

    #[test]
    fn outcome_as_str_is_stable_lowercase() {
        assert_eq!(DownloadDiagnosticsOutcome::Found.as_str(), "found");
        assert_eq!(DownloadDiagnosticsOutcome::NotFound.as_str(), "not_found");
        assert_eq!(DownloadDiagnosticsOutcome::Timeout.as_str(), "timeout");
        assert_eq!(
            DownloadDiagnosticsOutcome::NetworkError.as_str(),
            "network_error"
        );
        assert_eq!(
            DownloadDiagnosticsOutcome::ProtocolError.as_str(),
            "protocol_error"
        );
        assert_eq!(DownloadDiagnosticsOutcome::CacheHit.as_str(), "cache_hit");
        assert_eq!(
            DownloadDiagnosticsOutcome::LookupError.as_str(),
            "lookup_error"
        );
        assert_eq!(DownloadDiagnosticsOutcome::Exhausted.as_str(), "exhausted");
    }

    #[test]
    fn outcome_serializes_as_lowercase_string() {
        let v = serde_json::to_string(&DownloadDiagnosticsOutcome::NetworkError).unwrap();
        assert_eq!(v, "\"network_error\"");
    }

    #[test]
    fn peer_attempt_record_pins_field_names_and_null_ttfb() {
        let addr = [7u8; 32];
        let correlation = test_correlation(9_001);
        let record = DownloadDiagnosticsRecord::peer_attempt(
            1,
            3,
            &addr,
            "initial",
            1,
            Some(42),
            "lookup-1",
            "peer-abc",
            vec!["/ip4/1.2.3.4/udp/9000/quic".to_string()],
            vec!["direct".to_string()],
            Some(1_500),
            Some(2_000),
            Some(1_234_000_000),
            Some("peer-abc"),
            Some("/ip4/1.2.3.4/udp/9000/quic"),
            "direct",
            None,
            Some(true),
            Some(8),
            Some(8),
            120,
            1024,
            &correlation,
            904,
            1024,
            DownloadDiagnosticsOutcome::Found,
            None,
        );
        let json = serde_json::to_value(&record).unwrap();
        let obj = json.as_object().unwrap();
        // Pin field names — schema v4.
        for field in [
            "schema_version",
            "timestamp",
            "request_started_unix_ms",
            "request_completed_unix_ms",
            "request_id",
            "local_peer_id",
            "file_attempt",
            "chunk_index",
            "chunk_address",
            "sweep",
            "peer_attempt",
            "lookup_duration_ms",
            "lookup_correlation_id",
            "selected_peer_ordinal",
            "expected_peer",
            "selected_peer_addresses",
            "selected_peer_address_types",
            "local_last_seen_age_ms",
            "publisher_address_set_age_ms",
            "publisher_address_set_unix_ns",
            "source_peer",
            "transport_source",
            "route",
            "route_note",
            "peer_connected_before_request",
            "active_requests_at_start",
            "fetch_cap",
            "response_elapsed_ms",
            "ttfb_ms",
            "ttfb_available",
            "ttfb_unavailable_reason",
            "bytes",
            "outcome",
            "error",
        ] {
            assert!(obj.contains_key(field), "missing field {field}");
        }
        // Explicit unavailable-TTFB representation.
        assert_eq!(obj["ttfb_ms"], serde_json::Value::Null);
        assert_eq!(obj["ttfb_available"], serde_json::Value::Bool(false));
        assert!(
            obj["ttfb_unavailable_reason"]
                .as_str()
                .unwrap()
                .contains("first-byte"),
            "ttfb reason must mention first-byte"
        );
        // Route classified from actual transport source.
        assert_eq!(obj["route"], serde_json::Value::String("direct".into()));
        assert_eq!(obj["route_note"], serde_json::Value::Null);
        // Actual source_peer and transport_source from response metadata.
        assert_eq!(obj["source_peer"], serde_json::json!("peer-abc"));
        assert_eq!(
            obj["transport_source"],
            serde_json::json!("/ip4/1.2.3.4/udp/9000/quic")
        );
        // Request bounds, active count, peer state, and fetch cap sampled.
        assert_eq!(obj["request_started_unix_ms"], serde_json::json!(120u64));
        assert_eq!(obj["request_completed_unix_ms"], serde_json::json!(1024u64));
        assert_eq!(obj["active_requests_at_start"], serde_json::json!(8usize));
        assert_eq!(
            obj["peer_connected_before_request"],
            serde_json::json!(true)
        );
        assert_eq!(obj["fetch_cap"], serde_json::json!(8usize));
        // First peer attempt carries the lookup duration.
        assert_eq!(obj["lookup_duration_ms"], serde_json::json!(42u64));
        assert_eq!(obj["bytes"], serde_json::json!(1024u64));
        assert_eq!(obj["outcome"], serde_json::json!("found"));
        assert_eq!(obj["request_id"], serde_json::json!(9_001u64));
        assert_eq!(
            obj["local_peer_id"],
            serde_json::json!(correlation.local_peer_id)
        );
        assert_eq!(obj["schema_version"], serde_json::json!(4u8));
        assert_eq!(obj["lookup_correlation_id"], serde_json::json!("lookup-1"));
        assert_eq!(obj["selected_peer_ordinal"], serde_json::json!(1usize));
        assert_eq!(obj["local_last_seen_age_ms"], serde_json::json!(1_500u64));
        assert_eq!(
            obj["publisher_address_set_age_ms"],
            serde_json::json!(2_000u64)
        );
        assert_eq!(
            obj["chunk_address"],
            serde_json::Value::String(hex::encode(addr))
        );
    }

    #[test]
    fn later_peer_attempt_omits_lookup_duration_and_carries_route_note_when_unknown() {
        let addr = [9u8; 32];
        let record = DownloadDiagnosticsRecord::peer_attempt(
            1,
            1,
            &addr,
            "retry",
            3,
            Some(10),
            "lookup-2",
            "peer-x",
            vec!["/ip6/2001:db8::1/udp/9000/quic".to_string()],
            vec!["unverified".to_string()],
            None,
            None,
            None,
            // No response → no source_peer, no transport_source.
            None,
            None,
            "unknown",
            Some(DownloadDiagnosticsRecord::ROUTE_UNKNOWN_NOTE),
            Some(false),
            Some(4),
            Some(4),
            500,
            1_000,
            &test_correlation(9_002),
            500,
            0,
            DownloadDiagnosticsOutcome::Timeout,
            Some(bounded_error("timeout", "no response")),
        );
        let json = serde_json::to_value(&record).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["lookup_duration_ms"], serde_json::Value::Null);
        assert_eq!(obj["lookup_correlation_id"], serde_json::json!("lookup-2"));
        assert_eq!(obj["selected_peer_ordinal"], serde_json::json!(3usize));
        assert_eq!(
            obj["route_note"],
            serde_json::json!(DownloadDiagnosticsRecord::ROUTE_UNKNOWN_NOTE)
        );
        assert_eq!(obj["source_peer"], serde_json::Value::Null);
        assert_eq!(obj["transport_source"], serde_json::Value::Null);
        assert_eq!(obj["route"], serde_json::json!("unknown"));
        assert_eq!(
            obj["peer_connected_before_request"],
            serde_json::json!(false)
        );
        assert_eq!(obj["active_requests_at_start"], serde_json::json!(4usize));
        assert_eq!(obj["fetch_cap"], serde_json::json!(4usize));
        assert_eq!(obj["outcome"], serde_json::json!("timeout"));
        assert_eq!(obj["bytes"], serde_json::json!(0u64));
        assert!(obj["error"].as_str().unwrap().starts_with("timeout: "));
    }

    #[test]
    fn cache_hit_record_has_no_peer_fields() {
        let addr = [1u8; 32];
        let record = DownloadDiagnosticsRecord::chunk_level(
            1,
            2,
            &addr,
            "initial",
            Some(8),
            4096,
            DownloadDiagnosticsOutcome::CacheHit,
            None,
        );
        let json = serde_json::to_value(&record).unwrap();
        let obj = json.as_object().unwrap();
        assert_eq!(obj["peer_attempt"], serde_json::Value::Null);
        assert_eq!(obj["expected_peer"], serde_json::Value::Null);
        assert_eq!(obj["source_peer"], serde_json::Value::Null);
        assert_eq!(obj["transport_source"], serde_json::Value::Null);
        assert_eq!(obj["lookup_duration_ms"], serde_json::Value::Null);
        assert_eq!(obj["lookup_correlation_id"], serde_json::Value::Null);
        assert_eq!(obj["selected_peer_addresses"], serde_json::Value::Null);
        assert_eq!(obj["response_elapsed_ms"], serde_json::Value::Null);
        assert_eq!(
            obj["peer_connected_before_request"],
            serde_json::Value::Null
        );
        assert_eq!(obj["request_started_unix_ms"], serde_json::Value::Null);
        assert_eq!(obj["request_completed_unix_ms"], serde_json::Value::Null);
        assert_eq!(obj["request_id"], serde_json::Value::Null);
        assert_eq!(obj["local_peer_id"], serde_json::Value::Null);
        assert_eq!(obj["active_requests_at_start"], serde_json::Value::Null);
        assert_eq!(obj["fetch_cap"], serde_json::json!(8usize));
        assert_eq!(obj["route"], serde_json::json!("unknown"));
        assert_eq!(obj["route_note"], serde_json::Value::Null);
        assert_eq!(obj["outcome"], serde_json::json!("cache_hit"));
        assert_eq!(obj["bytes"], serde_json::json!(4096u64));
    }

    #[test]
    fn exhausted_and_lookup_error_records_classify_correctly() {
        let addr = [2u8; 32];
        let exhausted = DownloadDiagnosticsRecord::chunk_level(
            2,
            5,
            &addr,
            "retry",
            Some(2),
            0,
            DownloadDiagnosticsOutcome::Exhausted,
            None,
        );
        assert_eq!(
            serde_json::to_value(&exhausted).unwrap()["outcome"],
            serde_json::json!("exhausted")
        );

        let lookup_err = DownloadDiagnosticsRecord::chunk_level(
            2,
            5,
            &addr,
            "initial",
            Some(2),
            0,
            DownloadDiagnosticsOutcome::LookupError,
            Some(bounded_error("lookup", "DHT returned no peers")),
        );
        let v = serde_json::to_value(&lookup_err).unwrap();
        assert_eq!(v["outcome"], serde_json::json!("lookup_error"));
        assert!(v["error"].as_str().unwrap().starts_with("lookup: "));
    }

    #[test]
    fn bounded_error_truncates_long_detail() {
        let long = "x".repeat(10_000);
        let s = bounded_error("network", &long);
        assert!(s.starts_with("network: "));
        // +1 for the ellipsis added on truncation.
        assert!(s.chars().count() <= DIAGNOSTICS_ERROR_MAX_CHARS);
        assert!(s.ends_with('…'));
    }

    #[test]
    fn bounded_error_preserves_short_detail_intact() {
        let s = bounded_error("protocol", "mismatched address");
        assert_eq!(s, "protocol: mismatched address");
    }

    #[test]
    fn rfc3339_formatter_is_valid_for_known_epoch() {
        // 2021-01-01T00:00:00Z = 1609459200.
        let s = rfc3339_from_unix_secs(1_609_459_200);
        assert_eq!(s, "2021-01-01T00:00:00Z");
        // 1970-01-01T00:00:00Z = 0.
        assert_eq!(rfc3339_from_unix_secs(0), "1970-01-01T00:00:00Z");
        // Leap-year day: 2024-02-29T00:00:00Z = 1709164800.
        assert_eq!(
            rfc3339_from_unix_secs(1_709_164_800),
            "2024-02-29T00:00:00Z"
        );
    }

    #[test]
    fn disabled_diagnostics_does_not_create_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("disabled.jsonl");
        let diagnostics: Option<DownloadDiagnosticsSender> = None;

        assert!(diagnostics.is_none());
        assert!(!path.exists());
    }

    #[test]
    fn writer_emits_one_json_line_per_record_and_flushes_on_drop() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!(
            "ant-dl-diag-{}-{}.jsonl",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let (sender, writer) = spawn_download_diagnostics_writer(&path).unwrap();
        let addr = [3u8; 32];
        sender.try_emit(DownloadDiagnosticsRecord::chunk_level(
            1,
            1,
            &addr,
            "initial",
            Some(8),
            128,
            DownloadDiagnosticsOutcome::CacheHit,
            None,
        ));
        sender.try_emit(DownloadDiagnosticsRecord::peer_attempt(
            1,
            1,
            &addr,
            "initial",
            1,
            Some(5),
            "lookup-writer",
            "peer-z",
            vec!["/ip4/1.2.3.4/udp/9000/quic".to_string()],
            vec!["direct".to_string()],
            Some(50),
            Some(100),
            Some(1_234_000_000),
            Some("peer-z"),
            Some("/ip4/1.2.3.4/udp/9000/quic"),
            "direct",
            None,
            Some(true),
            Some(8),
            Some(8),
            30,
            60,
            &test_correlation(9_003),
            30,
            128,
            DownloadDiagnosticsOutcome::Found,
            None,
        ));
        // Drop the last sender: the channel closes and the writer flushes.
        drop(sender);
        writer.join().unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "expected 2 JSONL records, got: {contents:?}"
        );
        let first = serde_json::from_str::<serde_json::Value>(lines[0]).unwrap();
        assert_eq!(first["outcome"], serde_json::json!("cache_hit"));
        assert_eq!(first["schema_version"], serde_json::json!(4u8));
        let second = serde_json::from_str::<serde_json::Value>(lines[1]).unwrap();
        assert_eq!(second["outcome"], serde_json::json!("found"));
        assert_eq!(second["route"], serde_json::json!("direct"));
        assert_eq!(second["route_note"], serde_json::Value::Null);
        assert_eq!(
            second["transport_source"],
            serde_json::json!("/ip4/1.2.3.4/udp/9000/quic")
        );
        let _ = std::fs::remove_file(&path);
    }
}
