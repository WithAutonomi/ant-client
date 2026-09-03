//! Cross-platform Autonomi client logic and browser bindings.
//!
//! The manifest, protocol, payment, and immutable-data modules are portable
//! Rust shared by native clients and the browser WASM package. The
//! `browser-wasm` feature additionally provides the `web-sys` WebRTC Direct
//! host adapter. Higher-level browser integration is owned by the companion
//! `ant-client-browser-sdk` project.

pub mod manifest;
pub mod payment;
pub mod protocol;

pub use manifest::{
    parse_browser_manifest, validate_browser_payment_network, BrowserManifest,
    BrowserManifestEndpoint, BrowserPaymentNetwork, PublicFileDescriptor, BROWSER_MANIFEST_VERSION,
};
pub use payment::{
    storage_payment_total, verify_storage_quote, BrowserQuoteArtifact, VerifiedStorageQuote,
};
pub use protocol::{
    parse_webrtc_direct_multiaddr, WebRtcDirectEndpoint, BROWSER_PROTOCOL_NAME,
    BROWSER_PROTOCOL_VERSION, WEBRTC_DIRECT_DATA_CHANNEL,
};

#[cfg(all(target_arch = "wasm32", feature = "browser-wasm"))]
mod wasm_transport;

use bytes::Bytes;
use self_encryption::{DataMap, EncryptedChunk};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Maximum file size accepted by the browser API (1 GB decimal).
///
/// The page upload path streams through a worker and browser storage. Complete
/// downloads and the legacy whole-buffer encryption binding remain memory-bound.
pub const MAX_BROWSER_FILE_BYTES: usize = 1_000_000_000;

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

/// Metadata for a content-addressed record staged outside WASM memory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserRecordInfo {
    /// Lowercase hexadecimal BLAKE3 record address.
    pub address: String,
    /// Raw record size in bytes.
    pub size: usize,
}

