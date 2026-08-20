//! The Elasticsearch bulk sink (V2-1016 contract).
//!
//! The endpoint is Elasticsearch itself behind a transparent reverse proxy, not a translation
//! layer, so this speaks plain `_bulk`. Four details of that contract are easy to get wrong and
//! expensive to debug, so they are stated here rather than left implicit in the code:
//!
//! 1. **The action must be `create`, never `index`.** The write key grants `create_doc`, which can
//!    create but not overwrite — `index` comes back as a per-item 403. That restriction is
//!    deliberate: it stops one beta participant overwriting another's document by `_id`.
//! 2. **A `_bulk` response is `200 OK` even when documents failed.** Success is per position in
//!    `items[]`; trusting the HTTP status alone silently discards failures.
//! 3. **`200` at a position is a success, not a retry.** It means the server-side level filter
//!    dropped the document. It is reported as success precisely so forwarders do not retry it
//!    forever.
//! 4. **`409` is also a success.** It means a document with that `_id` is already indexed — our own
//!    earlier attempt landed after all. That is the entire point of the deterministic `_id`, and
//!    treating it as an error would turn a successful recovery into a reported failure.
//!
//! The proxy forces `filter_path=errors,items.*.status,items.*.error` on the response, so positions
//! line up with the submitted documents and a clean batch costs a handful of bytes to acknowledge.

use std::time::Duration;

use futures::future::BoxFuture;

use super::document::ForwardDocument;
use super::sink::{BatchOutcome, DocumentOutcome, LogSink};

/// How long to wait for the endpoint before giving up on a batch.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Ships documents to an Elasticsearch `_bulk` endpoint.
pub struct ElasticsearchSink {
    client: reqwest::Client,
    bulk_url: String,
    token: String,
}

impl ElasticsearchSink {
    /// Build a sink for the given endpoint base and write-only API key.
    pub fn new(endpoint_base: &str, token: &str) -> crate::error::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| crate::error::Error::LogForward(format!("HTTP client: {e}")))?;

        Ok(Self {
            client,
            bulk_url: format!("{}/_bulk", endpoint_base.trim_end_matches('/')),
            token: token.to_string(),
        })
    }

    /// Frame documents as an NDJSON bulk body.
    ///
    /// Each document contributes two lines — the `create` action naming its index and id, then its
    /// source — and the body ends with a newline, which Elasticsearch requires.
    #[must_use]
    pub fn build_body(batch: &[ForwardDocument]) -> String {
        let mut body = String::new();

        for document in batch {
            let action = serde_json::json!({
                "create": { "_index": document.index, "_id": document.id }
            });
            // Serialization of these types cannot fail: the action is built from strings here, and
            // the source is a plain struct of strings and options.
            body.push_str(&serde_json::to_string(&action).unwrap_or_default());
            body.push('\n');
            body.push_str(&serde_json::to_string(&document.source).unwrap_or_default());
            body.push('\n');
        }

        body
    }

    /// Map a per-item bulk status onto what the forwarder should do next.
    #[must_use]
    pub fn classify_item_status(status: u64) -> DocumentOutcome {
        match status {
            // Created, dropped by the server-side level filter, or already present from an earlier
            // attempt of ours. All three mean "stop carrying this document around".
            200..=299 | 409 => DocumentOutcome::Delivered,
            // Busy or briefly unavailable.
            429 | 500..=599 => DocumentOutcome::Retryable,
            // Anything else — 400 mapping conflicts, 403 permission errors — will fail identically
            // on every retry.
            _ => DocumentOutcome::Rejected,
        }
    }

    /// Interpret a bulk response body against the batch that produced it.
    #[must_use]
    pub fn classify_response(body: &str, batch_len: usize) -> BatchOutcome {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
            // An unparseable body from a 2xx response is not something a retry will fix, but nor is
            // it safe to call the documents delivered.
            return BatchOutcome {
                outcomes: vec![DocumentOutcome::Retryable; batch_len],
                transport_failure: false,
                error: Some("could not parse the bulk response".to_string()),
            };
        };

        if value.get("errors").and_then(serde_json::Value::as_bool) == Some(false) {
            return BatchOutcome::all_delivered(batch_len);
        }

        let Some(items) = value.get("items").and_then(serde_json::Value::as_array) else {
            return BatchOutcome {
                outcomes: vec![DocumentOutcome::Retryable; batch_len],
                transport_failure: false,
                error: Some("bulk response reported errors but listed no items".to_string()),
            };
        };

        let mut outcomes = Vec::with_capacity(items.len());
        // A batch can fail two ways at once — a transient 429 here, a permanent 403 there. Status
        // output has room for one, and the permanent one is the one the user can act on, so it
        // wins regardless of which came first in the array.
        let mut permanent_error = None;
        let mut transient_error = None;

        for item in items {
            // The action key is `create`, but read whatever key is present rather than assume it.
            let entry = item
                .as_object()
                .and_then(|object| object.values().next())
                .and_then(serde_json::Value::as_object);

            let status = entry
                .and_then(|entry| entry.get("status"))
                .and_then(serde_json::Value::as_u64);

            let outcome = match status {
                Some(status) => {
                    let classified = Self::classify_item_status(status);
                    match classified {
                        DocumentOutcome::Rejected if permanent_error.is_none() => {
                            permanent_error = Some(describe_item_error(entry, status));
                        }
                        DocumentOutcome::Retryable if transient_error.is_none() => {
                            transient_error = Some(describe_item_error(entry, status));
                        }
                        _ => {}
                    }
                    classified
                }
                // No status for this position: nothing is known, so do not claim it landed.
                None => DocumentOutcome::Retryable,
            };
            outcomes.push(outcome);
        }

        // A response shorter than the batch leaves a tail unaccounted for; `deliver` retries any
        // position it has no outcome for, so padding here would actively lose documents.
        BatchOutcome {
            outcomes,
            transport_failure: false,
            error: permanent_error.or(transient_error),
        }
    }
}

