//! Mock node for generated-WASM tests; excluded from production bindings.
//! Only the WebRTC host is mocked: authentication, framing, quotes, and storage
//! all traverse the same ant-core implementation used by browser callers.

use super::*;
use ant_protocol::chunk::{PointerGetResponse, PointerPutResponse};
use ant_protocol::pointer::Pointer;
use base64::Engine;
use fips204::{
    ml_dsa_65,
    traits::{KeyGen, SerDes, Signer},
};
use saorsa_transport::webrtc::{
    accept_pq_session, encode_response_frame, parse_request_frame, BrowserResponse,
};

/// Run independent peer reads through the real adapter's physical GET gate.
#[wasm_bindgen]
pub async fn test_budgeted_reads(endpoints: JsValue, close_early: bool) -> JsValue {
    use crate::data::network::BrowserNetwork;
    let endpoints: Vec<String> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let core = Rc::new(
        BrowserNetworkCore::new(
            endpoints
                .iter()
                .map(|endpoint| BrowserEndpoint {
                    multiaddr: endpoint.clone(),
                })
                .collect(),
        )
        .unwrap(),
    );
    let adapter = SharedNetworkAdapter::new(Rc::clone(&core));
    let requests = endpoints.iter().map(|endpoint| {
        let adapter = &adapter;
        async move {
            let parsed = parse_webrtc_direct_multiaddr(endpoint).unwrap();
            let peer = PeerId::from_hex(&parsed.peer_id).unwrap();
            adapter
                .request(
                    &peer,
                    &[endpoint.parse().unwrap()],
                    ant_protocol::ChunkMessage {
                        request_id: 1,
                        body: ant_protocol::ChunkMessageBody::GetRequest(
                            ant_protocol::ChunkGetRequest::new([1; 32]),
                        ),
                    },
                    Duration::from_secs(1),
                )
                .await
                .is_ok()
        }
    });
    let close = async {
        if close_early {
            crate::runtime::sleep(Duration::from_millis(50)).await;
            core.pool.close();
        }
    };
    let (results, ()) = futures::future::join(futures::future::join_all(requests), close).await;
    core.pool.close();
    serde_wasm_bindgen::to_value(&results).unwrap()
}

