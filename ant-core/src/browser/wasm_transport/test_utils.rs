//! Mock node for generated-WASM tests; excluded from production bindings.
//! Only the WebRTC host is mocked: authentication, framing, quotes, and storage
//! all traverse the same ant-core implementation used by browser callers.

use super::*;
use base64::Engine;
use fips204::{
    ml_dsa_65,
    traits::{KeyGen, SerDes, Signer},
};
use saorsa_transport::webrtc::{
    accept_pq_session, encode_response_frame, parse_request_frame, BrowserResponse,
};

#[wasm_bindgen]
pub struct BrowserTestNode {
    public: Vec<u8>,
    secret: ml_dsa_65::PrivateKey,
    peer: [u8; 32],
    endpoint: String,
    session: Option<PqSession>,
    received: Vec<u8>,
    already_stored: bool,
    last_method: String,
    chunk: Vec<u8>,
    records: HashMap<String, Vec<u8>>,
    uploads_enabled: bool,
    invalid_quote: bool,
    committed_key_count: u32,
    last_put_address: String,
    last_put_quote_hash: String,
    closest_peers: Vec<BrowserNode>,
    put_error: Option<(String, String)>,
}

fn network() -> BrowserPaymentNetwork {
    BrowserPaymentNetwork {
        chain_id: 31337,
        payment_token_address: format!("0x{}", "11".repeat(20)),
        payment_vault_address: format!("0x{}", "22".repeat(20)),
    }
}