fn describe_item_error(
    entry: Option<&serde_json::Map<String, serde_json::Value>>,
    status: u64,
) -> String {
    let reason = entry
        .and_then(|entry| entry.get("error"))
        .and_then(|error| error.get("reason"))
        .and_then(serde_json::Value::as_str);

    match reason {
        Some(reason) => format!("bulk item failed with {status}: {reason}"),
        None => format!("bulk item failed with {status}"),
    }
}

impl LogSink for ElasticsearchSink {
    fn send<'a>(&'a self, batch: &'a [ForwardDocument]) -> BoxFuture<'a, BatchOutcome> {
        Box::pin(async move {
            if batch.is_empty() {
                return BatchOutcome::all_delivered(0);
            }

            let response = self
                .client
                .post(&self.bulk_url)
                .header("Authorization", format!("ApiKey {}", self.token))
                .header("Content-Type", "application/x-ndjson")
                .body(Self::build_body(batch))
                .send()
                .await;

            let response = match response {
                Ok(response) => response,
                // The request never completed, so nothing is known about what landed. Replaying is
                // safe because every document carries a deterministic `_id`.
                Err(error) => return BatchOutcome::transport_failure(error.to_string()),
            };

            let status = response.status();

            if status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Self::classify_response(&body, batch.len());
            }

            // A whole-request rejection. 429 and 5xx are worth another go; 401 (bad key), 413
            // (body too large) and the rest will fail the same way every time.
            let outcome = if status.as_u16() == 429 || status.is_server_error() {
                DocumentOutcome::Retryable
            } else {
                DocumentOutcome::Rejected
            };

            BatchOutcome {
                outcomes: vec![outcome; batch.len()],
                transport_failure: false,
                error: Some(format!("bulk request rejected with HTTP {status}")),
            }
        })
    }

    fn describe(&self) -> String {
        self.bulk_url.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::daemon::forward::document::DocumentSource;

    fn document(id: &str, index: &str) -> ForwardDocument {
        ForwardDocument {
            id: id.to_string(),
            index: index.to_string(),
            source: DocumentSource {
                timestamp: "2026-08-19T20:50:00.000000Z".to_string(),
                level: "INFO".to_string(),
                target: Some("ant_node::node".to_string()),
                message: "hello".to_string(),
                node_id: "7".to_string(),
                service: "node7".to_string(),
                binary_version: "0.17.2".to_string(),
                channel: "beta".to_string(),
                os: "linux".to_string(),
                arch: "x86_64".to_string(),
                peer_id: None,
                version: None,
                commit: None,
            },
        }
    }

    #[test]
    fn the_bulk_action_is_create_not_index() {
        let body = ElasticsearchSink::build_body(&[document("id-1", "beta-nodes-2026.08.19")]);
        let action: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();

        assert!(
            action.get("create").is_some(),
            "`index` is refused with a per-item 403: {body}"
        );
        assert!(action.get("index").is_none());
        assert_eq!(action["create"]["_index"], "beta-nodes-2026.08.19");
        assert_eq!(action["create"]["_id"], "id-1");
    }

    #[test]
    fn the_body_is_ndjson_with_the_required_trailing_newline() {
        let body = ElasticsearchSink::build_body(&[
            document("id-1", "beta-nodes-2026.08.19"),
            document("id-2", "beta-nodes-2026.08.19"),
        ]);

        assert!(body.ends_with('\n'), "Elasticsearch requires it");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 4, "one action and one source per document");

        for line in &lines {
            serde_json::from_str::<serde_json::Value>(line)
                .unwrap_or_else(|_| panic!("every line must be standalone JSON: {line}"));
        }
        let source: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(source["@timestamp"], "2026-08-19T20:50:00.000000Z");
    }

    #[test]
    fn an_empty_batch_produces_an_empty_body() {
        assert_eq!(ElasticsearchSink::build_body(&[]), "");
    }

    /// 201 created, 200 dropped by the server-side level filter, and 409 already-indexed all mean
    /// the forwarder is finished with the document.
    #[test]
    fn created_filtered_and_already_indexed_all_count_as_delivered() {
        for status in [200, 201, 409] {
            assert_eq!(
                ElasticsearchSink::classify_item_status(status),
                DocumentOutcome::Delivered,
                "status {status}"
            );
        }
    }

    #[test]
    fn busy_and_server_errors_are_retryable() {
        for status in [429, 500, 502, 503] {
            assert_eq!(
                ElasticsearchSink::classify_item_status(status),
                DocumentOutcome::Retryable,
                "status {status}"
            );
        }
    }

    #[test]
    fn permission_and_mapping_errors_are_permanent() {
        for status in [400, 401, 403, 404] {
            assert_eq!(
                ElasticsearchSink::classify_item_status(status),
                DocumentOutcome::Rejected,
                "status {status}"
            );
        }
    }

    #[test]
    fn a_clean_response_delivers_the_whole_batch() {
        let outcome = ElasticsearchSink::classify_response(r#"{"errors":false}"#, 3);
        assert_eq!(outcome.outcomes, vec![DocumentOutcome::Delivered; 3]);
        assert!(outcome.error.is_none());
        assert!(!outcome.transport_failure);
    }

    /// The shape the proxy's forced `filter_path` produces on a dirty batch: one entry per
    /// submitted document, positions preserved.
    #[test]
    fn a_mixed_response_is_mapped_position_by_position() {
        let body = r#"{
            "errors": true,
            "items": [
                {"create": {"status": 201}},
                {"create": {"status": 429}},
                {"create": {"status": 403, "error": {"reason": "action [create] is unauthorized"}}},
                {"create": {"status": 200}},
                {"create": {"status": 409}}
            ]
        }"#;

        let outcome = ElasticsearchSink::classify_response(body, 5);

        assert_eq!(
            outcome.outcomes,
            vec![
                DocumentOutcome::Delivered,
                DocumentOutcome::Retryable,
                DocumentOutcome::Rejected,
                DocumentOutcome::Delivered,
                DocumentOutcome::Delivered,
            ]
        );
        let error = outcome.error.unwrap();
        assert!(
            error.contains("403") && error.contains("unauthorized"),
            "the permanent failure is the actionable one, not the transient 429: {error}"
        );
    }

    /// With nothing permanent to report, the transient failure is better than saying nothing.
    #[test]
    fn a_transient_error_is_surfaced_when_it_is_the_only_one() {
        let body = r#"{"errors":true,"items":[{"create":{"status":429}}]}"#;
        let error = ElasticsearchSink::classify_response(body, 1).error.unwrap();
        assert!(error.contains("429"), "{error}");
    }

    /// A 409 is our own earlier attempt having landed — a recovery, not a failure worth reporting.
    #[test]
    fn a_conflict_is_not_reported_as_an_error() {
        let body = r#"{"errors":true,"items":[{"create":{"status":409}}]}"#;
        let outcome = ElasticsearchSink::classify_response(body, 1);

        assert_eq!(outcome.outcomes, vec![DocumentOutcome::Delivered]);
        assert!(
            outcome.error.is_none(),
            "a deduplicated replay is the mechanism working"
        );
    }

    #[test]
    fn a_short_items_array_leaves_the_tail_unaccounted_for() {
        let body = r#"{"errors":true,"items":[{"create":{"status":201}}]}"#;
        let outcome = ElasticsearchSink::classify_response(body, 3);

        assert_eq!(
            outcome.outcomes.len(),
            1,
            "the tail is left for deliver() to retry rather than assumed delivered"
        );
    }

    #[test]
    fn an_item_without_a_status_is_retried_rather_than_assumed_delivered() {
        let body = r#"{"errors":true,"items":[{"create":{}}]}"#;
        let outcome = ElasticsearchSink::classify_response(body, 1);
        assert_eq!(outcome.outcomes, vec![DocumentOutcome::Retryable]);
    }

    #[test]
    fn an_unparseable_body_is_retried_not_discarded() {
        let outcome = ElasticsearchSink::classify_response("<html>gateway error</html>", 2);
        assert_eq!(outcome.outcomes, vec![DocumentOutcome::Retryable; 2]);
        assert!(outcome.error.unwrap().contains("parse"));
    }

    #[test]
    fn errors_reported_without_items_are_retried() {
        let outcome = ElasticsearchSink::classify_response(r#"{"errors":true}"#, 2);
        assert_eq!(outcome.outcomes, vec![DocumentOutcome::Retryable; 2]);
    }

    #[test]
    fn the_sink_describes_the_bulk_url_it_targets() {
        let sink = ElasticsearchSink::new("https://logs.autonomi.com/", "key").unwrap();
        assert_eq!(sink.describe(), "https://logs.autonomi.com/_bulk");
    }
}