/// Result of streaming self-encryption whose record bytes live in browser storage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserStagedFile {
    /// Display filename supplied by the selected browser `File`.
    pub name: String,
    /// Browser MIME type, or `application/octet-stream` when none was supplied.
    pub content_type: String,
    /// Public DataMap record address.
    pub address: String,
    /// Whole-file plaintext BLAKE3 hash.
    pub blake3: String,
    /// Plaintext file size.
    pub size: usize,
    /// Serialized public DataMap size.
    pub data_map_size: usize,
    /// Native root DataMap chunk descriptors.
    pub chunks: Vec<BrowserChunkInfo>,
    /// Staged encrypted records followed by the public DataMap record.
    pub records: Vec<BrowserRecordInfo>,
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
    /// Input did not satisfy the browser API limits.
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
    let (published_data_map, encrypted_chunks) =
        self_encryption::encrypt(Bytes::copy_from_slice(content))
            .map_err(|error| BrowserError::SelfEncryption(error.to_string()))?;
    let root_data_map = {
        let encrypted_by_address = encrypted_chunks
            .iter()
            .map(|chunk| (*blake3::hash(&chunk.content).as_bytes(), &chunk.content))
            .collect::<std::collections::HashMap<_, _>>();
        let mut get_local_chunk = |address: self_encryption::XorName| {
            encrypted_by_address
                .get(&address.0)
                .map(|content| (*content).clone())
                .ok_or_else(|| {
                    self_encryption::Error::Generic(format!(
                        "self-encryption output omitted DataMap chunk {}",
                        hex::encode(address.0)
                    ))
                })
        };
        self_encryption::get_root_data_map(published_data_map.clone(), &mut get_local_chunk)
            .map_err(|error| BrowserError::SelfEncryption(error.to_string()))?
    };
    let chunks = chunk_infos(&root_data_map);

    let mut records: Vec<BrowserRecord> = encrypted_chunks
        .into_iter()
        .map(|chunk| BrowserRecord {
            address: content_address(&chunk.content),
            content: chunk.content.to_vec(),
        })
        .collect();
    let encoded_data_map = rmp_serde::to_vec(&published_data_map)
        .map_err(|error| BrowserError::DataMap(error.to_string()))?;
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
    let available = encrypted_contents
        .iter()
        .map(|content| *blake3::hash(content).as_bytes())
        .collect::<HashSet<_>>();
    for info in data_map.infos() {
        if !available.contains(&info.dst_hash.0) {
            return Err(BrowserError::Invalid(format!(
                "record set does not contain DataMap chunk {}; a record may be missing or corrupt",
                hex::encode(info.dst_hash.0)
            )));
        }
    }
    let encrypted_chunks = encrypted_contents
        .iter()
        .map(|content| EncryptedChunk {
            content: Bytes::copy_from_slice(content),
        })
        .collect::<Vec<_>>();
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
    use super::manifest::parse_browser_manifest;
    use super::payment::{payment_quote_hash, verify_storage_quote, BrowserQuoteArtifact};
    use super::protocol::{
        ice_password_from_sdp, parse_response_frame, parse_webrtc_direct_multiaddr,
        server_answer_sdp, v2_server_ice_credential, BrowserEndpointInput,
    };
    use super::{
        chunk_infos, content_address, decrypt_public_file, encrypt_public_file, verify_record,
        BrowserRecord, BrowserRecordInfo, BrowserStagedFile, MAX_BROWSER_FILE_BYTES,
    };
    use bytes::Bytes;
    use js_sys::{Array, Function, Promise, Uint8Array};
    use saorsa_dht_lookup::{
        run_iterative_lookup, IterativeLookup, LookupConfig, LookupKey, LookupNode, LookupQuery,
        LookupQueryOutcome,
    };
    use serde::{Deserialize, Serialize};
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;
    use std::rc::Rc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    #[derive(Debug, Serialize)]
    struct BrowserSessionDescription {
        #[serde(rename = "type")]
        description_type: &'static str,
        sdp: String,
    }

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
        webrtc_direct: Option<BrowserLookupEndpoint>,
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

    /// Shared Saorsa iterative lookup state driven by browser WebRtcDirect.
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

        /// Run the complete shared Saorsa walk through a WebRtcDirect batch callback.
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
        if let Some(endpoint) = candidate.wire.webrtc_direct.clone() {
            known_endpoints.insert(candidate.peer_id, endpoint);
        } else if let Some(endpoint) = known_endpoints.get(&candidate.peer_id) {
            candidate.wire.webrtc_direct = Some(endpoint.clone());
        }
        candidate.wire.webrtc_direct.as_ref()?;
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

    /// Validate and normalize a WebRTC Direct multiaddress in shared Rust.
    #[wasm_bindgen(js_name = parseWebRtcDirectMultiaddr)]
    pub fn parse_webrtc_direct_multiaddr_wasm(endpoint: JsValue) -> Result<JsValue, JsValue> {
        let input: BrowserEndpointInput = serde_wasm_bindgen::from_value(endpoint)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let parsed = parse_webrtc_direct_multiaddr(input.multiaddr())
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&parsed).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Decode and bound-check a complete WebRTC browser response frame.
    #[wasm_bindgen(js_name = parseResponseFrame)]
    pub fn parse_response_frame_wasm(frame: &[u8]) -> Result<JsValue, JsValue> {
        let parsed =
            parse_response_frame(frame).map_err(|error| JsValue::from_str(&error.to_string()))?;
        parsed
            .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Build the ICE-lite answer pinned by a WebRTC Direct endpoint.
    #[wasm_bindgen(js_name = serverAnswerFromEndpoint)]
    pub fn server_answer_from_endpoint_wasm(
        endpoint: JsValue,
        ice_credential: &str,
    ) -> Result<JsValue, JsValue> {
        let input: BrowserEndpointInput = serde_wasm_bindgen::from_value(endpoint)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let endpoint = parse_webrtc_direct_multiaddr(input.multiaddr())
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let sdp = server_answer_sdp(&endpoint, ice_credential)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&BrowserSessionDescription {
            description_type: "answer",
            sdp,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Derive the v2 server ufrag from an unchanged browser local description.
    #[wasm_bindgen(js_name = webRtcDirectV2ServerCredential)]
    pub fn web_rtc_direct_v2_server_credential_wasm(local_sdp: &str) -> Result<String, JsValue> {
        let password = ice_password_from_sdp(local_sdp)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        v2_server_ice_credential(&password).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Validate and normalize browser bootstrap and public-file metadata.
    #[wasm_bindgen(js_name = parseBrowserManifest)]
    pub fn parse_browser_manifest_wasm(value: JsValue) -> Result<JsValue, JsValue> {
        let value: serde_json::Value = serde_wasm_bindgen::from_value(value)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let manifest =
            parse_browser_manifest(value).map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&manifest)
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Compute the native EVM `PaymentQuote` hash.
    #[wasm_bindgen(js_name = paymentQuoteHash)]
    #[must_use]
    pub fn payment_quote_hash_wasm(
        signed_bytes: &[u8],
        public_key: &[u8],
        signature: &[u8],
    ) -> String {
        hex::encode(payment_quote_hash(signed_bytes, public_key, signature))
    }

    /// Fully verify a storage quote before exposing it to a wallet signer.
    #[wasm_bindgen(js_name = verifyStorageQuote)]
    pub fn verify_storage_quote_wasm(
        quote: JsValue,
        expected_address: &str,
        expected_peer_id: &str,
    ) -> Result<JsValue, JsValue> {
        let quote: BrowserQuoteArtifact = serde_wasm_bindgen::from_value(quote)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let verified = verify_storage_quote(quote, expected_address, expected_peer_id)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&verified)
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Native `self_encryption` plus public DataMap generation.
    #[wasm_bindgen(js_name = encryptPublicFile)]
    pub fn encrypt_public_file_wasm(content: &[u8]) -> Result<JsValue, JsValue> {
        let encrypted =
            encrypt_public_file(content).map_err(|error| JsValue::from_str(&error.to_string()))?;
        serde_wasm_bindgen::to_value(&encrypted)
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Incremental self-encryptor used from a worker with a synchronous file reader.
    ///
    /// Each call to `nextRecord` materializes at most one encrypted record. This
    /// lets JavaScript persist the record before asking WASM for the next one,
    /// keeping plaintext and ciphertext file-sized buffers out of the page.
    #[wasm_bindgen(js_name = BrowserFileEncryptor)]
    pub struct BrowserFileEncryptor {
        stream: self_encryption::EncryptionStream<Box<dyn Iterator<Item = Bytes>>>,
        file_size: usize,
        bytes_read: Rc<Cell<usize>>,
        read_error: Rc<RefCell<Option<String>>>,
        whole_file_hasher: Rc<RefCell<blake3::Hasher>>,
        data_map_records: HashMap<[u8; 32], Bytes>,
        records: Vec<BrowserRecordInfo>,
        data_map_record_yielded: bool,
    }

    #[wasm_bindgen(js_class = BrowserFileEncryptor)]
    impl BrowserFileEncryptor {
        /// Create an encryptor around a synchronous `(offset, length) => Uint8Array` reader.
        ///
        /// Browsers expose synchronous `File` reads only inside dedicated workers,
        /// so page code should construct this class there rather than on the UI thread.
        #[wasm_bindgen(constructor)]
        pub fn new(file_size: usize, read_chunk: Function) -> Result<Self, JsValue> {
            if file_size < self_encryption::MIN_ENCRYPTABLE_BYTES {
                return Err(JsValue::from_str(&format!(
                    "self-encryption requires at least {} bytes",
                    self_encryption::MIN_ENCRYPTABLE_BYTES
                )));
            }
            if file_size > MAX_BROWSER_FILE_BYTES {
                return Err(JsValue::from_str(&format!(
                    "browser files are limited to {MAX_BROWSER_FILE_BYTES} bytes"
                )));
            }

            let bytes_read = Rc::new(Cell::new(0usize));
            let iterator_bytes_read = Rc::clone(&bytes_read);
            let read_error = Rc::new(RefCell::new(None));
            let iterator_error = Rc::clone(&read_error);
            let whole_file_hasher = Rc::new(RefCell::new(blake3::Hasher::new()));
            let iterator_hasher = Rc::clone(&whole_file_hasher);
            let iterator = std::iter::from_fn(move || {
                if iterator_error.borrow().is_some() {
                    return None;
                }
                let offset = iterator_bytes_read.get();
                if offset >= file_size {
                    return None;
                }
                let length = (file_size - offset).min(self_encryption::MAX_CHUNK_SIZE);
                let returned = match read_chunk.call2(
                    &JsValue::NULL,
                    &JsValue::from_f64(offset as f64),
                    &JsValue::from_f64(length as f64),
                ) {
                    Ok(returned) => returned,
                    Err(error) => {
                        *iterator_error.borrow_mut() = Some(js_error_message(error));
                        return None;
                    }
                };
                if !returned.is_instance_of::<Uint8Array>() {
                    *iterator_error.borrow_mut() = Some(format!(
                        "file reader returned a non-Uint8Array at byte offset {offset}"
                    ));
                    return None;
                }
                let returned = Uint8Array::new(&returned);
                let actual = returned.length() as usize;
                if actual != length {
                    *iterator_error.borrow_mut() = Some(format!(
                        "file reader returned {actual} bytes at offset {offset}, expected {length}"
                    ));
                    return None;
                }
                let mut content = vec![0u8; actual];
                returned.copy_to(&mut content);
                iterator_hasher.borrow_mut().update(&content);
                iterator_bytes_read.set(offset + actual);
                Some(Bytes::from(content))
            });
            let stream = self_encryption::stream_encrypt(
                file_size,
                Box::new(iterator) as Box<dyn Iterator<Item = Bytes>>,
            )
            .map_err(|error| JsValue::from_str(&error.to_string()))?;

            Ok(Self {
                stream,
                file_size,
                bytes_read,
                read_error,
                whole_file_hasher,
                data_map_records: HashMap::new(),
                records: Vec::new(),
                data_map_record_yielded: false,
            })
        }

        /// Produce the next encrypted record, or `undefined` once all records are staged.
        #[wasm_bindgen(js_name = nextRecord)]
        pub fn next_record(&mut self) -> Result<JsValue, JsValue> {
            if self.data_map_record_yielded {
                return Ok(JsValue::UNDEFINED);
            }

            let next = self.stream.chunks().next();
            if let Some(error) = self.read_error.borrow().as_ref() {
                return Err(JsValue::from_str(error));
            }
            if let Some(result) = next {
                let (hash, content) = result.map_err(|error| {
                    JsValue::from_str(&format!("self-encryption failed: {error}"))
                })?;
                // Once `datamap()` becomes available, stream output consists only
                // of the small encrypted child DataMaps needed to resolve the root.
                if self.stream.datamap().is_some() {
                    self.data_map_records.insert(hash.0, content.clone());
                }
                return self.serialize_record(hex::encode(hash.0), content.to_vec());
            }

            let published_data_map = self.stream.datamap().ok_or_else(|| {
                JsValue::from_str("self-encryption ended before producing a DataMap")
            })?;
            let encoded = rmp_serde::to_vec(published_data_map).map_err(|error| {
                JsValue::from_str(&format!("DataMap serialization failed: {error}"))
            })?;
            let address = content_address(&encoded);
            self.data_map_record_yielded = true;
            self.serialize_record(address, encoded)
        }

        /// Return upload metadata after `nextRecord` has reached `undefined`.
        pub fn finish(&self, name: &str, content_type: &str) -> Result<JsValue, JsValue> {
            if !self.data_map_record_yielded {
                return Err(JsValue::from_str(
                    "all encrypted records must be staged before finishing",
                ));
            }
            if self.bytes_read.get() != self.file_size {
                return Err(JsValue::from_str(&format!(
                    "file reader supplied {} bytes, expected {}",
                    self.bytes_read.get(),
                    self.file_size
                )));
            }
            let published_data_map = self
                .stream
                .datamap()
                .ok_or_else(|| JsValue::from_str("self-encryption did not produce a DataMap"))?;
            let mut get_local_chunk = |address: self_encryption::XorName| {
                self.data_map_records
                    .get(&address.0)
                    .cloned()
                    .ok_or_else(|| {
                        self_encryption::Error::Generic(format!(
                            "streaming output omitted DataMap chunk {}",
                            hex::encode(address.0)
                        ))
                    })
            };
            let root_data_map = self_encryption::get_root_data_map(
                published_data_map.clone(),
                &mut get_local_chunk,
            )
            .map_err(|error| JsValue::from_str(&format!("self-encryption failed: {error}")))?;
            let public_record = self.records.last().ok_or_else(|| {
                JsValue::from_str("self-encryption omitted the public DataMap record")
            })?;
            let staged = BrowserStagedFile {
                name: name.to_string(),
                content_type: if content_type.is_empty() {
                    "application/octet-stream".to_string()
                } else {
                    content_type.to_string()
                },
                address: public_record.address.clone(),
                blake3: self
                    .whole_file_hasher
                    .borrow()
                    .clone()
                    .finalize()
                    .to_hex()
                    .to_string(),
                size: self.file_size,
                data_map_size: public_record.size,
                chunks: chunk_infos(&root_data_map),
                records: self.records.clone(),
            };
            serde_wasm_bindgen::to_value(&staged)
                .map_err(|error| JsValue::from_str(&error.to_string()))
        }
    }

    impl BrowserFileEncryptor {
        fn serialize_record(
            &mut self,
            address: String,
            content: Vec<u8>,
        ) -> Result<JsValue, JsValue> {
            self.records.push(BrowserRecordInfo {
                address: address.clone(),
                size: content.len(),
            });
            serde_wasm_bindgen::to_value(&BrowserRecord { address, content })
                .map_err(|error| JsValue::from_str(&error.to_string()))
        }
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

    #[test]
    fn nested_data_map_round_trip() {
        let size = 3 * self_encryption::MAX_CHUNK_SIZE + 1;
        let content = (0..size).map(|index| index as u8).collect::<Vec<_>>();
        let encrypted = encrypt_public_file(&content).expect("encrypt nested fixture");
        assert_eq!(encrypted.chunks.len(), 4);
        assert!(encrypted.records.len() > encrypted.chunks.len() + 1);

        let data_map = &encrypted.records.last().expect("DataMap record").content;
        let published: DataMap = rmp_serde::from_slice(data_map).expect("decode published map");
        assert!(published.is_child());

        let mut required_addresses = decode_public_data_map(data_map)
            .expect("decode child map")
            .into_iter()
            .map(|chunk| chunk.dst_hash)
            .collect::<HashSet<_>>();
        required_addresses.extend(encrypted.chunks.iter().map(|chunk| chunk.dst_hash.clone()));
        let records = encrypted.records[..encrypted.records.len() - 1]
            .iter()
            .filter(|record| required_addresses.contains(&record.address))
            .map(|record| record.content.clone())
            .collect::<Vec<_>>();
        assert_eq!(records.len(), encrypted.records.len() - 1);
        assert_eq!(
            decrypt_public_file(data_map, &records).expect("decrypt nested fixture"),
            content
        );
    }
}