#[wasm_bindgen]
impl BrowserTestNode {
    #[wasm_bindgen(constructor)]
    pub fn new(seed: u8, already_stored: bool) -> Self {
        let (public, secret) = ml_dsa_65::KG::keygen_from_seed(&[seed; 32]);
        let public = public.into_bytes().to_vec();
        let peer = *blake3::hash(&public).as_bytes();
        let mut cert = vec![0x12, 0x20];
        cert.extend([seed; 32]);
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(cert);
        let endpoint = format!(
            "/ip4/127.0.0.1/udp/{}/webrtc-direct/certhash/u{encoded}/p2p/{}",
            24000 + seed as u16,
            hex::encode(peer)
        );
        Self {
            public,
            secret,
            peer,
            endpoint,
            session: None,
            received: Vec::new(),
            already_stored,
            last_method: String::new(),
            chunk: Vec::new(),
            records: HashMap::new(),
            uploads_enabled: true,
            invalid_quote: false,
            committed_key_count: 0,
            last_put_address: String::new(),
            last_put_quote_hash: String::new(),
            closest_peers: Vec::new(),
            put_error: None,
        }
    }
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }
    pub fn last_method(&self) -> String {
        self.last_method.clone()
    }
    pub fn set_uploads_enabled(&mut self, enabled: bool) {
        self.uploads_enabled = enabled;
    }
    pub fn set_invalid_quote(&mut self, invalid: bool) {
        self.invalid_quote = invalid;
    }
    pub fn set_chunk(&mut self, chunk: Vec<u8>) {
        self.chunk = chunk;
    }
    pub fn set_record(&mut self, address: String, content: Vec<u8>) {
        self.records.insert(address, content);
    }
    pub fn stored_record(&self, address: String) -> Vec<u8> {
        self.records.get(&address).cloned().unwrap_or_default()
    }
    pub fn set_committed_key_count(&mut self, count: u32) {
        self.committed_key_count = count;
    }
    pub fn set_closest_peers(&mut self, peers: JsValue) {
        self.closest_peers = serde_wasm_bindgen::from_value(peers).unwrap();
    }
    pub fn set_put_error(&mut self, code: String, message: String) {
        self.put_error = Some((code, message));
    }
    pub fn last_put_address(&self) -> String {
        self.last_put_address.clone()
    }
    pub fn last_put_quote_hash(&self) -> String {
        self.last_put_quote_hash.clone()
    }
    pub fn push(&mut self, message: &[u8]) -> Vec<u8> {
        self.received.extend_from_slice(message);
        let Some(expected) = pq_frame_length(&self.received, 8 * 1024 * 1024).unwrap() else {
            return Vec::new();
        };
        if self.received.len() < expected {
            return Vec::new();
        }
        let payload =
            decode_pq_frame(&std::mem::take(&mut self.received), 8 * 1024 * 1024).unwrap();
        if self.session.is_none() {
            let (accept, session) =
                accept_pq_session(&payload, &self.peer, &self.public, |transcript| {
                    self.secret
                        .try_sign_with_seed(&[7; 32], transcript, b"")
                        .map(|sig| sig.to_vec())
                })
                .unwrap();
            self.session = Some(session);
            self.last_method = "handshake".into();
            return encode_pq_frame(&accept).unwrap();
        }
        let plaintext = self.session.as_mut().unwrap().open(&payload).unwrap();
        let request = parse_request_frame(&plaintext).unwrap();
        let mut protocol_content = Vec::new();
        let body = match request.request.body {
            BrowserRequestBody::ChunkProtocol => {
                protocol_content = self.chunk_protocol(&request.content);
                BrowserResponseBody::ChunkProtocol
            }
            BrowserRequestBody::Hello => {
                self.last_method = "hello".into();
                BrowserResponseBody::Hello {
                    protocol: super::super::protocol::BROWSER_PROTOCOL_NAME.into(),
                    peer_id: hex::encode(self.peer),
                    max_chunk_size: MAX_BROWSER_RECORD_BYTES,
                    endpoint: BrowserEndpoint {
                        multiaddr: self.endpoint.clone(),
                    },
                    payment: network(),
                    capabilities: if self.uploads_enabled {
                        vec![
                            "chunk_protocol".into(),
                            "find_node".into(),
                            "get_chunk".into(),
                            "quote_chunk".into(),
                            "put_chunk".into(),
                        ]
                    } else {
                        vec![
                            "chunk_protocol".into(),
                            "find_node".into(),
                            "get_chunk".into(),
                        ]
                    },
                }
            }
            BrowserRequestBody::FindNode {
                target,
                count,
                with_address_records,
            } => {
                self.last_method = "find_node".into();
                let key = parse_lookup_key(&target, "target").unwrap();
                let mut nodes = self.closest_peers.clone();
                nodes.sort_by_key(|node| {
                    xor_distance(&parse_lookup_key(&node.peer_id, "peer").unwrap(), &key)
                });
                nodes.truncate(count.unwrap_or(20));
                if with_address_records {
                    for node in &mut nodes {
                        if let Some(encoded) = node.address_record.take() {
                            // Deliberately forward even bad proofs so tests exercise the client verifier.
                            let bytes = hex::decode(encoded).unwrap();
                            protocol_content.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
                            protocol_content.extend_from_slice(&bytes);
                        }
                    }
                }
                BrowserResponseBody::Nodes { target, nodes }
            }
            BrowserRequestBody::QuoteChunk { address, .. } => {
                self.last_method = "quote_chunk".into();
                let content: [u8; 32] = hex::decode(&address).unwrap().try_into().unwrap();
                let count = self.committed_key_count;
                let price = ant_protocol::payment::calculate_price(count as usize);
                let commitment = self.storage_commitment();
                let pin = commitment
                    .as_ref()
                    .and_then(ant_protocol::payment::commitment::commitment_hash);
                let timestamp = (js_sys::Date::now() / 1000.0) as u64;
                let signed = ant_protocol::evm::PaymentQuote::bytes_for_signing(
                    xor_name::XorName(content),
                    std::time::UNIX_EPOCH + Duration::from_secs(timestamp),
                    &price,
                    &ant_protocol::evm::RewardsAddress::from([0x44; 20]),
                    count,
                    &pin,
                );
                let signature = self
                    .secret
                    .try_sign_with_seed(&[8; 32], &signed, b"")
                    .unwrap();
                let hash =
                    super::super::payment::payment_quote_hash(&signed, &self.public, &signature);
                BrowserResponseBody::StorageQuote {
                    address: address.clone(),
                    already_stored: self.already_stored,
                    quote: BrowserQuoteArtifact {
                        peer_id: hex::encode(self.peer),
                        content: address,
                        timestamp_secs: timestamp,
                        price: price.to_string(),
                        rewards_address: hex::encode([0x44; 20]),
                        public_key: hex::encode(&self.public),
                        signature: hex::encode(signature),
                        committed_key_count: count,
                        commitment_pin: pin.map(hex::encode),
                        quote_hash: if self.invalid_quote {
                            "00".repeat(32)
                        } else {
                            hex::encode(hash)
                        },
                        commitment: commitment.map(|value| {
                            super::super::payment::BrowserCommitmentArtifact {
                                encoded: hex::encode(rmp_serde::to_vec(&value).unwrap()),
                                root: hex::encode(value.root),
                                key_count: value.key_count,
                                sender_peer_id: hex::encode(value.sender_peer_id),
                                sender_public_key: hex::encode(value.sender_public_key),
                                signature: hex::encode(value.signature),
                            }
                        }),
                    },
                }
            }
            BrowserRequestBody::PutChunk { address, quote, .. } => {
                self.last_method = "put_chunk".into();
                self.last_put_address.clone_from(&address);
                self.last_put_quote_hash = quote.quote_hash;
                match &self.put_error {
                    Some((code, message)) => BrowserResponseBody::Error {
                        code: code.clone(),
                        message: message.clone(),
                    },
                    None => {
                        self.records
                            .insert(address.clone(), request.content.to_vec());
                        BrowserResponseBody::ChunkStored {
                            address,
                            already_stored: false,
                        }
                    }
                }
            }
            BrowserRequestBody::GetChunk { address } => {
                self.last_method = "get_chunk".into();
                if let Some(content) = self.records.get(&address) {
                    self.chunk = content.clone();
                }
                if self.chunk.is_empty() {
                    BrowserResponseBody::ChunkNotFound { address }
                } else {
                    BrowserResponseBody::Chunk {
                        address,
                        size: self.chunk.len(),
                    }
                }
            }
        };
        let content = if !protocol_content.is_empty() {
            protocol_content.as_slice()
        } else if self.last_method == "get_chunk" {
            self.chunk.as_slice()
        } else {
            &[]
        };
        let response = match body {
            BrowserResponseBody::ChunkNotFound { address } => {
                BrowserResponse::not_found(request.request.request_id, address)
            }
            BrowserResponseBody::Error { code, message } => {
                BrowserResponse::error(request.request.request_id, code, message)
            }
            body => BrowserResponse::ok(request.request.request_id, body, content.len()),
        };
        let plaintext = encode_response_frame(&response, content).unwrap();
        let encrypted = self.session.as_mut().unwrap().seal(&plaintext).unwrap();
        encode_pq_frame(&encrypted).unwrap()
    }
}