/// Exercise independent RPC lanes, channel-local cancellation and reuse of the
/// shared association. Each lane authenticates its own PQ session and HELLO.
#[wasm_bindgen]
pub async fn test_parallel_lanes(endpoint: &str, cancel_lane: &str) -> JsValue {
    let pool = BrowserClientPool::new(1).unwrap();
    let endpoint = BrowserEndpoint {
        multiaddr: endpoint.into(),
    };
    let control = pool.client(&endpoint).await.unwrap();
    if cancel_lane != "cold" {
        control.hello().await.unwrap();
    }
    let data = pool
        .data_client_before(&endpoint, TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
        .await
        .unwrap();
    if cancel_lane != "cold" {
        data.hello().await.unwrap();
    }
    let start = web_time::Instant::now();
    let get = || async {
        data.authenticated()
            .await
            .map_err(|e| e.to_string())?
            .request(
                BrowserRequestBody::GetChunk {
                    address: "11".repeat(32),
                },
                &[],
            )
            .await
            .map(|_| ())
    };
    let first_control = async {
        let target = "11".repeat(32);
        let call = control.find_node(&target, 20);
        let result = if cancel_lane == "control" {
            crate::runtime::timeout(Duration::from_millis(20), call)
                .await
                .map_err(|_| "cancelled".to_string())
                .and_then(|value| value)
        } else {
            call.await
        };
        serde_json::json!({ "ms": start.elapsed().as_millis() as u64, "result": result.map(|_| "ok".to_string()).unwrap_or_else(|e| e) })
    };
    let first_data = async {
        let result = if cancel_lane == "data" {
            crate::runtime::timeout(Duration::from_millis(20), get())
                .await
                .map_err(|_| "cancelled".to_string())
                .and_then(|value| value)
        } else {
            get().await
        };
        serde_json::json!({ "ms": start.elapsed().as_millis() as u64, "result": result.map(|_| "ok".to_string()).unwrap_or_else(|e| e) })
    };
    let closer = async {
        if cancel_lane == "close" {
            crate::runtime::sleep(Duration::from_millis(20)).await;
            pool.close();
        }
    };
    let ((control_result, data_result), ()) =
        futures::future::join(futures::future::join(first_control, first_data), closer).await;
    if cancel_lane != "close" {
        control.find_node(&"22".repeat(32), 20).await.unwrap();
        get().await.unwrap();
    }
    pool.close();
    serde_wasm_bindgen::to_value(
        &serde_json::json!({ "control": control_result, "data": data_result }),
    )
    .unwrap()
}

#[wasm_bindgen]
pub async fn test_active_data_lane_capacity(endpoints: JsValue) {
    let endpoints: Vec<String> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let pool = BrowserClientPool::new(1).unwrap();
    let first = BrowserEndpoint {
        multiaddr: endpoints[0].clone(),
    };
    let second = BrowserEndpoint {
        multiaddr: endpoints[1].clone(),
    };
    let data = pool
        .data_client_before(&first, TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
        .await
        .unwrap();
    data.hello().await.unwrap();
    assert!(pool
        .client_before(&second, TransferDeadline::new(Duration::from_millis(20)))
        .await
        .is_err());
    data.hello().await.unwrap();
    drop(data);
    pool.client(&second).await.unwrap().hello().await.unwrap();
    pool.close();
}

/// Abandon an admitted lookup exactly as the iterative lookup grace timer does.
/// A subsequent lookup must reuse the drained session or the actual dial error.
#[wasm_bindgen]
pub async fn test_abandoned_lookup(endpoint: &str, close_pool: bool) -> String {
    let pool = BrowserClientPool::new(1).unwrap();
    let endpoint = BrowserEndpoint {
        multiaddr: endpoint.into(),
    };
    let lease = pool.client(&endpoint).await.unwrap();
    assert!(
        crate::runtime::timeout(Duration::from_millis(20), lease.lookup("11".repeat(32), 20),)
            .await
            .is_err()
    );
    if close_pool {
        pool.close();
    }
    crate::runtime::sleep(Duration::from_millis(150)).await;
    let result = async {
        pool.client(&endpoint)
            .await?
            .lookup("22".repeat(32), 20)
            .await
    }
    .await;
    pool.close();
    result
        .map(|_| "ok".to_string())
        .unwrap_or_else(|error| error)
}

/// Abandon a lookup that has not acquired the peer lock. It must send no RPC.
#[wasm_bindgen]
pub async fn test_abandoned_queued_lookup(endpoint: &str) {
    let pool = BrowserClientPool::new(1).unwrap();
    let endpoint = BrowserEndpoint {
        multiaddr: endpoint.into(),
    };
    let active = pool.client(&endpoint).await.unwrap();
    active.hello().await.unwrap();
    let queued = pool.client(&endpoint).await.unwrap();
    let target = "11".repeat(32);
    let first = active.find_node(&target, 20);
    let second = async {
        crate::runtime::sleep(Duration::from_millis(1)).await;
        assert!(crate::runtime::timeout(
            Duration::from_millis(20),
            queued.lookup("22".repeat(32), 20),
        )
        .await
        .is_err());
    };
    let (result, ()) = futures::future::join(first, second).await;
    result.unwrap();
    crate::runtime::sleep(Duration::from_millis(20)).await;
    pool.close();
}

/// Exercise scheduling with nodes at the already-verified preconnect boundary.
/// Wire-proof verification is separately exercised through the real FIND_NODE
/// decoder; this fixture supplies only the trusted marker needed by the pool.
#[wasm_bindgen]
pub async fn test_preconnect_pool(
    endpoints: JsValue,
    signed: bool,
    close_early: bool,
    capacity: usize,
) {
    let endpoints: Vec<String> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let nodes = endpoints
        .into_iter()
        .map(|multiaddr| {
            let peer_id = parse_webrtc_direct_multiaddr(&multiaddr).unwrap().peer_id;
            BrowserNode {
                peer_id,
                address_record: signed.then(|| "verified upstream".to_string()),
                peer_record: None,
                native_addresses: vec![],
                reliability: 1.0,
                webrtc_direct: Some(BrowserEndpoint { multiaddr }),
            }
        })
        .collect::<Vec<_>>();
    let pool = Rc::new(BrowserClientPool::new(capacity).unwrap());
    pool.preconnect(&nodes);
    pool.preconnect(&nodes);
    crate::runtime::sleep(Duration::from_millis(20)).await;
    if close_early {
        pool.close();
    }
    crate::runtime::timeout(Duration::from_secs(2), async {
        while !pool.preconnecting.borrow().is_empty() {
            crate::runtime::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert!(
        pool.preconnect_errors.borrow().is_empty(),
        "{:?}",
        pool.preconnect_errors.borrow()
    );
    pool.close();
}

#[wasm_bindgen]
pub struct BrowserTestNode {
    public: Vec<u8>,
    secret: ml_dsa_65::PrivateKey,
    peer: [u8; 32],
    endpoint: String,
    session: Option<PqSession>,
    hello_received: bool,
    received: Vec<u8>,
    already_stored: bool,
    last_method: String,
    chunk: Vec<u8>,
    records: HashMap<String, Vec<u8>>,
    uploads_enabled: bool,
    address_v2: bool,
    multiplex: bool,
    invalid_quote: bool,
    committed_key_count: u32,
    last_put_address: String,
    last_put_quote_hash: String,
    closest_peers: Vec<BrowserNode>,
    put_error: Option<(String, String)>,
    pointers_enabled: bool,
}

/// Where the mock keeps a pointer, beside chunks in the same record map.
fn pointer_key(address: &[u8; 32]) -> String {
    format!("pointer:{}", hex::encode(address))
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
            hello_received: false,
            received: Vec::new(),
            already_stored,
            last_method: String::new(),
            chunk: Vec::new(),
            records: HashMap::new(),
            uploads_enabled: true,
            address_v2: false,
            multiplex: false,
            invalid_quote: false,
            committed_key_count: 0,
            last_put_address: String::new(),
            last_put_quote_hash: String::new(),
            closest_peers: Vec::new(),
            put_error: None,
            pointers_enabled: true,
        }
    }
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }
    pub fn last_method(&self) -> String {
        self.last_method.clone()
    }
    pub fn set_multiplex(&mut self, enabled: bool) {
        self.multiplex = enabled;
    }
    pub fn seal_response(&mut self, plaintext: &[u8]) -> Vec<u8> {
        encode_pq_frame(&self.session.as_mut().unwrap().seal(plaintext).unwrap()).unwrap()
    }
    pub fn set_address_v2(&mut self, enabled: bool) {
        self.address_v2 = enabled;
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
    pub fn set_pointers_enabled(&mut self, enabled: bool) {
        self.pointers_enabled = enabled;
    }
    pub fn set_put_error(&mut self, code: String, message: String) {
        self.put_error = (!code.is_empty()).then_some((code, message));
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
        if !self.hello_received && !matches!(&request.request.body, BrowserRequestBody::Hello) {
            let response = BrowserResponse::error(
                request.request.request_id,
                "authentication_required",
                "HELLO must be the first request",
            );
            let plaintext = encode_response_frame(&response, &[]).unwrap();
            let encrypted = self.session.as_mut().unwrap().seal(&plaintext).unwrap();
            return encode_pq_frame(&encrypted).unwrap();
        }
        let mut protocol_content = Vec::new();
        let body = match request.request.body {
            BrowserRequestBody::ChunkProtocol => {
                protocol_content = self.chunk_protocol(&request.content);
                BrowserResponseBody::ChunkProtocol
            }
            BrowserRequestBody::Hello => {
                self.hello_received = true;
                self.last_method = "hello".into();
                BrowserResponseBody::Hello {
                    protocol: super::super::protocol::BROWSER_PROTOCOL_NAME.into(),
                    peer_id: hex::encode(self.peer),
                    max_chunk_size: MAX_BROWSER_RECORD_BYTES,
                    endpoint: BrowserEndpoint {
                        multiaddr: self.endpoint.clone(),
                    },
                    payment: network(),
                    capabilities: {
                        let mut capabilities = vec![
                            "chunk_protocol".into(),
                            "find_node".into(),
                            "get_chunk".into(),
                        ];
                        if self.multiplex {
                            capabilities.push(multiplex::CAPABILITY.into());
                        }
                        if self.uploads_enabled {
                            capabilities.extend(["quote_chunk".into(), "put_chunk".into()]);
                        }
                        if self.address_v2 {
                            capabilities
                                .push(ant_protocol::transport::ADDRESS_V2_CAPABILITY.into());
                        }
                        if self.pointers_enabled {
                            capabilities.push(POINTER_PROTOCOL_CAPABILITY.into());
                        }
                        capabilities
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
        if self.multiplex {
            return plaintext;
        }
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
    client.hello().await.map_err(|e| JsValue::from_str(&e))?;
    let content = b"structured PUT rejection fixture";
    let address = super::super::content_address(content);
    let (quote, _) = client
        .authenticated()
        .await
        .unwrap()
        .quote_chunk(&address, content.len())
        .await
        .map_err(|error| JsValue::from_str(&error))?;
    let result = client
        .authenticated()
        .await
        .unwrap()
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
        let request_body = match request.body {
            Body::QuoteRequestV2(r) => Body::QuoteRequest(ant_protocol::ChunkQuoteRequest {
                address: r.address,
                data_size: r.data_size,
                data_type: r.data_type,
            }),
            Body::MerkleCandidateQuoteRequestV2(r) => {
                Body::MerkleCandidateQuoteRequest(ant_protocol::MerkleCandidateQuoteRequest {
                    address: r.address,
                    data_size: r.data_size,
                    data_type: r.data_type,
                    merkle_payment_timestamp: r.merkle_payment_timestamp,
                })
            }
            body => body,
        };
        let body = match request_body {
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
            Body::PointerPutRequest(request) => {
                assert!(
                    self.pointers_enabled,
                    "pointer PUT sent to a node without them"
                );
                self.last_method = "put_pointer".into();
                let record = Pointer::from_bytes(&request.record).unwrap();
                let address = record.address();
                let state_id = record.state_id();
                self.last_put_address = hex::encode(address);
                let (proof, _) = ant_protocol::payment::deserialize_proof(
                    request.payment_proof.as_deref().unwrap(),
                )
                .unwrap();
                // Paid at the state, never the address.
                assert!(proof
                    .peer_quotes
                    .iter()
                    .all(|(_, quote)| quote.content.0 == state_id));
                let mut quotes = proof
                    .peer_quotes
                    .iter()
                    .map(|(_, quote)| quote)
                    .collect::<Vec<_>>();
                quotes.sort_by_key(|quote| quote.price);
                self.last_put_quote_hash = hex::encode(quotes[quotes.len() / 2].hash());
                let key = pointer_key(&address);
                let held = self
                    .records
                    .get(&key)
                    .and_then(|bytes| Pointer::from_bytes(bytes).ok());
                Body::PointerPutResponse(match held {
                    Some(held) if held.state_id() == state_id => {
                        PointerPutResponse::Unchanged { address, state_id }
                    }
                    Some(held) if !record.replaces(&held) => PointerPutResponse::Stale {
                        address,
                        state_id: held.state_id(),
                    },
                    _ => {
                        self.records.insert(key, request.record.to_vec());
                        PointerPutResponse::Success { address, state_id }
                    }
                })
            }
            Body::PointerGetRequest(request) => {
                assert!(
                    self.pointers_enabled,
                    "pointer GET sent to a node without them"
                );
                self.last_method = "get_pointer".into();
                Body::PointerGetResponse(match self.records.get(&pointer_key(&request.address)) {
                    Some(record) => PointerGetResponse::Success {
                        record: record.clone().into(),
                    },
                    None => PointerGetResponse::NotFound {
                        address: request.address,
                    },
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

/// Build a real, non-expiring owner proof for browser forwarding regressions.
#[wasm_bindgen]
pub fn test_signed_address_node() -> JsValue {
    use ant_protocol::transport::signed_address::SignedAddressRecord;
    use ant_protocol::transport::{KnownReachability, NodeIdentity, TransportAddressRecord};
    let identity = NodeIdentity::from_seed(&[255; 32]).unwrap();
    let address = "/ip4/9.9.9.9/udp/9000/quic".parse().unwrap();
    let record = TransportAddressRecord::from_multiaddr(&address, KnownReachability::Direct)
        .unwrap()
        .unwrap();
    let signed = SignedAddressRecord::sign(&identity, 10, vec![record]).unwrap();
    let node = shared::browser_record(signed.verify().unwrap().peer_record(1.0)).unwrap();
    serde_wasm_bindgen::to_value(&node).unwrap()
}

/// Exercise capacity release, cancellation, and close with multiple pool waiters.
#[wasm_bindgen]
pub async fn test_pool_waiters(endpoints: JsValue) -> Result<(), JsValue> {
    let endpoints: Vec<BrowserEndpoint> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let pool = BrowserClientPool::new(2).unwrap();
    let first = pool.client(&endpoints[0]).await.unwrap();
    let second = pool.client(&endpoints[1]).await.unwrap();
    let mut waiting_a = Box::pin(pool.client(&endpoints[2]));
    let mut waiting_b = Box::pin(pool.client(&endpoints[3]));
    assert!(futures::poll!(&mut waiting_a).is_pending());
    assert!(futures::poll!(&mut waiting_b).is_pending());
    drop(first);
    drop(second);
    let (a, b) = timeout_with_ms(
        async { Ok(futures::future::join(waiting_a, waiting_b).await) },
        "pool did not wake both waiters",
        1000,
    )
    .await
    .map_err(|error| JsValue::from_str(&error))?;
    let (a, b) = (a.unwrap(), b.unwrap());
    let mut cancelled = Box::pin(pool.client(&endpoints[0]));
    assert!(futures::poll!(&mut cancelled).is_pending());
    drop(cancelled);
    let mut waiting = Box::pin(pool.client(&endpoints[1]));
    assert!(futures::poll!(&mut waiting).is_pending());
    pool.close();
    assert!(matches!(waiting.await, Err(error) if error.contains("closed")));
    drop((a, b));
    Ok(())
}

/// Send through the production adapter with the native operation's response timeout.
#[wasm_bindgen]
pub async fn test_operation_timeout(endpoint: &str, timeout_ms: u32) -> Result<(), JsValue> {
    use crate::data::network::BrowserNetwork;
    use ant_protocol::{ChunkGetRequest, ChunkMessage, ChunkMessageBody};
    let parsed = parse_webrtc_direct_multiaddr(endpoint).unwrap();
    let endpoint = BrowserEndpoint {
        multiaddr: parsed.multiaddr.clone(),
    };
    let core = Rc::new(BrowserNetworkCore::new(vec![endpoint]).unwrap());
    let adapter = shared::SharedNetworkAdapter::new(Rc::clone(&core));
    let peer = ant_protocol::transport::PeerId::from_hex(&parsed.peer_id).unwrap();
    let addresses = vec![parsed.multiaddr.parse().unwrap()];
    let result = adapter
        .request(
            &peer,
            &addresses,
            ChunkMessage {
                request_id: 42,
                body: ChunkMessageBody::GetRequest(ChunkGetRequest::new([1; 32])),
            },
            Duration::from_millis(u64::from(timeout_ms)),
        )
        .await;
    core.pool.close();
    result
        .map(|_| ())
        .map_err(|error| JsValue::from_str(&error.to_string()))
}

#[wasm_bindgen]
impl BrowserNetworkClient {
    /// Simulate a refusal corroborated by other concurrent quote requests.
    pub fn test_refuse_settlement(&self) {
        for seed in 1..=crate::data::client::SETTLEMENT_REFUSAL_QUORUM {
            self.shared.note_settlement_refusal(
                ant_protocol::transport::PeerId::from_bytes([seed as u8; 32]),
                "test settlement refusal: update required",
            );
        }
    }
}

/// Exercise concurrent requests through the production pooled adapter.
#[wasm_bindgen]
pub async fn test_pooled_requests(
    endpoint: &str,
    timeout_ms: u32,
    warm: bool,
    payment: JsValue,
    before: js_sys::Function,
) -> Result<JsValue, JsValue> {
    use crate::data::network::BrowserNetwork;
    use ant_protocol::{ChunkGetRequest, ChunkMessage, ChunkMessageBody};
    let parsed = parse_webrtc_direct_multiaddr(endpoint).unwrap();
    let endpoint = BrowserEndpoint {
        multiaddr: parsed.multiaddr.clone(),
    };
    let core = Rc::new(BrowserNetworkCore::new(vec![endpoint.clone()]).unwrap());
    let mut adapter = shared::SharedNetworkAdapter::new(Rc::clone(&core));
    let upload = !payment.is_undefined();
    if upload {
        adapter.payment_network = Some(serde_wasm_bindgen::from_value(payment).unwrap());
    }
    let peer = ant_protocol::transport::PeerId::from_hex(&parsed.peer_id).unwrap();
    let addresses = vec![parsed.multiaddr.parse().unwrap()];
    if warm {
        let lease = if upload {
            core.pool.client(&endpoint).await.unwrap()
        } else {
            core.pool
                .data_client_before(&endpoint, TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
                .await
                .unwrap()
        };
        lease.hello().await.unwrap();
    }
    before.call0(&JsValue::NULL)?;
    let started = web_time::Instant::now();
    let requests = [0_u32, 0, timeout_ms / 10].into_iter().enumerate().map(|(i, delay)| {
        let adapter = &adapter;
        let peer = &peer;
        let addresses = &addresses;
        async move {
            if delay > 0 { crate::runtime::sleep(Duration::from_millis(u64::from(delay))).await; }
            let result = adapter.request(peer, addresses, ChunkMessage {
                request_id: i as u64 + 42,
                body: if upload {
                    ChunkMessageBody::QuoteRequest(ant_protocol::ChunkQuoteRequest {
                        address: [1; 32], data_size: 100, data_type: 0,
                    })
                } else { ChunkMessageBody::GetRequest(ChunkGetRequest::new([1; 32])) },
            }, Duration::from_millis(u64::from(timeout_ms))).await;
            serde_json::json!({ "request": i, "startedMs": delay, "finishedMs": started.elapsed().as_millis() as u64,
                "result": result.map(|_| "ok".to_string()).unwrap_or_else(|error| error.to_string()) })
        }
    });
    let result = futures::future::join_all(requests).await;
    core.pool.close();
    serde_wasm_bindgen::to_value(&result).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// A local admission timeout must not cancel the RPC holding the peer lock.
#[wasm_bindgen]
pub async fn test_admission_deadlines(endpoints: JsValue) {
    let endpoints: Vec<BrowserEndpoint> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let pool = BrowserClientPool::new(1).unwrap();
    let lease = pool.client(&endpoints[0]).await.unwrap();
    lease.hello().await.unwrap();
    let target = "11".repeat(32);
    let active = lease.find_node(&target, 20);
    let waiter = async {
        TimeoutFuture::new(1).await;
        let deadline = TransferDeadline::new(Duration::from_millis(20));
        assert!(matches!(lease.client.authenticated_before(&deadline).await,
            Err(RpcError::Timeout(message)) if message.contains("admission")));
    };
    let (result, ()) = futures::future::join(active, waiter).await;
    result.unwrap();
    assert!(lease.is_connected());
    let deadline = TransferDeadline::new(Duration::from_millis(20));
    assert!(matches!(pool.client_before(&endpoints[1], deadline).await,
        Err(RpcError::Timeout(message)) if message.contains("pool capacity")));
    lease.find_node(&target, 20).await.unwrap();
    pool.close();
}

/// Closing the pool during an outstanding browser setup must be terminal.
#[wasm_bindgen]
pub async fn test_close_pool_during_connect(endpoint: &str) {
    let pool = BrowserClientPool::new(1).unwrap();
    let lease = pool
        .client(&BrowserEndpoint {
            multiaddr: endpoint.into(),
        })
        .await
        .unwrap();
    let connect = lease.hello();
    let close = async {
        TimeoutFuture::new(5).await;
        pool.close();
    };
    let (result, ()) = futures::future::join(connect, close).await;
    assert!(result.unwrap_err().contains("closed"));
    assert!(!lease.is_connected());
    assert!(lease.hello().await.unwrap_err().contains("closed"));
}

/// Drive more callers than one channel admits, with optional cancellation.
#[wasm_bindgen]
pub async fn test_multiplex_requests(endpoint: &str, count: usize, cancel: bool) -> JsValue {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    client.hello().await.unwrap();
    let started = web_time::Instant::now();
    let results = futures::future::join_all((0..count).map(|index| {
        let client = &client;
        async move {
            let call = async {
                client
                    .authenticated()
                    .await
                    .map_err(|e| e.to_string())?
                    .request(
                        BrowserRequestBody::GetChunk {
                            address: format!("{index:064x}"),
                        },
                        &[],
                    )
                    .await
            };
            let result = if cancel && index == 0 {
                crate::runtime::timeout(Duration::from_millis(20), call)
                    .await
                    .map_err(|_| "cancelled".to_string())
                    .and_then(|r| r)
            } else {
                call.await
            };
            serde_json::json!({"index": index, "ms": started.elapsed().as_millis() as u64,
                "result": result.map(|_| "ok".to_string()).unwrap_or_else(|e| e)})
        }
    }))
    .await;
    // Allow the deliberately abandoned reply to drain before testing reuse.
    crate::runtime::sleep(Duration::from_millis(220)).await;
    let reuse = client.find_node(&"11".repeat(32), 20).await;
    client.close();
    serde_wasm_bindgen::to_value(&serde_json::json!({"results": results, "reuse": reuse.is_ok()}))
        .unwrap()
}

#[wasm_bindgen]
pub async fn test_cancelled_read_reservation(endpoint: &str) {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    client.hello().await.unwrap();
    let budget = crate::client_engine::read_budget::ReadBudget::new(1, 1);
    let permit = budget.acquire(|| 1).await.unwrap();
    let authenticated = client.authenticated().await.unwrap();
    let request = authenticated.request_reserved(
        BrowserRequestBody::GetChunk {
            address: "11".repeat(32),
        },
        &[],
        Duration::from_secs(1),
        Some(permit),
        false,
    );
    assert!(crate::runtime::timeout(Duration::from_millis(20), request)
        .await
        .is_err());
    assert!(
        crate::runtime::timeout(Duration::from_millis(20), budget.acquire(|| 1))
            .await
            .is_err()
    );
    crate::runtime::sleep(Duration::from_millis(120)).await;
    let permit = crate::runtime::timeout(Duration::from_millis(20), budget.acquire(|| 1))
        .await
        .unwrap()
        .unwrap();
    let (_, retained) = client
        .authenticated()
        .await
        .unwrap()
        .request_reserved(
            BrowserRequestBody::GetChunk {
                address: "22".repeat(32),
            },
            &[],
            Duration::from_secs(1),
            Some(permit),
            false,
        )
        .await
        .unwrap();
    assert!(
        crate::runtime::timeout(Duration::from_millis(20), budget.acquire(|| 1))
            .await
            .is_err()
    );
    drop(retained);
    assert!(
        crate::runtime::timeout(Duration::from_millis(20), budget.acquire(|| 1))
            .await
            .is_ok()
    );
    client.close();
}

#[wasm_bindgen]
pub async fn test_stale_rpc_admission(endpoint: &str) {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    let old = client.authenticated().await.unwrap();
    client.close();
    client.hello().await.unwrap();
    assert!(old
        .find_node(&"11".repeat(32), 20)
        .await
        .unwrap_err()
        .contains("session closed"));
    client.find_node(&"22".repeat(32), 20).await.unwrap();
    client.close();
}

#[wasm_bindgen]
pub async fn test_draining_pool_capacity(endpoints: JsValue) {
    let endpoints: Vec<String> = serde_wasm_bindgen::from_value(endpoints).unwrap();
    let pool = BrowserClientPool::new(1).unwrap();
    let first = BrowserEndpoint {
        multiaddr: endpoints[0].clone(),
    };
    let second = BrowserEndpoint {
        multiaddr: endpoints[1].clone(),
    };
    {
        let lease = pool.client(&first).await.unwrap();
        lease.hello().await.unwrap();
        assert!(crate::runtime::timeout(
            Duration::from_millis(10),
            lease.find_node(&"11".repeat(32), 20)
        )
        .await
        .is_err());
    }
    assert!(pool
        .client_before(&second, TransferDeadline::new(Duration::from_millis(20)))
        .await
        .is_err());
    pool.client_before(&second, TransferDeadline::new(Duration::from_millis(500)))
        .await
        .unwrap()
        .hello()
        .await
        .unwrap();
    pool.close();
}

#[wasm_bindgen]
pub async fn test_multiplex_puts(endpoint: &str) -> JsValue {
    let client = BrowserNodeClientCore::new(parse_webrtc_direct_multiaddr(endpoint).unwrap());
    client.hello().await.unwrap();
    let results = futures::future::join_all((0..6).map(|index| {
        let client = &client;
        async move {
            let bytes = vec![index as u8; 1024];
            let address = super::super::content_address(&bytes);
            let (quote, _) = client
                .authenticated()
                .await
                .unwrap()
                .quote_chunk(&address, bytes.len())
                .await
                .unwrap();
            client
                .authenticated()
                .await
                .unwrap()
                .put_chunk_typed(&address, &bytes, quote, &"ab".repeat(32))
                .await
                .is_ok()
        }
    }))
    .await;
    client.close();
    serde_wasm_bindgen::to_value(&results).unwrap()
}
