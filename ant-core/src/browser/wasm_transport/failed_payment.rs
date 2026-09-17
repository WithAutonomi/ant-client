// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Explicit reconciliation of terminal wallet failures, separate from submission.

use super::*;
use crate::data::client::upload_state::PaymentAttempt;

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "camelCase", deny_unknown_fields)]
enum FailureResolution {
    NotSubmitted {
        evidence: serde_json::Value,
    },
    Reverted {
        #[serde(rename = "transactionHashes")]
        transaction_hashes: Vec<String>,
        evidence: serde_json::Value,
    },
}

impl FailureResolution {
    fn validate(&self, attempt: &PaymentAttempt) -> Result<(), String> {
        let evidence = match self {
            Self::NotSubmitted { evidence } => {
                if !attempt.submissions.is_empty() || attempt.receipt.is_some() {
                    return Err(
                        "submission evidence exists; verify every transaction reverted".into(),
                    );
                }
                evidence
            }
            Self::Reverted {
                transaction_hashes,
                evidence,
            } => {
                let reverted = transaction_hashes
                    .iter()
                    .map(|hash| parse_lookup_key(hash, "reverted transaction hash"))
                    .collect::<Result<HashSet<_>, _>>()?;
                let mut submitted = HashSet::new();
                for value in attempt.submissions.iter().chain(attempt.receipt.iter()) {
                    let mut hashes = Vec::new();
                    if let Some(hash) = value.get("transactionHash").and_then(|v| v.as_str()) {
                        hashes.push(hash);
                    }
                    if let Some(mapping) =
                        value.get("transactionHashes").and_then(|v| v.as_object())
                    {
                        for hash in mapping.values() {
                            hashes.push(hash.as_str().ok_or("invalid journal transaction hash")?);
                        }
                    }
                    if hashes.is_empty() {
                        return Err(
                            "payment journal contains unrecognised submission evidence".into()
                        );
                    }
                    for hash in hashes {
                        submitted.insert(parse_lookup_key(hash, "journal transaction hash")?);
                    }
                }
                if submitted.is_empty() || submitted != reverted {
                    return Err(
                        "reverted transaction hashes must cover every journaled transaction".into(),
                    );
                }
                evidence
            }
        };
        if evidence.as_object().is_none_or(|value| value.is_empty()) {
            return Err("verified failure requires wallet or chain evidence".into());
        }
        Ok(())
    }
}

#[wasm_bindgen(js_class = BrowserNetworkClient)]
impl BrowserNetworkClient {
    /// Resolve a definitively failed payment, persist the updated checkpoint, and return it.
    ///
    /// Call only after the original upload and wallet request have finished. The trusted
    /// `verify_failure(attempt, scope)` callback must independently verify the entire
    /// journal against the original wallet/payment network, without submitting payment.
    /// It returns `{ status: "notSubmitted", evidence: {...} }` only when it can prove
    /// the wallet never submitted and cannot still submit, or
    /// `{ status: "reverted", transactionHashes: [...], evidence: {...} }` after verifying
    /// final reverts for every transaction. A missing receipt or timeout is insufficient.
    ///
    /// Rust checks the result against the journal; the callback owns wallet/chain
    /// verification, just as payment callbacks own confirmation. Failure evidence is
    /// archived, confirmed proofs are retained, and persistence is awaited before return.
    /// Resume the normal upload explicitly with the returned checkpoint.
    #[wasm_bindgen(js_name = reconcileFailedUploadPayment)]
    pub async fn reconcile_failed_upload_payment(
        &self,
        snapshot: String,
        verify_failure: js_sys::Function,
        on_checkpoint: js_sys::Function,
    ) -> Result<String, JsValue> {
        let result = async {
            let envelope = UploadCheckpoint::envelope(&snapshot)?;
            let checkpoint = UploadCheckpoint {
                snapshot: Some(snapshot),
                callback: Some(on_checkpoint),
                ..Default::default()
            };
            let mut state = checkpoint.restore(&envelope.scope)?;
            let attempt = state.pending_payment.as_ref().ok_or("no pending payment")?;
            let input = attempt
                .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
                .map_err(|e| e.to_string())?;
            let returned = verify_failure
                .call2(&JsValue::NULL, &input, &JsValue::from_str(&envelope.scope))
                .map_err(js_error_message)?;
            let value = JsFuture::from(Promise::resolve(&returned))
                .await
                .map_err(js_error_message)?;
            let raw: serde_json::Value = serde_wasm_bindgen::from_value(value)
                .map_err(|e| format!("invalid payment failure evidence: {e}"))?;
            let resolution: FailureResolution = serde_json::from_value(raw.clone())
                .map_err(|e| format!("payment failure is not verified: {e}"))?;
            resolution.validate(attempt)?;
            state
                .record_failed_payment(raw)
                .map_err(|e| e.to_string())?;
            checkpoint.save(&envelope.scope, &state).await?;
            UploadCheckpoint::encode(&envelope.scope, &state)
        }
        .await;
        result.map_err(|error: String| JsValue::from_str(&error))
    }
}