/// Exercise classification after decoding an authenticated remote PUT error.
#[wasm_bindgen]
pub async fn test_put_failure_kind(endpoint: &str) -> Result<String, JsValue> {
    let endpoint = parse_webrtc_direct_multiaddr(endpoint)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    let client = BrowserNodeClientCore::new(endpoint);
    let content = b"structured PUT rejection fixture";
    let address = super::super::content_address(content);
    let (quote, _) = client
        .quote_chunk(&address, content.len())
        .await
        .map_err(|error| JsValue::from_str(&error))?;
    let result = client
        .put_chunk_typed(&address, content, quote, &"ab".repeat(32))
        .await;
    client.close();
    match result {
        Ok(_) => Ok("Success".into()),
        Err(error) => {
            let (timeouts, dial, remote) = match error.put_rejection() {
                PutRejection::Timeout => (1, 0, false),
                PutRejection::Dial => (0, 1, false),
                _ => (0, 0, true),
            };
            Ok(format!(
                "{:?}",
                crate::transfer_policy::put_shortfall(timeouts, dial, remote).failure_kind()
            ))
        }
    }
}

impl BrowserTestNode {
    fn storage_commitment(&self) -> Option<ant_protocol::payment::commitment::StorageCommitment> {
        if self.committed_key_count == 0 {
            return None;
        }
        let mut commitment = ant_protocol::payment::commitment::StorageCommitment {
            root: [0x53; 32],
            key_count: self.committed_key_count,
            sender_peer_id: self.peer,
            sender_public_key: self.public.clone(),
            signature: Vec::new(),
        };
        let payload = ant_protocol::payment::commitment::commitment_signed_payload(
            &commitment.root,
            commitment.key_count,
            &self.peer,
            &self.public,
        );
        commitment.signature = self
            .secret
            .try_sign_with_seed(
                &[9; 32],
                &payload,
                ant_protocol::payment::commitment::DOMAIN_COMMITMENT,
            )
            .unwrap()
            .to_vec();
        Some(commitment)
    }
}

/// Cancel a real RPC while retaining the owning client, then reuse the client.
#[wasm_bindgen]
pub async fn test_cancel_request(
    endpoint: &str,
    before_request: js_sys::Function,
) -> Result<(), JsValue> {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    client
        .hello()
        .await
        .map_err(|error| JsValue::from_str(&error))?;
    before_request.call0(&JsValue::NULL)?;
    let target = "11".repeat(32);
    let request = Box::pin(client.find_node(&target, 20));
    match select(request, Box::pin(TimeoutFuture::new(10))).await {
        Either::Right((_, pending)) => drop(pending),
        Either::Left(_) => return Err(JsValue::from_str("test RPC completed before cancellation")),
    }
    let result = client.find_node(&"22".repeat(32), 20).await;
    client.close();
    result
        .map(|_| ())
        .map_err(|error| JsValue::from_str(&error))
}

