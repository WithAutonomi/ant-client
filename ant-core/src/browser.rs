//! Browser-safe immutable-data primitives.
//!
//! This module deliberately contains no transport, filesystem, Tokio runtime,
//! or EVM provider. It is the shared compatibility-sensitive core used by the
//! native client and by the browser WASM package: native self-encryption,
//! public DataMap encoding, content addressing, and reconstruction.

use bytes::Bytes;
use self_encryption::{DataMap, EncryptedChunk};
use serde::{Deserialize, Serialize};

/// Maximum file size accepted by the in-memory browser demo.
pub const MAX_BROWSER_FILE_BYTES: usize = 64 * 1024 * 1024;

/// One native self-encryption chunk descriptor exposed to the browser.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserChunkInfo {
    /// Zero-based chunk index.
    pub index: usize,
    /// BLAKE3 address of the encrypted record.
    pub dst_hash: String,
    /// BLAKE3 hash of the plaintext chunk.
    pub src_hash: String,
    /// Plaintext chunk size.
    pub src_size: usize,
}

/// One content-addressed record ready for network upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRecord {
    /// Lowercase hexadecimal BLAKE3 record address.
    pub address: String,
    /// Raw record bytes. `serde_bytes` maps this to `Uint8Array` in WASM.
    #[serde(with = "serde_bytes")]
    pub content: Vec<u8>,
}

/// Result of native public-file self-encryption for a browser upload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserEncryptedFile {
    /// Public DataMap record address.
    pub address: String,
    /// Whole-file plaintext BLAKE3 hash.
    pub blake3: String,
    /// Serialized public DataMap size.
    pub data_map_size: usize,
    /// Native root DataMap chunk descriptors.
    pub chunks: Vec<BrowserChunkInfo>,
    /// Encrypted data records followed by the public DataMap record.
    pub records: Vec<BrowserRecord>,
}

/// Browser immutable-data processing error.
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// Input did not satisfy the browser demo limits.
    #[error("invalid browser data: {0}")]
    Invalid(String),
    /// Native self-encryption failed.
    #[error("self-encryption failed: {0}")]
    SelfEncryption(String),
    /// Public DataMap encoding or decoding failed.
    #[error("DataMap serialization failed: {0}")]
    DataMap(String),
}

/// BLAKE3-address bytes using the same lowercase hexadecimal representation as
/// the native chunk protocol.
#[must_use]
pub fn content_address(content: &[u8]) -> String {
    blake3::hash(content).to_hex().to_string()
}

/// Verify raw record bytes against a lowercase or uppercase hexadecimal BLAKE3
/// address.
pub fn verify_record(address: &str, content: &[u8]) -> Result<(), BrowserError> {
    let expected = address.strip_prefix("0x").unwrap_or(address);
    if expected.len() != 64 || !expected.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(BrowserError::Invalid(
            "record address must be 32 hexadecimal bytes".to_string(),
        ));
    }
    let actual = content_address(content);
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(BrowserError::Invalid(format!(
            "BLAKE3 mismatch: expected {}, received {actual}",
            expected.to_ascii_lowercase()
        )));
    }
    Ok(())
}

