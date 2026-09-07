//! Mock node for generated-WASM tests; excluded from production bindings.
//! Only the WebRTC host is mocked: authentication, framing, quotes, and storage
//! all traverse the same ant-core implementation used by browser callers.

use super::*;
use base64::Engine;
use fips204::{
    ml_dsa_65,
    traits::{KeyGen, SerDes, Signer},
};
use saorsa_webrtc::{
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
}

fn network() -> BrowserPaymentNetwork {
    BrowserPaymentNetwork {
        rpc_url: "http://127.0.0.1:8545/".into(),
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
        }
    }
    pub fn endpoint(&self) -> String {
        self.endpoint.clone()
    }
    pub fn last_method(&self) -> String {
        self.last_method.clone()
    }
    pub fn set_chunk(&mut self, chunk: Vec<u8>) {
        self.chunk = chunk;
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
        let body = match request.request.body {
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
                    capabilities: vec![
                        "find_node".into(),
                        "get_chunk".into(),
                        "quote_chunk".into(),
                        "put_chunk".into(),
                    ],
                }
            }
            BrowserRequestBody::FindNode { target, .. } => {
                self.last_method = "find_node".into();
                BrowserResponseBody::Nodes {
                    target,
                    nodes: vec![],
                }
            }
            BrowserRequestBody::QuoteChunk { address, .. } => {
                self.last_method = "quote_chunk".into();
                let content: [u8; 32] = hex::decode(&address).unwrap().try_into().unwrap();
                let price = saorsa_webrtc::calculate_price_wei(0);
                let timestamp = (js_sys::Date::now() / 1000.0) as u64;
                let signed = saorsa_webrtc::payment_quote_bytes_for_signing(
                    &content,
                    timestamp,
                    price,
                    &[0x44; 20],
                    0,
                    None,
                );
                let signature = self
                    .secret
                    .try_sign_with_seed(&[8; 32], &signed, b"")
                    .unwrap();
                let hash = saorsa_webrtc::payment_quote_hash(&signed, &self.public, &signature);
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
                        committed_key_count: 0,
                        commitment_pin: None,
                        quote_hash: hex::encode(hash),
                        commitment: None,
                    },
                }
            }
            BrowserRequestBody::PutChunk { address, .. } => {
                self.last_method = "put_chunk".into();
                BrowserResponseBody::ChunkStored {
                    address,
                    already_stored: false,
                }
            }
            BrowserRequestBody::GetChunk { address } => {
                self.last_method = "get_chunk".into();
                BrowserResponseBody::Chunk {
                    address,
                    size: self.chunk.len(),
                }
            }
        };
        let content = if self.last_method == "get_chunk" {
            self.chunk.as_slice()
        } else {
            &[]
        };
        let response = BrowserResponse::ok(request.request.request_id, body, content.len());
        let plaintext = encode_response_frame(&response, content).unwrap();
        let encrypted = self.session.as_mut().unwrap().seal(&plaintext).unwrap();
        encode_pq_frame(&encrypted).unwrap()
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