/// Cancel only a waiter for the request lock, leaving the active RPC intact.
#[wasm_bindgen]
pub async fn test_cancel_queued_request(endpoint: &str) -> Result<(), JsValue> {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    client
        .hello()
        .await
        .map_err(|error| JsValue::from_str(&error))?;
    let target = "11".repeat(32);
    let active = client.find_node(&target, 20);
    let waiting = async {
        TimeoutFuture::new(1).await;
        timeout_with_ms(client.find_node(&target, 20), "cancel waiting RPC", 10).await
    };
    let (result, canceled) = futures_util::future::join(active, waiting).await;
    assert_eq!(canceled.unwrap_err(), "cancel waiting RPC");
    result.map_err(|error| JsValue::from_str(&error))?;
    let result = client.find_node(&target, 20).await;
    client.close();
    result
        .map(|_| ())
        .map_err(|error| JsValue::from_str(&error))
}

impl BrowserTestNode {
    fn chunk_protocol(&mut self, bytes: &[u8]) -> Vec<u8> {
        use ant_protocol::evm::{PaymentQuote, RewardsAddress};
        use ant_protocol::{
            ChunkGetResponse, ChunkMessage, ChunkMessageBody as Body, ChunkPutResponse,
            ChunkQuoteResponse,
        };
        let request = ChunkMessage::decode(bytes).unwrap();
        let body = match request.body {
            Body::QuoteRequest(request) => {
                self.last_method = "quote_chunk".into();
                let commitment = self.storage_commitment();
                let pin = commitment
                    .as_ref()
                    .and_then(ant_protocol::payment::commitment::commitment_hash);
                let mut quote = PaymentQuote {
                    content: xor_name::XorName(request.address),
                    timestamp: std::time::UNIX_EPOCH
                        + Duration::from_secs((js_sys::Date::now() / 1000.0) as u64),
                    price: ant_protocol::payment::calculate_price(
                        self.committed_key_count as usize,
                    ),
                    rewards_address: RewardsAddress::from([0x44; 20]),
                    pub_key: self.public.clone(),
                    signature: Vec::new(),
                    committed_key_count: self.committed_key_count,
                    commitment_pin: pin,
                };
                quote.signature = self
                    .secret
                    .try_sign_with_seed(&[8; 32], &quote.bytes_for_sig(), b"")
                    .unwrap()
                    .to_vec();
                if self.invalid_quote {
                    quote.signature[0] ^= 1;
                }
                Body::QuoteResponse(ChunkQuoteResponse::Success {
                    quote: rmp_serde::to_vec(&quote).unwrap(),
                    already_stored: self.already_stored,
                    commitment: commitment.map(|value| rmp_serde::to_vec(&value).unwrap()),
                })
            }
            Body::MerkleCandidateQuoteRequest(request) => {
                self.last_method = "merkle_quote".into();
                use ant_protocol::evm::MerklePaymentCandidateNode;
                let commitment = self.storage_commitment();
                let pin = commitment
                    .as_ref()
                    .and_then(ant_protocol::payment::commitment::commitment_hash);
                let price =
                    ant_protocol::payment::calculate_price(self.committed_key_count as usize);
                let rewards = RewardsAddress::from([0x44; 20]);
                let signed = MerklePaymentCandidateNode::bytes_to_sign(
                    &price,
                    &rewards,
                    request.merkle_payment_timestamp,
                    self.committed_key_count,
                    &pin,
                );
                let candidate = MerklePaymentCandidateNode {
                    pub_key: self.public.clone(),
                    price,
                    reward_address: rewards,
                    merkle_payment_timestamp: request.merkle_payment_timestamp,
                    signature: self
                        .secret
                        .try_sign_with_seed(&[8; 32], &signed, b"")
                        .unwrap()
                        .to_vec(),
                    committed_key_count: self.committed_key_count,
                    commitment_pin: pin,
                };
                Body::MerkleCandidateQuoteResponse(
                    ant_protocol::MerkleCandidateQuoteResponse::Success {
                        candidate_node: rmp_serde::to_vec(&candidate).unwrap(),
                        commitment: commitment.map(|value| rmp_serde::to_vec(&value).unwrap()),
                    },
                )
            }
            Body::GetRequest(request) => {
                self.last_method = "get_chunk".into();
                let content = self
                    .records
                    .get(&hex::encode(request.address))
                    .cloned()
                    .unwrap_or_else(|| self.chunk.clone());
                Body::GetResponse(if content.is_empty() {
                    ChunkGetResponse::NotFound {
                        address: request.address,
                    }
                } else {
                    ChunkGetResponse::Success {
                        address: request.address,
                        content,
                    }
                })
            }
            Body::PutRequest(request) => {
                self.last_method = "put_chunk".into();
                self.last_put_address = hex::encode(request.address);
                if let Ok(proof) = ant_protocol::payment::deserialize_merkle_proof(
                    request.payment_proof.as_deref().unwrap(),
                ) {
                    assert!(proof.data_proof.verify());
                    assert_eq!(proof.address.0, request.address);
                    self.last_put_quote_hash = hex::encode(proof.winner_pool.hash());
                } else {
                    let (proof, _) = ant_protocol::payment::deserialize_proof(
                        request.payment_proof.as_deref().unwrap(),
                    )
                    .unwrap();
                    let mut quotes = proof
                        .peer_quotes
                        .iter()
                        .map(|(_, quote)| quote)
                        .collect::<Vec<_>>();
                    quotes.sort_by_key(|quote| quote.price);
                    self.last_put_quote_hash = hex::encode(quotes[quotes.len() / 2].hash());
                }
                Body::PutResponse(match &self.put_error {
                    Some((code, message)) => ChunkPutResponse::Error(match code.as_str() {
                        "storage_full" => {
                            ant_protocol::ProtocolError::StorageFailed(message.clone())
                        }
                        "price_floor" => {
                            ant_protocol::ProtocolError::PaymentFailed(message.clone())
                        }
                        _ => ant_protocol::ProtocolError::StorageFailed(message.clone()),
                    }),
                    None => {
                        assert_eq!(
                            ant_protocol::compute_address(&request.content),
                            request.address
                        );
                        self.records
                            .insert(self.last_put_address.clone(), request.content.to_vec());
                        ChunkPutResponse::Success {
                            address: request.address,
                        }
                    }
                })
            }
            other => panic!("unsupported mock request: {other:?}"),
        };
        ChunkMessage {
            request_id: request.request_id,
            body,
        }
        .encode()
        .unwrap()
    }
}