/// Encrypt a complete public file with the native `self_encryption 0.36`
/// implementation and append its MessagePack DataMap as a public record.
pub fn encrypt_public_file(content: &[u8]) -> Result<BrowserEncryptedFile, BrowserError> {
    if content.len() < self_encryption::MIN_ENCRYPTABLE_BYTES {
        return Err(BrowserError::Invalid(format!(
            "self-encryption requires at least {} bytes",
            self_encryption::MIN_ENCRYPTABLE_BYTES
        )));
    }
    if content.len() > MAX_BROWSER_FILE_BYTES {
        return Err(BrowserError::Invalid(format!(
            "browser files are limited to {MAX_BROWSER_FILE_BYTES} bytes"
        )));
    }

    let whole_file_hash = content_address(content);
    let (data_map, encrypted_chunks) = self_encryption::encrypt(Bytes::copy_from_slice(content))
        .map_err(|error| BrowserError::SelfEncryption(error.to_string()))?;
    if data_map.is_child() {
        return Err(BrowserError::Invalid(
            "nested DataMaps are not supported by the browser client".to_string(),
        ));
    }
    let chunks = chunk_infos(&data_map);
    if encrypted_chunks.len() != chunks.len() {
        return Err(BrowserError::SelfEncryption(format!(
            "native encryptor returned {} records for {} DataMap entries",
            encrypted_chunks.len(),
            chunks.len()
        )));
    }

    let mut records: Vec<BrowserRecord> = encrypted_chunks
        .into_iter()
        .zip(&chunks)
        .map(|(chunk, info)| BrowserRecord {
            address: info.dst_hash.clone(),
            content: chunk.content.to_vec(),
        })
        .collect();
    let encoded_data_map =
        rmp_serde::to_vec(&data_map).map_err(|error| BrowserError::DataMap(error.to_string()))?;
    let address = content_address(&encoded_data_map);
    let data_map_size = encoded_data_map.len();
    records.push(BrowserRecord {
        address: address.clone(),
        content: encoded_data_map,
    });

    Ok(BrowserEncryptedFile {
        address,
        blake3: whole_file_hash,
        data_map_size,
        chunks,
        records,
    })
}

/// Decode and normalize a native public DataMap.
pub fn decode_public_data_map(content: &[u8]) -> Result<Vec<BrowserChunkInfo>, BrowserError> {
    let data_map: DataMap =
        rmp_serde::from_slice(content).map_err(|error| BrowserError::DataMap(error.to_string()))?;
    if data_map.is_child() {
        return Err(BrowserError::Invalid(
            "nested DataMaps are not supported by the browser client".to_string(),
        ));
    }
    Ok(chunk_infos(&data_map))
}

/// Reconstruct a public file with native self-encryption after verifying every
/// encrypted record against its DataMap destination address.
pub fn decrypt_public_file(
    data_map_content: &[u8],
    encrypted_contents: &[Vec<u8>],
) -> Result<Vec<u8>, BrowserError> {
    let data_map: DataMap = rmp_serde::from_slice(data_map_content)
        .map_err(|error| BrowserError::DataMap(error.to_string()))?;
    if data_map.is_child() {
        return Err(BrowserError::Invalid(
            "nested DataMaps are not supported by the browser client".to_string(),
        ));
    }
    if encrypted_contents.len() != data_map.infos().len() {
        return Err(BrowserError::Invalid(format!(
            "received {} encrypted records for {} DataMap entries",
            encrypted_contents.len(),
            data_map.infos().len()
        )));
    }

    let encrypted_chunks = data_map
        .infos()
        .iter()
        .zip(encrypted_contents)
        .map(|(info, content)| {
            verify_record(&hex::encode(info.dst_hash.0), content)?;
            Ok(EncryptedChunk {
                content: Bytes::copy_from_slice(content),
            })
        })
        .collect::<Result<Vec<_>, BrowserError>>()?;
    self_encryption::decrypt(&data_map, &encrypted_chunks)
        .map(|bytes| bytes.to_vec())
        .map_err(|error| BrowserError::SelfEncryption(error.to_string()))
}

fn chunk_infos(data_map: &DataMap) -> Vec<BrowserChunkInfo> {
    data_map
        .infos()
        .iter()
        .map(|info| BrowserChunkInfo {
            index: info.index,
            dst_hash: hex::encode(info.dst_hash.0),
            src_hash: hex::encode(info.src_hash.0),
            src_size: info.src_size,
        })
        .collect()
}