/// Exercise core identity and the platform timer from generated WASM.
#[wasm_bindgen]
pub async fn test_shared_identity_and_timers() {
    let identity = ant_protocol::transport::NodeIdentity::generate().unwrap();
    let imported = ant_protocol::transport::NodeIdentity::import(&identity.export()).unwrap();
    let signature = imported.sign(b"shared native and wasm identity").unwrap();
    assert!(identity
        .verify(b"shared native and wasm identity", &signature)
        .unwrap());
    assert!(!identity.verify(b"tampered", &signature).unwrap());
    let started = js_sys::Date::now();
    crate::runtime::sleep(Duration::from_millis(5)).await;
    assert!(js_sys::Date::now() >= started + 4.0);
    assert!(
        crate::runtime::timeout(Duration::from_millis(2), std::future::pending::<()>())
            .await
            .is_err()
    );
    assert_eq!(
        crate::runtime::timeout(Duration::from_secs(1), async { 42 })
            .await
            .unwrap(),
        42
    );
}

/// Build a real owner proof for browser forwarding and freshness regressions.
#[wasm_bindgen]
pub fn test_signed_address_node(age_seconds: u32) -> JsValue {
    use ant_protocol::transport::signed_address::SignedAddressRecord;
    use ant_protocol::transport::{KnownReachability, NodeIdentity, TransportAddressRecord};
    let identity = NodeIdentity::from_seed(&[255; 32]).unwrap();
    let issued = (js_sys::Date::now() / 1000.0) as u64 - u64::from(age_seconds);
    let address = "/ip4/9.9.9.9/udp/9000/quic".parse().unwrap();
    let record = TransportAddressRecord::from_multiaddr(&address, KnownReachability::Direct)
        .unwrap()
        .unwrap();
    let signed = SignedAddressRecord::sign(&identity, 10, issued, vec![record]).unwrap();
    let node = shared::browser_record(signed.verify(issued).unwrap().peer_record(1.0)).unwrap();
    serde_wasm_bindgen::to_value(&node).unwrap()
}