#[cfg(all(target_arch = "wasm32", feature = "browser-wasm"))]
mod wasm {
    use super::{content_address, decrypt_public_file, encrypt_public_file, verify_record};
    use js_sys::{Array, Function, Promise, Uint8Array};
    use saorsa_dht_lookup::{
        run_iterative_lookup, IterativeLookup, LookupConfig, LookupKey, LookupNode, LookupQuery,
        LookupQueryOutcome,
    };
    use serde::{Deserialize, Serialize};
    use std::collections::HashMap;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen_futures::JsFuture;

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(untagged)]
    enum BrowserLookupEndpoint {
        Structured { multiaddr: String },
        Multiaddr(String),
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    struct BrowserLookupNode {
        peer_id: String,
        #[serde(default)]
        native_addresses: Vec<String>,
        #[serde(default)]
        reliability: f64,
        #[serde(default)]
        webtransport: Option<BrowserLookupEndpoint>,
    }

    #[derive(Debug, Serialize)]
    struct BrowserLookupBatch {
        target: String,
        count: usize,
        iteration: usize,
        candidates: Vec<BrowserLookupNode>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(tag = "status", rename_all = "snake_case")]
    enum BrowserLookupQueryOutcome {
        Succeeded {
            responder: String,
            #[serde(default)]
            candidates: Vec<BrowserLookupNode>,
        },
        Failed {
            responder: String,
        },
        Unresponsive {
            responder: String,
        },
    }

    #[derive(Debug, Clone)]
    struct BrowserLookupCandidate {
        peer_id: LookupKey,
        wire: BrowserLookupNode,
    }

    impl LookupNode for BrowserLookupCandidate {
        fn lookup_peer_id(&self) -> LookupKey {
            self.peer_id
        }
    }

    impl BrowserLookupCandidate {
        fn parse(mut wire: BrowserLookupNode) -> Result<Self, JsValue> {
            let peer_id = parse_lookup_key(&wire.peer_id, "peer ID")?;
            wire.peer_id = hex::encode(peer_id);
            Ok(Self { peer_id, wire })
        }
    }

    /// Shared Saorsa iterative lookup state driven by browser WebTransport.
    #[wasm_bindgen(js_name = BrowserIterativeLookup)]
    pub struct BrowserIterativeLookup {
        lookup: IterativeLookup<BrowserLookupCandidate>,
        known_endpoints: HashMap<LookupKey, BrowserLookupEndpoint>,
    }

    #[wasm_bindgen(js_class = BrowserIterativeLookup)]
    impl BrowserIterativeLookup {
        /// Construct a browser lookup using the same scheduler as native QUIC.
        #[wasm_bindgen(constructor)]
        pub fn new(
            target: &str,
            count: usize,
            alpha: usize,
            max_iterations: usize,
        ) -> Result<Self, JsValue> {
            let target = parse_lookup_key(target, "lookup target")?;
            let config = LookupConfig {
                count,
                alpha,
                max_iterations,
                ..LookupConfig::saorsa(count)
            };
            let lookup = IterativeLookup::new(target, config)
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
            Ok(Self {
                lookup,
                known_endpoints: HashMap::new(),
            })
        }

        /// Add validated bootstrap or FIND_NODE candidates.
        #[wasm_bindgen(js_name = addCandidates)]
        pub fn add_candidates(&mut self, nodes: JsValue) -> Result<(), JsValue> {
            for candidate in parse_lookup_nodes(nodes)? {
                self.add_candidate(candidate);
            }
            Ok(())
        }

        /// Run the complete shared Saorsa walk through a WebTransport batch callback.
        #[wasm_bindgen(js_name = run)]
        pub async fn run(&mut self, query_batch: Function) -> Result<String, JsValue> {
            let mut query = BrowserLookupQuery {
                callback: query_batch,
                known_endpoints: &mut self.known_endpoints,
            };
            run_iterative_lookup(&mut self.lookup, &mut query)
                .await
                .map(|termination| format!("{termination:?}"))
                .map_err(|error| JsValue::from_str(&error.to_string()))
        }

        /// Successful responders in final closest-first order.
        #[wasm_bindgen(js_name = results)]
        pub fn results(&self) -> Result<JsValue, JsValue> {
            let nodes = self
                .lookup
                .results()
                .into_iter()
                .map(|candidate| candidate.wire)
                .collect::<Vec<_>>();
            serde_wasm_bindgen::to_value(&nodes)
                .map_err(|error| JsValue::from_str(&error.to_string()))
        }

        /// Peer IDs selected for network queries, in query order.
        #[wasm_bindgen(js_name = queriedPeers)]
        pub fn queried_peers(&self) -> Result<JsValue, JsValue> {
            let peers = self
                .lookup
                .queried_peers()
                .iter()
                .map(hex::encode)
                .collect::<Vec<_>>();
            serde_wasm_bindgen::to_value(&peers)
                .map_err(|error| JsValue::from_str(&error.to_string()))
        }
    }

    impl BrowserIterativeLookup {
        fn add_candidate(&mut self, candidate: BrowserLookupCandidate) {
            if let Some(candidate) =
                resolve_candidate_endpoint(&mut self.known_endpoints, candidate)
            {
                let _ = self.lookup.add_candidate(candidate);
            }
        }
    }

    struct BrowserLookupQuery<'a> {
        callback: Function,
        known_endpoints: &'a mut HashMap<LookupKey, BrowserLookupEndpoint>,
    }

    impl LookupQuery<BrowserLookupCandidate> for BrowserLookupQuery<'_> {
        type Error = String;

        async fn query_batch(
            &mut self,
            target: LookupKey,
            count: usize,
            iteration: usize,
            batch: Vec<BrowserLookupCandidate>,
        ) -> Result<Vec<LookupQueryOutcome<BrowserLookupCandidate>>, Self::Error> {
            let request = BrowserLookupBatch {
                target: hex::encode(target),
                count,
                iteration,
                candidates: batch.into_iter().map(|candidate| candidate.wire).collect(),
            };
            let request = serde_wasm_bindgen::to_value(&request)
                .map_err(|error| format!("could not encode lookup batch: {error}"))?;
            let returned = self
                .callback
                .call1(&JsValue::NULL, &request)
                .map_err(js_error_message)?;
            let returned = JsFuture::from(Promise::resolve(&returned))
                .await
                .map_err(js_error_message)?;
            let outcomes: Vec<BrowserLookupQueryOutcome> = serde_wasm_bindgen::from_value(returned)
                .map_err(|error| format!("invalid lookup batch response: {error}"))?;

            outcomes
                .into_iter()
                .map(|outcome| match outcome {
                    BrowserLookupQueryOutcome::Succeeded {
                        responder,
                        candidates,
                    } => {
                        let responder = parse_lookup_key(&responder, "lookup responder")
                            .map_err(js_error_message)?;
                        let candidates = candidates
                            .into_iter()
                            .map(BrowserLookupCandidate::parse)
                            .collect::<Result<Vec<_>, _>>()
                            .map_err(js_error_message)?
                            .into_iter()
                            .filter_map(|candidate| {
                                resolve_candidate_endpoint(self.known_endpoints, candidate)
                            })
                            .collect();
                        Ok(LookupQueryOutcome::Succeeded {
                            responder,
                            candidates,
                        })
                    }
                    BrowserLookupQueryOutcome::Failed { responder } => {
                        parse_lookup_key(&responder, "lookup responder")
                            .map(|responder| LookupQueryOutcome::Failed { responder })
                            .map_err(js_error_message)
                    }
                    BrowserLookupQueryOutcome::Unresponsive { responder } => {
                        parse_lookup_key(&responder, "lookup responder")
                            .map(|responder| LookupQueryOutcome::Unresponsive { responder })
                            .map_err(js_error_message)
                    }
                })
                .collect()
        }
    }

    fn resolve_candidate_endpoint(
        known_endpoints: &mut HashMap<LookupKey, BrowserLookupEndpoint>,
        mut candidate: BrowserLookupCandidate,
    ) -> Option<BrowserLookupCandidate> {
        if let Some(endpoint) = candidate.wire.webtransport.clone() {
            known_endpoints.insert(candidate.peer_id, endpoint);
        } else if let Some(endpoint) = known_endpoints.get(&candidate.peer_id) {
            candidate.wire.webtransport = Some(endpoint.clone());
        }
        candidate.wire.webtransport.as_ref()?;
        Some(candidate)
    }

    fn js_error_message(value: JsValue) -> String {
        value
            .as_string()
            .unwrap_or_else(|| format!("JavaScript lookup callback failed: {value:?}"))
    }

    fn parse_lookup_nodes(value: JsValue) -> Result<Vec<BrowserLookupCandidate>, JsValue> {
        let nodes: Vec<BrowserLookupNode> = serde_wasm_bindgen::from_value(value)
            .map_err(|error| JsValue::from_str(&format!("invalid lookup nodes: {error}")))?;
        nodes
            .into_iter()
            .map(BrowserLookupCandidate::parse)
            .collect()
    }

    fn parse_lookup_key(value: &str, label: &str) -> Result<LookupKey, JsValue> {
        let value = value.strip_prefix("0x").unwrap_or(value);
        let bytes = hex::decode(value)
            .map_err(|error| JsValue::from_str(&format!("invalid {label}: {error}")))?;
        bytes.try_into().map_err(|bytes: Vec<u8>| {
            JsValue::from_str(&format!(
                "invalid {label}: expected 32 bytes, received {}",
                bytes.len()
            ))
        })
    }

    /// Install a readable panic hook for browser developer tools.
    #[wasm_bindgen(start)]
    pub fn start() {
        console_error_panic_hook::set_once();
    }

    /// Native `self_encryption` plus public DataMap generation.
    #[wasm_bindgen(js_name = encryptPublicFile)]
    pub fn encrypt_public_file_wasm(content: &[u8]) -> Result<JsValue, JsValue> {
        let encrypted =
            encrypt_public_file(content).map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&encrypted)
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Native BLAKE3 content address.
    #[wasm_bindgen(js_name = contentAddress)]
    #[must_use]
    pub fn content_address_wasm(content: &[u8]) -> String {
        content_address(content)
    }

    /// Verify one content-addressed record with native BLAKE3.
    #[wasm_bindgen(js_name = verifyRecord)]
    pub fn verify_record_wasm(address: &str, content: &[u8]) -> Result<String, JsValue> {
        verify_record(address, content).map_err(|error| JsValue::from_str(&error.to_string()))?;
        Ok(content_address(content))
    }

    /// Decode a native public DataMap for browser-side record retrieval.
    #[wasm_bindgen(js_name = decodePublicDataMap)]
    pub fn decode_public_data_map_wasm(content: &[u8]) -> Result<JsValue, JsValue> {
        let chunks = super::decode_public_data_map(content)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&chunks).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Native public DataMap decoding and whole-file reconstruction.
    #[wasm_bindgen(js_name = decryptPublicFile)]
    pub fn decrypt_public_file_wasm(
        data_map_content: &[u8],
        encrypted_contents: Array,
    ) -> Result<Uint8Array, JsValue> {
        let encrypted_contents = encrypted_contents
            .iter()
            .map(|value| Uint8Array::new(&value).to_vec())
            .collect::<Vec<_>>();
        let plaintext = decrypt_public_file(data_map_content, &encrypted_contents)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        Ok(Uint8Array::from(plaintext.as_slice()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<u8> {
        "browser whole-file fixture\n".repeat(160).into_bytes()
    }

    #[test]
    fn native_browser_encrypt_matches_existing_wire_vector() {
        let encrypted = encrypt_public_file(&fixture()).expect("encrypt fixture");
        assert_eq!(
            encrypted
                .chunks
                .iter()
                .map(|chunk| chunk.dst_hash.as_str())
                .collect::<Vec<_>>(),
            vec![
                "c024c6884a2f39be7ba07c3d9636efedeb94df7397fcd38bac5ae904643c5cc9",
                "350a88e6eb0b2a3e774107a212a272b4191af69ca4366a4b91f5a1e5872c459a",
                "d73db5a8b0be3b571b40d2b80ff490fe45e135f1992c5863ecb78e25d00ceddb",
            ]
        );
        assert_eq!(
            encrypted.address,
            "0d3636dd504d04a236f7e104909234766f077fa7e1ca4a18293d3d168d5f169b"
        );
        assert_eq!(
            encrypted.blake3,
            "e0e422267ac59c56bf032d6d830035d343369d20147dd5f6b63351a29b015f22"
        );
    }

    #[test]
    fn native_browser_round_trip_and_tamper_rejection() {
        let content = fixture();
        let encrypted = encrypt_public_file(&content).expect("encrypt fixture");
        let data_map = &encrypted.records.last().expect("DataMap record").content;
        let chunks = encrypted.records[..encrypted.records.len() - 1]
            .iter()
            .map(|record| record.content.clone())
            .collect::<Vec<_>>();
        assert_eq!(
            decrypt_public_file(data_map, &chunks).expect("decrypt fixture"),
            content
        );

        let mut tampered = chunks;
        tampered[0][0] ^= 1;
        assert!(decrypt_public_file(data_map, &tampered).is_err());
    }
}
