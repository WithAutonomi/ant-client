//! `web-sys` WebRTC Direct transport and typed node operations.

use super::manifest::{
    validate_browser_payment_network, BrowserPaymentNetwork, PublicFileDescriptor,
};
use super::payment::{
    storage_payment_total, verify_storage_quote, BrowserQuoteArtifact, VerifiedStorageQuote,
};
use super::protocol::{
    encode_request_frame, munge_offer_ice_credentials, parse_response_frame,
    parse_webrtc_direct_multiaddr, response_frame_length, server_answer_sdp, verify_hello_identity,
    BrowserEndpoint, BrowserEndpointInput, BrowserHello, BrowserProtocolError,
    BrowserResponseFrame, WebRtcDirectEndpoint, MAX_BROWSER_RESPONSE_BYTES,
    WEBRTC_DIRECT_DATA_CHANNEL, WEBRTC_WRITE_CHUNK_BYTES,
};
use crate::client_engine::adaptive::{
    observe_op, AdaptiveConfig, AdaptiveController, ChannelStart, Outcome,
};
use ant_protocol::web_rtc::transfer_timeout;
use ant_protocol::{CLOSE_GROUP_MAJORITY, CLOSE_GROUP_SIZE};
use futures_channel::{mpsc, oneshot};
use futures_util::{
    future::{join_all, select, Either},
    lock::Mutex,
    stream::{self, FuturesUnordered, StreamExt as _},
};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, ArrayBuffer, Promise, Uint8Array};
use saorsa_dht_lookup::{
    collect_after_first_with_grace, run_iterative_lookup, xor_distance, IterativeLookup,
    LookupConfig, LookupKey, LookupNode, LookupQuery, LookupQueryOutcome,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcDataChannelType, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

const REQUEST_TIMEOUT_MS: u32 = 10_000;
const MAX_BUFFERED_AMOUNT: u32 = 2 * 1024 * 1024;
const ICE_CREDENTIAL_PREFIX: &str = "saorsa+webrtc+v1/";
const ICE_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
const DEFAULT_MAX_POOLED_CLIENTS: usize = 32;
const DEFAULT_LOOKUP_K: usize = 20;
const DEFAULT_LOOKUP_ALPHA: usize = 3;
const DEFAULT_MAX_LOOKUP_ITERATIONS: usize = 20;
const LOOKUP_GRACE_TIMEOUT_MS: u32 = 5_000;
const ENDPOINT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const MAX_BROWSER_ROUTING_ENTRIES: usize = 256;
const MAX_BROWSER_ENDPOINT_FAILURES: usize = 256;
const DEFAULT_BROWSER_QUOTE_CONCURRENCY: usize = 4;
const MAX_DOWNLOAD_CONCURRENCY: usize = 6;
const MAX_BROWSER_RANGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RANGE_CACHE_BYTES: usize = 32 * 1024 * 1024;

type ResponseInbox = Rc<Mutex<mpsc::UnboundedReceiver<Result<Vec<u8>, String>>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct BrowserNode {
    pub(super) peer_id: String,
    #[serde(default)]
    pub(super) native_addresses: Vec<String>,
    #[serde(default)]
    pub(super) reliability: f64,
    #[serde(default)]
    pub(super) webrtc_direct: Option<BrowserEndpoint>,
}

#[derive(Debug, Serialize)]
struct BrowserLookupResult {
    nodes: Vec<BrowserNode>,
    queried: Vec<String>,
    failures: Vec<BrowserLookupFailure>,
}

#[derive(Debug, Clone, Serialize)]
struct BrowserLookupFailure {
    #[serde(rename = "peerId")]
    peer_id: String,
    message: String,
}

#[derive(Debug, Clone)]
struct BrowserLookupCandidate {
    peer_id: LookupKey,
    wire: BrowserNode,
}

impl LookupNode for BrowserLookupCandidate {
    fn lookup_peer_id(&self) -> LookupKey {
        self.peer_id
    }
}

impl BrowserLookupCandidate {
    fn parse(mut wire: BrowserNode) -> Result<Self, String> {
        let peer_id = parse_lookup_key(&wire.peer_id, "peer ID")?;
        wire.peer_id = hex::encode(peer_id);
        Ok(Self { peer_id, wire })
    }
}

struct PoolEntry {
    client: Rc<BrowserNodeClientCore>,
    last_used: u64,
}

struct BrowserClientPool {
    max_clients: usize,
    clients: RefCell<HashMap<String, PoolEntry>>,
    clock: Cell<u64>,
    closed: Cell<bool>,
    available_tx: mpsc::UnboundedSender<()>,
    available_rx: Mutex<mpsc::UnboundedReceiver<()>>,
}

struct BrowserClientLease {
    client: Rc<BrowserNodeClientCore>,
    available_tx: mpsc::UnboundedSender<()>,
}

impl Deref for BrowserClientLease {
    type Target = BrowserNodeClientCore;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Drop for BrowserClientLease {
    fn drop(&mut self) {
        let _ = self.available_tx.unbounded_send(());
    }
}

impl BrowserClientPool {
    fn new(max_clients: usize) -> Result<Self, String> {
        if max_clients == 0 {
            return Err("WebRTC client pool size must be a positive integer".to_string());
        }
        let (available_tx, available_rx) = mpsc::unbounded();
        Ok(Self {
            max_clients,
            clients: RefCell::new(HashMap::new()),
            clock: Cell::new(0),
            closed: Cell::new(false),
            available_tx,
            available_rx: Mutex::new(available_rx),
        })
    }

    async fn client(&self, endpoint: &BrowserEndpoint) -> Result<BrowserClientLease, String> {
        let endpoint = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
            .map_err(|error| error.to_string())?;
        let key = endpoint.multiaddr.clone();
        loop {
            if self.closed.get() {
                return Err("WebRTC client pool is closed".to_string());
            }
            let now = self.clock.get().wrapping_add(1);
            self.clock.set(now);
            let client = {
                let mut clients = self.clients.borrow_mut();
                if let Some(entry) = clients.get_mut(&key) {
                    entry.last_used = now;
                    Some(Rc::clone(&entry.client))
                } else {
                    if clients.len() >= self.max_clients {
                        let evict = clients
                            .iter()
                            .filter(|(_, entry)| Rc::strong_count(&entry.client) == 1)
                            .min_by_key(|(_, entry)| entry.last_used)
                            .map(|(key, _)| key.clone());
                        if let Some(evict) = evict {
                            if let Some(entry) = clients.remove(&evict) {
                                entry.client.close();
                            }
                        }
                    }
                    if clients.len() < self.max_clients {
                        let client = Rc::new(BrowserNodeClientCore::new(endpoint.clone()));
                        clients.insert(
                            key.clone(),
                            PoolEntry {
                                client: Rc::clone(&client),
                                last_used: now,
                            },
                        );
                        Some(client)
                    } else {
                        None
                    }
                }
            };
            if let Some(client) = client {
                return Ok(BrowserClientLease {
                    client,
                    available_tx: self.available_tx.clone(),
                });
            }
            if self.available_rx.lock().await.next().await.is_none() {
                return Err("WebRTC client pool closed while waiting for capacity".to_string());
            }
        }
    }

    fn close(&self) {
        self.closed.set(true);
        self.available_tx.close_channel();
        for (_, entry) in self.clients.borrow_mut().drain() {
            entry.client.close();
        }
    }
}

#[derive(Debug, Serialize)]
struct BrowserChunk {
    #[serde(with = "serde_bytes")]
    content: Vec<u8>,
    hash: String,
}

#[derive(Debug, Serialize)]
struct BrowserQuoteResponse {
    quote: BrowserQuoteArtifact,
    #[serde(rename = "alreadyStored")]
    already_stored: bool,
}

#[derive(Debug, Serialize)]
struct BrowserPutResponse {
    address: String,
    #[serde(rename = "alreadyStored")]
    already_stored: bool,
}

#[derive(Debug, Deserialize)]
struct NodesResponse {
    #[serde(rename = "type")]
    response_type: String,
    target: String,
    nodes: Vec<BrowserNode>,
}

#[derive(Debug, Deserialize)]
struct ChunkResponse {
    #[serde(rename = "type")]
    response_type: String,
    address: String,
    size: usize,
}

#[derive(Debug, Deserialize)]
struct QuoteResponse {
    #[serde(rename = "type")]
    response_type: String,
    address: String,
    already_stored: bool,
    quote: BrowserQuoteArtifact,
}

#[derive(Debug, Deserialize)]
struct PutResponse {
    #[serde(rename = "type")]
    response_type: String,
    address: String,
    already_stored: bool,
}

struct Connection {
    peer_connection: RtcPeerConnection,
    data_channel: RtcDataChannel,
    inbox: ResponseInbox,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
    _on_close: Closure<dyn FnMut(Event)>,
    _on_open: Closure<dyn FnMut(Event)>,
}

impl Connection {
    async fn open(endpoint: &WebRtcDirectEndpoint) -> Result<Self, String> {
        let configuration = RtcConfiguration::new();
        configuration.set_ice_servers(&Array::new());
        let peer_connection =
            RtcPeerConnection::new_with_configuration(&configuration).map_err(js_error_message)?;
        let channel_configuration = RtcDataChannelInit::new();
        channel_configuration.set_ordered(true);
        let data_channel = peer_connection.create_data_channel_with_data_channel_dict(
            WEBRTC_DIRECT_DATA_CHANNEL,
            &channel_configuration,
        );
        data_channel.set_binary_type(RtcDataChannelType::Arraybuffer);

        let (inbox_tx, inbox_rx) = mpsc::unbounded::<Result<Vec<u8>, String>>();
        let message_tx = inbox_tx.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            let data = event.data();
            let result = if data.is_instance_of::<ArrayBuffer>() || ArrayBuffer::is_view(&data) {
                Ok(Uint8Array::new(&data).to_vec())
            } else {
                Err("node sent a non-binary DataChannel message".to_string())
            };
            let _ = message_tx.unbounded_send(result);
        });
        data_channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let error_tx = inbox_tx.clone();
        let on_error = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            let _ = error_tx.unbounded_send(Err("WebRTC DataChannel failed".to_string()));
        });
        data_channel.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let close_tx = inbox_tx;
        let on_close = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            let _ = close_tx.unbounded_send(Err("WebRTC DataChannel closed".to_string()));
        });
        data_channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let (open_tx, open_rx) = oneshot::channel::<()>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));
        let open_sender = Rc::clone(&open_tx);
        let on_open = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            if let Some(sender) = open_sender.borrow_mut().take() {
                let _ = sender.send(());
            }
        });
        data_channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));

        // Own the browser objects and every installed callback before the
        // first await. Any setup error now runs `Drop`, detaches the callbacks,
        // and closes the half-open peer connection deterministically.
        let connection = Self {
            peer_connection,
            data_channel,
            inbox: Rc::new(Mutex::new(inbox_rx)),
            _on_message: on_message,
            _on_error: on_error,
            _on_close: on_close,
            _on_open: on_open,
        };

        let credential = random_ice_credential()?;
        let offer = JsFuture::from(connection.peer_connection.create_offer())
            .await
            .map_err(js_error_message)?;
        // `RTCSessionDescriptionInit` is a Web IDL dictionary, not a branded
        // interface. Chromium returns a plain object here, so `dyn_into` can
        // reject a perfectly valid offer because there is no `instanceof`
        // identity to test. Read the dictionary member structurally instead.
        let offer_sdp = js_sys::Reflect::get(&offer, &JsValue::from_str("sdp"))
            .map_err(js_error_message)?
            .as_string()
            .filter(|sdp| !sdp.is_empty())
            .ok_or_else(|| "browser created an empty WebRTC offer".to_string())?;
        let munged_sdp = munge_offer_ice_credentials(&offer_sdp, &credential)
            .map_err(|error| error.to_string())?;
        let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        local.set_sdp(&munged_sdp);
        JsFuture::from(connection.peer_connection.set_local_description(&local))
            .await
            .map_err(js_error_message)?;
        let answer_sdp =
            server_answer_sdp(endpoint, &credential).map_err(|error| error.to_string())?;
        let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
        remote.set_sdp(&answer_sdp);
        JsFuture::from(connection.peer_connection.set_remote_description(&remote))
            .await
            .map_err(js_error_message)?;

        timeout(
            async move {
                open_rx
                    .await
                    .map_err(|_| "WebRTC DataChannel closed before opening".to_string())
            },
            "WebRTC DataChannel opening timed out",
        )
        .await?;
        connection.data_channel.set_onopen(None);

        Ok(connection)
    }

    fn close(self) {
        drop(self);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.data_channel.set_onmessage(None);
        self.data_channel.set_onerror(None);
        self.data_channel.set_onclose(None);
        self.data_channel.set_onopen(None);
        self.data_channel.set_onbufferedamountlow(None);
        self.data_channel.close();
        self.peer_connection.close();
    }
}

pub(super) struct BrowserNodeClientCore {
    endpoint: WebRtcDirectEndpoint,
    connection: RefCell<Option<Connection>>,
    request_lock: Mutex<()>,
    next_request_id: Cell<u64>,
    hello: RefCell<Option<BrowserHello>>,
    peer_id: RefCell<Option<String>>,
}

impl BrowserNodeClientCore {
    pub(super) fn new(endpoint: WebRtcDirectEndpoint) -> Self {
        Self {
            endpoint,
            connection: RefCell::new(None),
            request_lock: Mutex::new(()),
            next_request_id: Cell::new(1),
            hello: RefCell::new(None),
            peer_id: RefCell::new(None),
        }
    }

    pub(super) fn peer_id(&self) -> Option<String> {
        self.peer_id.borrow().clone()
    }

    async fn ensure_connected(&self) -> Result<(), String> {
        let open = self.connection.borrow().as_ref().is_some_and(|connection| {
            connection.data_channel.ready_state() == RtcDataChannelState::Open
        });
        if open {
            return Ok(());
        }
        self.close();
        let connection = Connection::open(&self.endpoint).await?;
        self.connection.replace(Some(connection));
        Ok(())
    }

    async fn request(
        &self,
        request_type: &str,
        fields: Map<String, Value>,
        content: &[u8],
    ) -> Result<BrowserResponseFrame, String> {
        let _guard = self.request_lock.lock().await;
        self.ensure_connected().await?;
        let request_id = self.next_request_id.get();
        self.next_request_id.set(request_id.wrapping_add(1).max(1));
        let frame = encode_request_frame(request_id, request_type, fields, content)
            .map_err(|error| error.to_string())?;
        let transfer_timeout_ms = transfer_timeout_ms(frame.len());
        let channel = {
            let connection = self.connection.borrow();
            connection
                .as_ref()
                .map(|connection| connection.data_channel.clone())
        };
        let Some(channel) = channel else {
            self.close();
            return Err("WebRTC DataChannel is not connected".to_string());
        };
        let send_deadline_ms = js_sys::Date::now() + f64::from(transfer_timeout_ms);
        let send_result = async {
            for message in frame.chunks(WEBRTC_WRITE_CHUNK_BYTES) {
                wait_for_capacity(&channel, remaining_timeout_ms(send_deadline_ms)).await?;
                channel
                    .send_with_u8_array(message)
                    .map_err(js_error_message)?;
            }
            Ok::<(), String>(())
        }
        .await;
        if let Err(error) = send_result {
            self.close();
            return Err(error);
        }
        let receiver = {
            let connection = self.connection.borrow();
            connection
                .as_ref()
                .map(|connection| Rc::clone(&connection.inbox))
        };
        let Some(receiver) = receiver else {
            self.close();
            return Err("WebRTC response inbox is unavailable".to_string());
        };
        let response = match read_response(receiver, transfer_timeout_ms).await {
            Ok(response) => response,
            Err(error) => {
                self.close();
                return Err(error);
            }
        };
        if response.header.get("request_id").and_then(Value::as_u64) != Some(request_id) {
            let error = format!(
                "response ID {} does not match request {request_id}",
                response.header.get("request_id").unwrap_or(&Value::Null)
            );
            self.close();
            return Err(error);
        }
        if response.header.get("status").and_then(Value::as_str) == Some("error") {
            let authentication_required = response.header.get("code").and_then(Value::as_str)
                == Some("authentication_required");
            let error = response
                .header
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("node returned an error")
                .to_string();
            if authentication_required {
                self.close();
            }
            return Err(error);
        }
        Ok(response)
    }

    pub(super) async fn hello(&self) -> Result<BrowserHello, String> {
        if let Some(hello) = self.hello.borrow().clone() {
            if self.connection.borrow().as_ref().is_some_and(|connection| {
                connection.data_channel.ready_state() == RtcDataChannelState::Open
            }) {
                return Ok(hello);
            }
        }
        let mut challenge = [0u8; 32];
        getrandom::getrandom(&mut challenge)
            .map_err(|error| format!("browser entropy failed: {error}"))?;
        let mut fields = Map::new();
        fields.insert("challenge".to_string(), Value::from(hex::encode(challenge)));
        let response = self.request("hello", fields, &[]).await?;
        let hello: BrowserHello = match serde_json::from_value(response.header) {
            Ok(hello) => hello,
            Err(error) => {
                self.close();
                return Err(format!("invalid HELLO response: {error}"));
            }
        };
        let peer_id = match verify_hello_identity(&hello, &self.endpoint, &challenge) {
            Ok(peer_id) => peer_id,
            Err(error) => {
                self.close();
                return Err(error.to_string());
            }
        };
        self.peer_id.replace(Some(peer_id));
        self.hello.replace(Some(hello.clone()));
        Ok(hello)
    }

    pub(super) async fn find_node(
        &self,
        target: &str,
        count: usize,
    ) -> Result<Vec<BrowserNode>, String> {
        let target = super::protocol::normalize_hex(target, 32)?;
        let mut fields = Map::new();
        fields.insert("target".to_string(), Value::from(target.clone()));
        fields.insert("count".to_string(), Value::from(count));
        let response = self.request("find_node", fields, &[]).await?;
        let response: NodesResponse = serde_json::from_value(response.header)
            .map_err(|error| format!("invalid NODES response: {error}"))?;
        if response.response_type != "nodes" {
            return Err("expected a NODES response".to_string());
        }
        if response.target.to_ascii_lowercase() != target {
            return Err("node returned results for a different lookup target".to_string());
        }
        for node in &response.nodes {
            let peer_id = super::protocol::normalize_hex(&node.peer_id, 32)?;
            if let Some(endpoint) = &node.webrtc_direct {
                let endpoint = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
                    .map_err(|error| error.to_string())?;
                if endpoint.peer_id != peer_id {
                    return Err(format!("node {peer_id} advertised another peer's endpoint"));
                }
            }
        }
        Ok(response.nodes)
    }

    pub(super) async fn get_chunk(&self, address: &str) -> Result<(Vec<u8>, String), String> {
        let address = super::protocol::normalize_hex(address, 32)?;
        let mut fields = Map::new();
        fields.insert("address".to_string(), Value::from(address.clone()));
        let response = self.request("get_chunk", fields, &[]).await?;
        if response.header.get("status").and_then(Value::as_str) == Some("not_found") {
            return Err(format!("chunk {address} was not found on this node"));
        }
        let header: ChunkResponse = serde_json::from_value(response.header)
            .map_err(|error| format!("invalid CHUNK response: {error}"))?;
        if header.response_type != "chunk" {
            return Err("expected a CHUNK response".to_string());
        }
        if header.address.to_ascii_lowercase() != address {
            return Err("node returned a different chunk address".to_string());
        }
        if header.size != response.content.len() {
            return Err("chunk metadata size does not match its content".to_string());
        }
        super::verify_record(&address, &response.content).map_err(|error| error.to_string())?;
        Ok((response.content, address))
    }

    pub(super) async fn quote_chunk(
        &self,
        address: &str,
        size: usize,
    ) -> Result<(BrowserQuoteArtifact, bool), String> {
        let address = super::protocol::normalize_hex(address, 32)?;
        if size > super::protocol::MAX_BROWSER_RECORD_BYTES {
            return Err(format!("invalid chunk size {size}"));
        }
        let mut fields = Map::new();
        fields.insert("address".to_string(), Value::from(address.clone()));
        fields.insert("size".to_string(), Value::from(size));
        let response = self.request("quote_chunk", fields, &[]).await?;
        let header: QuoteResponse = serde_json::from_value(response.header)
            .map_err(|error| format!("invalid STORAGE_QUOTE response: {error}"))?;
        if header.response_type != "storage_quote" {
            return Err("expected a STORAGE_QUOTE response".to_string());
        }
        if header.address.to_ascii_lowercase() != address {
            return Err("node returned a quote for a different chunk address".to_string());
        }
        Ok((header.quote, header.already_stored))
    }

    pub(super) async fn put_chunk(
        &self,
        address: &str,
        content: &[u8],
        quote: BrowserQuoteArtifact,
        transaction_hash: &str,
    ) -> Result<(String, bool), String> {
        let address = super::protocol::normalize_hex(address, 32)?;
        let transaction_hash = super::protocol::normalize_hex(transaction_hash, 32)?;
        super::verify_record(&address, content).map_err(|error| error.to_string())?;
        let mut fields = Map::new();
        fields.insert("address".to_string(), Value::from(address.clone()));
        fields.insert(
            "quote".to_string(),
            serde_json::to_value(quote).map_err(|error| error.to_string())?,
        );
        fields.insert(
            "transaction_hash".to_string(),
            Value::from(transaction_hash),
        );
        let response = self.request("put_chunk", fields, content).await?;
        let header: PutResponse = serde_json::from_value(response.header)
            .map_err(|error| format!("invalid CHUNK_STORED response: {error}"))?;
        if header.response_type != "chunk_stored" {
            return Err("expected a CHUNK_STORED response".to_string());
        }
        if header.address.to_ascii_lowercase() != address {
            return Err("node stored a different chunk address".to_string());
        }
        Ok((address, header.already_stored))
    }

    pub(super) fn close(&self) {
        if let Some(connection) = self.connection.borrow_mut().take() {
            connection.close();
        }
        self.hello.borrow_mut().take();
        self.peer_id.borrow_mut().take();
    }
}

struct BrowserNetworkCore {
    seeds: Vec<BrowserEndpoint>,
    pool: Rc<BrowserClientPool>,
    routing: Rc<RefCell<HashMap<LookupKey, BrowserLookupCandidate>>>,
    failed_endpoints: Rc<RefCell<crate::client_engine::EndpointFailureCache<LookupKey>>>,
}

impl BrowserNetworkCore {
    fn new(seeds: Vec<BrowserEndpoint>) -> Result<Self, String> {
        if seeds.is_empty() {
            return Err("at least one seed endpoint is required".to_string());
        }
        let seeds = seeds
            .into_iter()
            .map(|seed| {
                parse_webrtc_direct_multiaddr(&seed.multiaddr)
                    .map(|endpoint| BrowserEndpoint {
                        multiaddr: endpoint.multiaddr,
                    })
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            seeds,
            pool: Rc::new(BrowserClientPool::new(DEFAULT_MAX_POOLED_CLIENTS)?),
            routing: Rc::new(RefCell::new(HashMap::new())),
            failed_endpoints: Rc::new(RefCell::new(
                crate::client_engine::EndpointFailureCache::new(
                    ENDPOINT_FAILURE_COOLDOWN,
                    MAX_BROWSER_ENDPOINT_FAILURES,
                ),
            )),
        })
    }

    async fn find_closest(
        &self,
        target: &str,
        progress: &ProgressReporter,
    ) -> Result<BrowserLookupResult, String> {
        let target_key = parse_lookup_key(target, "lookup target")?;
        let failures = Rc::new(RefCell::new(Vec::new()));
        let seed_futures = self.seeds.iter().cloned().map(|endpoint| {
            let pool = Rc::clone(&self.pool);
            let failures = Rc::clone(&failures);
            let progress = progress.clone();
            async move {
                let seed_name = endpoint.multiaddr.clone();
                let result = async {
                    let client = pool.client(&endpoint).await?;
                    let hello = client.hello().await?;
                    progress.report(&format!("Connected seed {}", hello.peer_id));
                    BrowserLookupCandidate::parse(BrowserNode {
                        peer_id: hello.peer_id,
                        native_addresses: Vec::new(),
                        reliability: 1.0,
                        webrtc_direct: Some(hello.endpoint),
                    })
                }
                .await;
                match result {
                    Ok(candidate) => Some(candidate),
                    Err(error) => {
                        progress.report(&format!("Seed {seed_name} failed: {error}"));
                        failures.borrow_mut().push(BrowserLookupFailure {
                            peer_id: seed_name,
                            message: error,
                        });
                        None
                    }
                }
            }
        });
        let mut initial_candidates = self.routing.borrow().values().cloned().collect::<Vec<_>>();
        if initial_candidates.is_empty() {
            initial_candidates = join_all(seed_futures)
                .await
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
        }
        if initial_candidates.is_empty() {
            let detail = failures
                .borrow()
                .iter()
                .map(|failure| failure.message.as_str())
                .collect::<Vec<_>>()
                .join("; ");
            return Err(format!(
                "could not connect to any WebRtcDirect seed: {detail}"
            ));
        }

        let config = LookupConfig {
            count: DEFAULT_LOOKUP_K,
            alpha: DEFAULT_LOOKUP_ALPHA,
            max_iterations: DEFAULT_MAX_LOOKUP_ITERATIONS,
            ..LookupConfig::saorsa(DEFAULT_LOOKUP_K)
        };
        let mut lookup =
            IterativeLookup::new(target_key, config).map_err(|error| error.to_string())?;
        let mut known_endpoints = self
            .routing
            .borrow()
            .iter()
            .filter_map(|(peer, candidate)| {
                candidate
                    .wire
                    .webrtc_direct
                    .clone()
                    .map(|endpoint| (*peer, endpoint))
            })
            .collect::<HashMap<_, _>>();
        for candidate in initial_candidates {
            if let Some(endpoint) = candidate.wire.webrtc_direct.clone() {
                known_endpoints.insert(candidate.peer_id, endpoint);
                self.routing
                    .borrow_mut()
                    .insert(candidate.peer_id, candidate.clone());
                let _ = lookup.add_candidate(candidate);
            }
        }
        let mut query = BrowserNetworkLookupQuery {
            pool: Rc::clone(&self.pool),
            progress: progress.clone(),
            failures: Rc::clone(&failures),
            known_endpoints,
            routing: Rc::clone(&self.routing),
            failed_endpoints: Rc::clone(&self.failed_endpoints),
        };
        run_iterative_lookup(&mut lookup, &mut query)
            .await
            .map_err(|error| error.to_string())?;
        let mut routes = self.routing.borrow_mut();
        if routes.len() > MAX_BROWSER_ROUTING_ENTRIES {
            let mut peers = routes.keys().copied().collect::<Vec<_>>();
            peers.sort_by_key(|peer| xor_distance(peer, &target_key));
            for peer in peers.into_iter().skip(MAX_BROWSER_ROUTING_ENTRIES) {
                routes.remove(&peer);
            }
        }
        drop(routes);
        let nodes = lookup
            .results()
            .into_iter()
            .map(|candidate| candidate.wire)
            .collect();
        let queried = lookup.queried_peers().iter().map(hex::encode).collect();
        let failures = failures.borrow().clone();
        Ok(BrowserLookupResult {
            nodes,
            queried,
            failures,
        })
    }

    async fn get_chunk_from_closest(
        &self,
        address: &str,
        progress: &ProgressReporter,
    ) -> Result<(Vec<u8>, BrowserNode), String> {
        let address = super::protocol::normalize_hex(address, 32)?;
        let lookup = self.find_closest(&address, progress).await?;
        let mut failures = Vec::new();
        for node in lookup.nodes {
            let Some(endpoint) = node.webrtc_direct.as_ref() else {
                continue;
            };
            progress.report(&format!("Requesting {address} from {}", node.peer_id));
            let result = async {
                let client = self.pool.client(endpoint).await?;
                client.hello().await?;
                client.get_chunk(&address).await
            }
            .await;
            match result {
                Ok((content, _)) => return Ok((content, node)),
                Err(error) => {
                    progress.report(&format!(
                        "Node {} did not return the file: {error}",
                        node.peer_id
                    ));
                    failures.push(format!("{}: {error}", node.peer_id));
                }
            }
        }
        Err(format!(
            "no closest WebRtcDirect node returned chunk {address}{}",
            if failures.is_empty() {
                String::new()
            } else {
                format!(" ({})", failures.join("; "))
            }
        ))
    }
}

struct BrowserNetworkLookupQuery {
    pool: Rc<BrowserClientPool>,
    progress: ProgressReporter,
    failures: Rc<RefCell<Vec<BrowserLookupFailure>>>,
    known_endpoints: HashMap<LookupKey, BrowserEndpoint>,
    routing: Rc<RefCell<HashMap<LookupKey, BrowserLookupCandidate>>>,
    failed_endpoints: Rc<RefCell<crate::client_engine::EndpointFailureCache<LookupKey>>>,
}

impl LookupQuery<BrowserLookupCandidate> for BrowserNetworkLookupQuery {
    type Error = String;

    async fn is_candidate_eligible(
        &mut self,
        candidate: &BrowserLookupCandidate,
    ) -> Result<bool, Self::Error> {
        let Some(endpoint) = candidate.wire.webrtc_direct.as_ref() else {
            return Ok(false);
        };
        Ok(!self
            .failed_endpoints
            .borrow_mut()
            .is_suppressed(&candidate.peer_id, &endpoint.multiaddr))
    }

    async fn query_batch(
        &mut self,
        target: LookupKey,
        count: usize,
        iteration: usize,
        batch: Vec<BrowserLookupCandidate>,
    ) -> Result<Vec<LookupQueryOutcome<BrowserLookupCandidate>>, Self::Error> {
        let target = hex::encode(target);
        let attempted = batch
            .iter()
            .filter_map(|candidate| {
                candidate
                    .wire
                    .webrtc_direct
                    .as_ref()
                    .map(|endpoint| (candidate.peer_id, endpoint.multiaddr.clone()))
            })
            .collect::<HashMap<_, _>>();
        let futures: FuturesUnordered<_> = batch
            .into_iter()
            .map(|candidate| {
                let pool = Rc::clone(&self.pool);
                let progress = self.progress.clone();
                let failures = Rc::clone(&self.failures);
                let failed_endpoints = Rc::clone(&self.failed_endpoints);
                let target = target.clone();
                async move {
                    let responder = candidate.peer_id;
                    let peer_id = candidate.wire.peer_id.clone();
                    let failed_endpoint = candidate
                        .wire
                        .webrtc_direct
                        .as_ref()
                        .map(|endpoint| endpoint.multiaddr.clone());
                    let result = async {
                        let endpoint = candidate.wire.webrtc_direct.as_ref().ok_or_else(|| {
                            "lookup candidate has no WebRTC Direct endpoint".to_string()
                        })?;
                        let client = pool.client(endpoint).await?;
                        client.hello().await?;
                        client.find_node(&target, count).await
                    }
                    .await;
                    match result {
                        Ok(nodes) => {
                            failed_endpoints.borrow_mut().record_success(&responder);
                            progress.report(&format!(
                                "Iteration {iteration}: {peer_id} returned {} nodes",
                                nodes.len()
                            ));
                            let candidates = nodes
                                .into_iter()
                                .filter_map(|wire| match BrowserLookupCandidate::parse(wire) {
                                    Ok(candidate) => Some(candidate),
                                    Err(error) => {
                                        progress.report(&format!(
                                            "Ignoring invalid candidate from {peer_id}: {error}"
                                        ));
                                        None
                                    }
                                })
                                .collect();
                            LookupQueryOutcome::Succeeded {
                                responder,
                                candidates,
                            }
                        }
                        Err(error) => {
                            if let Some(endpoint) = failed_endpoint {
                                failed_endpoints
                                    .borrow_mut()
                                    .record_failure(responder, endpoint);
                            }
                            progress.report(&format!("Query {peer_id} failed: {error}"));
                            failures.borrow_mut().push(BrowserLookupFailure {
                                peer_id,
                                message: error,
                            });
                            LookupQueryOutcome::Failed { responder }
                        }
                    }
                }
            })
            .collect();
        let mut outcomes =
            collect_after_first_with_grace(futures, || TimeoutFuture::new(LOOKUP_GRACE_TIMEOUT_MS))
                .await;
        let responded = outcomes
            .iter()
            .map(|outcome| *outcome.responder())
            .collect::<HashSet<_>>();
        for (peer, endpoint) in attempted {
            if !responded.contains(&peer) {
                self.failed_endpoints
                    .borrow_mut()
                    .record_failure(peer, endpoint);
                let peer_id = hex::encode(peer);
                let message = "did not respond before the lookup grace period".to_string();
                self.progress
                    .report(&format!("Query {peer_id} failed: {message}"));
                self.failures
                    .borrow_mut()
                    .push(BrowserLookupFailure { peer_id, message });
            }
        }
        for outcome in &mut outcomes {
            if let LookupQueryOutcome::Succeeded { candidates, .. } = outcome {
                candidates.retain_mut(|candidate| {
                    if let Some(endpoint) = candidate.wire.webrtc_direct.clone() {
                        self.known_endpoints.insert(candidate.peer_id, endpoint);
                    } else if let Some(endpoint) = self.known_endpoints.get(&candidate.peer_id) {
                        candidate.wire.webrtc_direct = Some(endpoint.clone());
                    }
                    if candidate.wire.webrtc_direct.is_some() {
                        self.routing
                            .borrow_mut()
                            .insert(candidate.peer_id, candidate.clone());
                        true
                    } else {
                        false
                    }
                });
            }
        }
        Ok(outcomes)
    }
}

#[derive(Clone, Default)]
struct ProgressReporter(Option<js_sys::Function>);

impl ProgressReporter {
    fn from_js(value: Option<js_sys::Function>) -> Self {
        Self(value)
    }

    fn report(&self, message: &str) {
        if let Some(callback) = &self.0 {
            let _ = callback.call1(&JsValue::NULL, &JsValue::from_str(message));
        }
    }
}

fn parse_lookup_key(value: &str, label: &str) -> Result<LookupKey, String> {
    let value = value.strip_prefix("0x").unwrap_or(value);
    let bytes = hex::decode(value).map_err(|error| format!("invalid {label}: {error}"))?;
    bytes.try_into().map_err(|bytes: Vec<u8>| {
        format!(
            "invalid {label}: expected 32 bytes, received {}",
            bytes.len()
        )
    })
}

#[derive(Debug, Serialize)]
struct BrowserDownloadResult {
    #[serde(with = "serde_bytes")]
    content: Vec<u8>,
    hash: String,
    #[serde(rename = "dataMapNode")]
    data_map_node: BrowserNode,
}

#[derive(Clone)]
struct StoreTarget {
    peer_id: String,
    endpoint: BrowserEndpoint,
}

struct PreparedRecord {
    record: super::BrowserRecord,
    already_stored: bool,
    targets: Vec<StoreTarget>,
    verified: Option<VerifiedStorageQuote>,
}

struct PendingStoreRecord<'a> {
    index: usize,
    record: &'a PreparedRecord,
    successful_peers: HashSet<String>,
}

struct StoreAttemptError {
    successful_peers: HashSet<String>,
    message: String,
}

impl StoreAttemptError {
    fn new(successful_peers: HashSet<String>, message: impl Into<String>) -> Self {
        Self {
            successful_peers,
            message: message.into(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserPaymentSubmission {
    #[serde(rename = "transactionHash")]
    transaction_hash: Option<String>,
    #[serde(rename = "totalAmount")]
    total_amount: String,
}

#[derive(Debug, Serialize)]
struct BrowserUploadResult {
    file: PublicFileDescriptor,
    #[serde(rename = "transactionHash", skip_serializing_if = "Option::is_none")]
    transaction_hash: Option<String>,
    #[serde(rename = "storageCostAtto")]
    storage_cost_atto: String,
    records: usize,
}

struct CachedRangeRecord {
    content: bytes::Bytes,
    last_used: u64,
}

#[derive(Default)]
struct BrowserRangeCache {
    entries: HashMap<[u8; 32], CachedRangeRecord>,
    total_bytes: usize,
    clock: u64,
}

impl BrowserRangeCache {
    fn contains(&self, address: &[u8; 32]) -> bool {
        self.entries.contains_key(address)
    }

    fn get(&mut self, address: &[u8; 32]) -> Option<bytes::Bytes> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.entries.get_mut(address)?;
        entry.last_used = self.clock;
        Some(entry.content.clone())
    }

    fn insert(&mut self, address: [u8; 32], content: Vec<u8>) {
        self.clock = self.clock.wrapping_add(1);
        let content = bytes::Bytes::from(content);
        if let Some(previous) = self.entries.remove(&address) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.content.len());
        }
        self.total_bytes = self.total_bytes.saturating_add(content.len());
        self.entries.insert(
            address,
            CachedRangeRecord {
                content,
                last_used: self.clock,
            },
        );
        while self.total_bytes > MAX_RANGE_CACHE_BYTES && self.entries.len() > 1 {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(address, _)| *address)
            else {
                break;
            };
            if let Some(removed) = self.entries.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.content.len());
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.total_bytes = 0;
    }
}

/// Random-access public-file reader for media playback and bounded downloads.
#[wasm_bindgen(js_name = BrowserFileReader)]
pub struct BrowserFileReader {
    inner: Rc<BrowserNetworkCore>,
    file: PublicFileDescriptor,
    root_data_map: self_encryption::DataMap,
    cache: RefCell<BrowserRangeCache>,
    progress: ProgressReporter,
    closed: Cell<bool>,
}

#[wasm_bindgen(js_class = BrowserFileReader)]
impl BrowserFileReader {
    /// Plaintext file size in bytes.
    #[wasm_bindgen(getter)]
    pub fn size(&self) -> usize {
        self.file.size
    }

    /// Browser MIME type advertised by the file descriptor.
    #[wasm_bindgen(getter, js_name = contentType)]
    pub fn content_type(&self) -> String {
        self.file.content_type.clone()
    }

    /// Display filename advertised by the file descriptor.
    #[wasm_bindgen(getter)]
    pub fn name(&self) -> String {
        self.file.name.clone()
    }

    /// Fetch and decrypt one plaintext byte range without reconstructing the file.
    #[wasm_bindgen(js_name = readRange)]
    pub async fn read_range(&self, start: usize, length: usize) -> Result<Uint8Array, JsValue> {
        let content = self
            .read_range_inner(start, length)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        Ok(Uint8Array::from(content.as_slice()))
    }

    /// Release cached encrypted records held for playback read-ahead and seeks.
    pub fn close(&self) {
        self.closed.set(true);
        self.cache.borrow_mut().clear();
    }
}

impl BrowserFileReader {
    async fn read_range_inner(&self, start: usize, length: usize) -> Result<Vec<u8>, String> {
        if self.closed.get() {
            return Err("browser file reader is closed".to_string());
        }
        if length > MAX_BROWSER_RANGE_BYTES {
            return Err(format!(
                "browser range reads are limited to {MAX_BROWSER_RANGE_BYTES} bytes"
            ));
        }
        if length == 0 || start >= self.file.size {
            return Ok(Vec::new());
        }
        let end = start.saturating_add(length).min(self.file.size);
        let required = required_range_records(&self.root_data_map, start, end)?;
        if required.is_empty() {
            return Err("DataMap contains no records for the requested range".to_string());
        }

        let missing = {
            let cache = self.cache.borrow();
            required
                .iter()
                .filter(|(_, address)| !cache.contains(address))
                .copied()
                .collect::<Vec<_>>()
        };
        if !missing.is_empty() {
            let downloads = stream::iter(missing)
                .map(|(index, address)| {
                    let inner = Rc::clone(&self.inner);
                    let progress = self.progress.clone();
                    async move {
                        let encoded = hex::encode(address);
                        progress.report(&format!(
                            "Streaming encrypted chunk {} ({encoded})",
                            index + 1
                        ));
                        inner
                            .get_chunk_from_closest(&encoded, &progress)
                            .await
                            .map(|(content, _)| (address, content))
                    }
                })
                .buffer_unordered(MAX_DOWNLOAD_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            let mut cache = self.cache.borrow_mut();
            for download in downloads {
                let (address, content) = download?;
                cache.insert(address, content);
            }
        }

        let available = {
            let mut cache = self.cache.borrow_mut();
            required
                .iter()
                .map(|(_, address)| {
                    cache
                        .get(address)
                        .map(|content| (*address, content))
                        .ok_or_else(|| {
                            format!("streaming cache omitted record {}", hex::encode(address))
                        })
                })
                .collect::<Result<HashMap<_, _>, _>>()?
        };
        let fetch_cached = |requested: &[(usize, self_encryption::XorName)]| {
            requested
                .iter()
                .map(|(index, address)| {
                    available
                        .get(&address.0)
                        .cloned()
                        .map(|content| (*index, content))
                        .ok_or_else(|| {
                            self_encryption::Error::Generic(format!(
                                "streaming range omitted record {}",
                                hex::encode(address.0)
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()
        };
        let stream = self_encryption::streaming_decrypt_with_batch_size(
            &self.root_data_map,
            fetch_cached,
            required.len(),
        )
        .map_err(|error| format!("could not initialize range decryption: {error}"))?;
        let plaintext = stream
            .get_range(start, end - start)
            .map_err(|error| format!("could not decrypt requested range: {error}"))?;
        if plaintext.len() != end - start {
            return Err(format!(
                "range decryption returned {} bytes, expected {}",
                plaintext.len(),
                end - start
            ));
        }
        Ok(plaintext.to_vec())
    }
}

fn required_range_records(
    data_map: &self_encryption::DataMap,
    start: usize,
    end: usize,
) -> Result<Vec<(usize, [u8; 32])>, String> {
    let mut infos = data_map.infos().to_vec();
    infos.sort_by_key(|info| info.index);
    let mut cursor = 0usize;
    let mut required = Vec::new();
    for info in infos {
        let chunk_end = cursor
            .checked_add(info.src_size)
            .ok_or_else(|| "DataMap plaintext size overflow".to_string())?;
        if cursor < end && chunk_end > start {
            required.push((info.index, info.dst_hash.0));
        }
        cursor = chunk_end;
    }
    Ok(required)
}

/// Stateful Autonomi browser client sharing Rust lookup and data workflows.
#[wasm_bindgen(js_name = BrowserNetworkClient)]
pub struct BrowserNetworkClient {
    inner: Rc<BrowserNetworkCore>,
    controller: AdaptiveController,
}

#[wasm_bindgen(js_class = BrowserNetworkClient)]
impl BrowserNetworkClient {
    /// Construct a reusable client around stable WebRTC Direct seed addresses.
    #[wasm_bindgen(constructor)]
    pub fn new(endpoints: JsValue) -> Result<Self, JsValue> {
        let endpoints: Vec<BrowserEndpointInput> = serde_wasm_bindgen::from_value(endpoints)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let endpoints = endpoints
            .into_iter()
            .map(|endpoint| BrowserEndpoint {
                multiaddr: endpoint.multiaddr().to_string(),
            })
            .collect();
        let inner =
            BrowserNetworkCore::new(endpoints).map_err(|error| JsValue::from_str(&error))?;
        Ok(Self {
            inner: Rc::new(inner),
            controller: AdaptiveController::new(ChannelStart::default(), AdaptiveConfig::default()),
        })
    }

    /// Run Saorsa's iterative closest-node lookup over Rust-owned DataChannels.
    #[wasm_bindgen(js_name = findClosest)]
    pub async fn find_closest(
        &self,
        target: &str,
        on_progress: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let progress = ProgressReporter::from_js(on_progress);
        let result = self
            .inner
            .find_closest(target, &progress)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&result).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Download and reconstruct a complete public Autonomi file.
    #[wasm_bindgen(js_name = downloadPublicFile)]
    pub async fn download_public_file(
        &self,
        file: JsValue,
        concurrency: usize,
        on_progress: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let file: PublicFileDescriptor = serde_wasm_bindgen::from_value(file)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        let result = self
            .download_public_file_inner(file, concurrency, &progress)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&result).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Resolve and validate a public file for random-access range reads.
    #[wasm_bindgen(js_name = openPublicFile)]
    pub async fn open_public_file(
        &self,
        file: JsValue,
        on_progress: Option<js_sys::Function>,
    ) -> Result<BrowserFileReader, JsValue> {
        let file: PublicFileDescriptor = serde_wasm_bindgen::from_value(file)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        self.open_public_file_inner(file, progress)
            .await
            .map_err(|error| JsValue::from_str(&error))
    }

    /// Self-encrypt, quote, pay through a wallet callback, and store a public file.
    #[wasm_bindgen(js_name = uploadPublicFile)]
    pub async fn upload_public_file(
        &self,
        content: &[u8],
        name: &str,
        content_type: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        on_progress: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let payment_network: BrowserPaymentNetwork =
            serde_wasm_bindgen::from_value(payment_network)
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let payment_network = validate_browser_payment_network(payment_network)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        let result = self
            .upload_public_file_inner(
                content,
                name,
                content_type,
                payment_network,
                &pay_for_quotes,
                &progress,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&result).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Close all pooled WebRTC associations.
    pub fn close(&self) {
        self.inner.pool.close();
    }
}

impl BrowserNetworkClient {
    async fn open_public_file_inner(
        &self,
        mut file: PublicFileDescriptor,
        progress: ProgressReporter,
    ) -> Result<BrowserFileReader, String> {
        file.address = super::protocol::normalize_hex(&file.address, 32)?;
        file.blake3 = super::protocol::normalize_hex(&file.blake3, 32)?;
        if file.name.is_empty() {
            return Err("public file has no name".to_string());
        }
        if file.size == 0 || file.size > super::MAX_BROWSER_FILE_BYTES {
            return Err(format!("invalid public file size {}", file.size));
        }
        progress.report(&format!(
            "Opening {} for random-access streaming",
            file.name
        ));
        let (encoded_data_map, _) = self
            .inner
            .get_chunk_from_closest(&file.address, &progress)
            .await?;
        if encoded_data_map.len() != file.data_map_size {
            return Err(format!(
                "public DataMap has {} bytes, expected {}",
                encoded_data_map.len(),
                file.data_map_size
            ));
        }
        let published_data_map: self_encryption::DataMap = rmp_serde::from_slice(&encoded_data_map)
            .map_err(|error| format!("could not decode public DataMap: {error}"))?;
        let root_data_map = if published_data_map.is_child() {
            let child_infos = published_data_map.infos().to_vec();
            let downloads = stream::iter(child_infos.iter().cloned())
                .map(|info| {
                    let inner = Rc::clone(&self.inner);
                    let progress = progress.clone();
                    async move {
                        let address = hex::encode(info.dst_hash.0);
                        progress.report(&format!(
                            "Resolving nested DataMap record {}",
                            info.index + 1
                        ));
                        inner
                            .get_chunk_from_closest(&address, &progress)
                            .await
                            .map(|(content, _)| (info.dst_hash.0, bytes::Bytes::from(content)))
                    }
                })
                .buffer_unordered(MAX_DOWNLOAD_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
            let mut child_records = HashMap::with_capacity(downloads.len());
            for download in downloads {
                let (address, content) = download?;
                child_records.insert(address, content);
            }
            let mut get_child = |address: self_encryption::XorName| {
                child_records.get(&address.0).cloned().ok_or_else(|| {
                    self_encryption::Error::Generic(format!(
                        "nested DataMap resolution requested unavailable record {}",
                        hex::encode(address.0)
                    ))
                })
            };
            self_encryption::get_root_data_map(published_data_map, &mut get_child)
                .map_err(|error| format!("could not resolve root DataMap: {error}"))?
        } else {
            published_data_map
        };

        let actual_chunks = super::chunk_infos(&root_data_map);
        let mut expected_chunks = file
            .chunks
            .iter()
            .map(|chunk| super::BrowserChunkInfo {
                index: chunk.index,
                dst_hash: chunk.dst_hash.to_ascii_lowercase(),
                src_hash: chunk.src_hash.to_ascii_lowercase(),
                src_size: chunk.src_size,
            })
            .collect::<Vec<_>>();
        expected_chunks.sort_by_key(|chunk| chunk.index);
        if actual_chunks != expected_chunks {
            return Err(
                "resolved root DataMap does not match the public file descriptor".to_string(),
            );
        }
        let resolved_size = actual_chunks.iter().try_fold(0usize, |total, chunk| {
            total
                .checked_add(chunk.src_size)
                .ok_or_else(|| "resolved public file size overflow".to_string())
        })?;
        if resolved_size != file.size {
            return Err(format!(
                "resolved public file has {resolved_size} bytes, expected {}",
                file.size
            ));
        }
        progress.report(&format!(
            "Ready to stream {} ({} bytes, {} chunks)",
            file.name,
            file.size,
            actual_chunks.len()
        ));
        Ok(BrowserFileReader {
            inner: Rc::clone(&self.inner),
            file,
            root_data_map,
            cache: RefCell::new(BrowserRangeCache::default()),
            progress,
            closed: Cell::new(false),
        })
    }

    async fn download_public_file_inner(
        &self,
        file: PublicFileDescriptor,
        concurrency: usize,
        progress: &ProgressReporter,
    ) -> Result<BrowserDownloadResult, String> {
        let address = super::protocol::normalize_hex(&file.address, 32)?;
        let expected_hash = super::protocol::normalize_hex(&file.blake3, 32)?;
        if concurrency == 0 {
            return Err("download concurrency must be a positive integer".to_string());
        }
        let concurrency = concurrency.min(MAX_DOWNLOAD_CONCURRENCY);
        progress.report(&format!("Fetching public DataMap {address}"));
        let (data_map, data_map_node) = self
            .inner
            .get_chunk_from_closest(&address, progress)
            .await?;
        if data_map.len() != file.data_map_size {
            return Err(format!(
                "public DataMap has {} bytes, expected {}",
                data_map.len(),
                file.data_map_size
            ));
        }
        progress.report(&format!(
            "Verified public DataMap ({} bytes)",
            data_map.len()
        ));
        let mut chunks =
            super::decode_public_data_map(&data_map).map_err(|error| error.to_string())?;
        chunks.extend(file.chunks.iter().cloned());
        let mut seen = HashSet::new();
        chunks.retain(|chunk| seen.insert(chunk.dst_hash.clone()));
        if chunks.len() < 3 {
            return Err("ant-core returned an invalid public DataMap".to_string());
        }
        let total = chunks.len();
        let downloads = stream::iter(chunks.into_iter().enumerate())
            .map(|(position, chunk)| {
                let inner = Rc::clone(&self.inner);
                let progress = progress.clone();
                async move {
                    progress.report(&format!(
                        "Fetching encrypted file chunk {}/{} ({})",
                        position + 1,
                        total,
                        chunk.dst_hash
                    ));
                    inner
                        .get_chunk_from_closest(&chunk.dst_hash, &progress)
                        .await
                        .map(|(content, _)| (position, content))
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await;
        let mut encrypted_chunks = Vec::with_capacity(total);
        for download in downloads {
            encrypted_chunks.push(download?);
        }
        encrypted_chunks.sort_by_key(|(position, _)| *position);
        let encrypted_chunks = encrypted_chunks
            .into_iter()
            .map(|(_, content)| content)
            .collect::<Vec<_>>();
        progress.report(&format!(
            "Reconstructing {} with native ant-core WASM",
            file.name
        ));
        let content = super::decrypt_public_file(&data_map, &encrypted_chunks)
            .map_err(|error| error.to_string())?;
        if content.len() != file.size {
            return Err(format!(
                "reconstructed file has {} bytes, expected {}",
                content.len(),
                file.size
            ));
        }
        super::verify_record(&expected_hash, &content).map_err(|error| error.to_string())?;
        progress.report(&format!(
            "Verified complete {} as {expected_hash}",
            file.name
        ));
        Ok(BrowserDownloadResult {
            content,
            hash: expected_hash,
            data_map_node,
        })
    }

    async fn upload_public_file_inner(
        &self,
        content: &[u8],
        name: &str,
        content_type: &str,
        payment_network: BrowserPaymentNetwork,
        pay_for_quotes: &js_sys::Function,
        progress: &ProgressReporter,
    ) -> Result<BrowserUploadResult, String> {
        if name.is_empty() {
            return Err("upload file has no name".to_string());
        }
        progress.report(&format!(
            "Self-encrypting {name} with native ant-core WASM ({} bytes)",
            content.len()
        ));
        let encrypted = super::encrypt_public_file(content).map_err(|error| error.to_string())?;
        let mut records = encrypted.records.iter().cloned().enumerate();
        let mut prepared = Vec::with_capacity(encrypted.records.len());
        if let Some((index, record)) = records.next() {
            progress.report(&format!(
                "Preparing record {}/{}",
                index + 1,
                encrypted.records.len()
            ));
            prepared.push((
                index,
                self.prepare_record(record, &payment_network, progress)
                    .await?,
            ));
        }
        let record_count = encrypted.records.len();
        let payment_network_ref = &payment_network;
        let remaining = records.map(|(index, record)| async move {
            progress.report(&format!("Preparing record {}/{}", index + 1, record_count));
            self.prepare_record(record, payment_network_ref, progress)
                .await
                .map(|prepared| (index, prepared))
        });
        let remaining =
            crate::client_engine::bounded_unordered(remaining, DEFAULT_BROWSER_QUOTE_CONCURRENCY)
                .collect::<Vec<_>>()
                .await;
        for result in remaining {
            prepared.push(result?);
        }
        prepared.sort_by_key(|(index, _)| *index);
        let prepared = prepared
            .into_iter()
            .map(|(_, record)| record)
            .collect::<Vec<_>>();
        let verified_quotes = prepared
            .iter()
            .filter_map(|record| record.verified.clone())
            .collect::<Vec<_>>();
        let expected_total =
            storage_payment_total(&verified_quotes).map_err(|error| error.to_string())?;
        let mut payment = if verified_quotes.is_empty() {
            BrowserPaymentSubmission {
                transaction_hash: None,
                total_amount: "0".to_string(),
            }
        } else {
            invoke_payment(pay_for_quotes, &payment_network, &verified_quotes).await?
        };
        if !verified_quotes.is_empty() && payment.transaction_hash.is_none() {
            return Err("wallet callback returned no storage payment transaction".to_string());
        }
        if payment.total_amount != expected_total {
            return Err(format!(
                "wallet callback reported payment total {}, expected {expected_total}",
                payment.total_amount
            ));
        }
        if let Some(transaction_hash) = payment.transaction_hash.as_mut() {
            *transaction_hash = super::protocol::normalize_hex(transaction_hash, 32)?;
        }

        let replicas = self
            .store_prepared_records(
                &prepared,
                &payment_network,
                payment.transaction_hash.as_deref(),
                progress,
            )
            .await?;
        let descriptor = PublicFileDescriptor {
            name: name.to_string(),
            address: encrypted.address,
            size: content.len(),
            content_type: if content_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                content_type.to_string()
            },
            blake3: encrypted.blake3,
            data_map_size: encrypted.data_map_size,
            chunks: encrypted.chunks,
            replicas,
        };
        Ok(BrowserUploadResult {
            file: descriptor,
            transaction_hash: payment.transaction_hash,
            storage_cost_atto: payment.total_amount,
            records: prepared.len(),
        })
    }

    async fn prepare_record(
        &self,
        record: super::BrowserRecord,
        payment_network: &BrowserPaymentNetwork,
        progress: &ProgressReporter,
    ) -> Result<PreparedRecord, String> {
        progress.report(&format!("Finding closest nodes for {}", record.address));
        let lookup = self.inner.find_closest(&record.address, progress).await?;
        let targets = lookup
            .nodes
            .into_iter()
            .filter_map(|node| {
                node.webrtc_direct.map(|endpoint| StoreTarget {
                    peer_id: node.peer_id,
                    endpoint,
                })
            })
            .take(CLOSE_GROUP_SIZE)
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Err(
                "closest-node lookup returned no WebRTC Direct storage targets".to_string(),
            );
        }
        let mut failures = Vec::new();
        for target in &targets {
            let result = async {
                let client = self.inner.pool.client(&target.endpoint).await?;
                let hello = client.hello().await?;
                assert_upload_node(&hello, payment_network)?;
                let (quote, already_stored) = client
                    .quote_chunk(&record.address, record.content.len())
                    .await?;
                let verified = verify_storage_quote(quote, &record.address, &target.peer_id)
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>((already_stored, verified))
            }
            .await;
            match result {
                Ok((true, _)) => {
                    progress.report(&format!(
                        "Chunk {} is already stored; skipping payment",
                        record.address
                    ));
                    return Ok(PreparedRecord {
                        record,
                        already_stored: true,
                        targets,
                        verified: None,
                    });
                }
                Ok((false, verified)) => {
                    progress.report(&format!(
                        "Verified storage quote {} from {}",
                        verified.quote_hash, target.peer_id
                    ));
                    let mut ordered_targets = Vec::with_capacity(targets.len());
                    ordered_targets.push(target.clone());
                    ordered_targets.extend(
                        targets
                            .iter()
                            .filter(|candidate| candidate.peer_id != target.peer_id)
                            .cloned(),
                    );
                    return Ok(PreparedRecord {
                        record,
                        already_stored: false,
                        targets: ordered_targets,
                        verified: Some(verified),
                    });
                }
                Err(error) => failures.push(format!("{}: {error}", target.peer_id)),
            }
        }
        Err(format!(
            "no closest node supplied a valid quote ({})",
            failures.join("; ")
        ))
    }

    /// Store every paid record with the same adaptive, byte-bounded retry
    /// rounds used by the native client.
    async fn store_prepared_records(
        &self,
        prepared: &[PreparedRecord],
        payment_network: &BrowserPaymentNetwork,
        transaction_hash: Option<&str>,
        progress: &ProgressReporter,
    ) -> Result<usize, String> {
        let record_count = prepared.len();
        let max_record_bytes = prepared
            .iter()
            .map(|record| record.record.content.len())
            .max()
            .unwrap_or(0);
        let byte_bound = crate::client_engine::store_byte_bound(max_record_bytes);
        let mut to_retry = prepared
            .iter()
            .enumerate()
            .map(|(index, record)| PendingStoreRecord {
                index,
                record,
                successful_peers: HashSet::new(),
            })
            .collect::<Vec<_>>();
        let mut replicas = usize::MAX;

        for attempt in 0..=crate::client_engine::STORE_MAX_RETRIES {
            if attempt > 0 {
                let delay = crate::client_engine::store_retry_delay(attempt);
                progress.report(&format!(
                    "Retrying {} record(s), attempt {attempt}/{}",
                    to_retry.len(),
                    crate::client_engine::STORE_MAX_RETRIES
                ));
                TimeoutFuture::new(u32::try_from(delay.as_millis()).unwrap_or(u32::MAX)).await;
            }

            let op_limiter = self.controller.store.clone();
            let cap_limiter = op_limiter.clone();
            let results = crate::client_engine::rolling_unordered(
                to_retry,
                |pending| {
                    let PendingStoreRecord {
                        index,
                        record,
                        successful_peers,
                    } = pending;
                    let limiter = op_limiter.clone();
                    async move {
                        progress.report(&format!(
                            "Storing record {}/{} (attempt {}/{})",
                            index + 1,
                            record_count,
                            attempt + 1,
                            crate::client_engine::STORE_MAX_RETRIES + 1
                        ));
                        let result = observe_op(
                            &limiter,
                            || {
                                self.store_prepared_once(
                                    record,
                                    payment_network,
                                    transaction_hash,
                                    progress,
                                    successful_peers,
                                )
                            },
                            |error| classify_browser_store_error(&error.message),
                        )
                        .await;
                        ((index, record), result)
                    }
                },
                || cap_limiter.current().min(byte_bound),
            )
            .await;

            let mut failed = Vec::new();
            for ((index, record), result) in results {
                match result {
                    Ok(stored) => replicas = replicas.min(stored),
                    Err(error) => failed.push((
                        PendingStoreRecord {
                            index,
                            record,
                            successful_peers: error.successful_peers,
                        },
                        error.message,
                    )),
                }
            }
            if failed.is_empty() {
                return Ok(if replicas == usize::MAX { 0 } else { replicas });
            }
            if attempt == crate::client_engine::STORE_MAX_RETRIES {
                let failed_count = failed.len();
                let details = failed
                    .into_iter()
                    .map(|(pending, error)| {
                        format!("record {}/{}: {error}", pending.index + 1, record_count)
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(format!(
                    "{} paid record(s) failed after {} attempts: {details}",
                    failed_count,
                    crate::client_engine::STORE_MAX_RETRIES + 1
                ));
            }
            to_retry = failed.into_iter().map(|(pending, _)| pending).collect();
        }

        Err("record store retry loop ended unexpectedly".to_string())
    }

    /// Store one record to a close-group majority, advancing through the rest
    /// of the ordered K=7 target set only when an initial target fails.
    async fn store_prepared_once(
        &self,
        prepared: &PreparedRecord,
        payment_network: &BrowserPaymentNetwork,
        transaction_hash: Option<&str>,
        progress: &ProgressReporter,
        mut successful_peers: HashSet<String>,
    ) -> Result<usize, StoreAttemptError> {
        if prepared.already_stored {
            return Ok(1);
        }
        let Some(transaction_hash) = transaction_hash else {
            return Err(StoreAttemptError::new(
                successful_peers,
                "paid record has no transaction hash",
            ));
        };
        let transaction_hash = transaction_hash.to_string();
        let Some(verified) = prepared.verified.as_ref() else {
            return Err(StoreAttemptError::new(
                successful_peers,
                "paid record has no verified quote",
            ));
        };
        let required = CLOSE_GROUP_MAJORITY.saturating_sub(successful_peers.len());
        let outcome = crate::client_engine::quorum_with_fallback(
            prepared
                .targets
                .iter()
                .filter(|target| !successful_peers.contains(&target.peer_id))
                .cloned(),
            required,
            |target| {
                let pool = Rc::clone(&self.inner.pool);
                let record = prepared.record.clone();
                let quote = verified.quote.clone();
                let payment_network = payment_network.clone();
                let transaction_hash = transaction_hash.clone();
                let progress = progress.clone();
                async move {
                    let client = pool.client(&target.endpoint).await?;
                    let hello = client.hello().await?;
                    assert_upload_node(&hello, &payment_network)?;
                    let (_, already_stored) = client
                        .put_chunk(&record.address, &record.content, quote, &transaction_hash)
                        .await?;
                    if already_stored {
                        progress.report(&format!(
                            "Already stored on {}: {}",
                            target.peer_id, record.address
                        ));
                    } else {
                        progress
                            .report(&format!("Stored {} on {}", record.address, target.peer_id));
                    }
                    Ok::<(), String>(())
                }
            },
        )
        .await;
        debug_assert_eq!(outcome.successes, outcome.successful_targets.len());
        for target in outcome.successful_targets {
            successful_peers.insert(target.peer_id);
        }
        let failures = outcome
            .failures
            .into_iter()
            .map(|(target, error)| {
                progress.report(&format!("Store target {} failed: {error}", target.peer_id));
                format!("{}: {error}", target.peer_id)
            })
            .collect::<Vec<_>>();
        if !outcome.reached || successful_peers.len() < CLOSE_GROUP_MAJORITY {
            let replicas = successful_peers.len();
            return Err(StoreAttemptError::new(
                successful_peers,
                format!(
                    "stored on {} peers, need {CLOSE_GROUP_MAJORITY}; failures: {}",
                    replicas,
                    failures.join("; ")
                ),
            ));
        }
        Ok(successful_peers.len())
    }
}

fn classify_browser_store_error(error: &str) -> Outcome {
    let error = error.to_ascii_lowercase();
    if error.contains("timed out") || error.contains("timeout") {
        Outcome::Timeout
    } else if error.contains("webrtc")
        || error.contains("datachannel")
        || error.contains("ice")
        || error.contains("connect")
        || error.contains("closed")
        || error.contains("invalid state")
    {
        Outcome::NetworkError
    } else {
        Outcome::ApplicationError
    }
}

async fn invoke_payment(
    callback: &js_sys::Function,
    payment_network: &BrowserPaymentNetwork,
    quotes: &[VerifiedStorageQuote],
) -> Result<BrowserPaymentSubmission, String> {
    let payment_network =
        serde_wasm_bindgen::to_value(payment_network).map_err(|error| error.to_string())?;
    let quotes = serde_wasm_bindgen::to_value(quotes).map_err(|error| error.to_string())?;
    let returned = callback
        .call2(&JsValue::NULL, &payment_network, &quotes)
        .map_err(js_error_message)?;
    let returned = JsFuture::from(Promise::resolve(&returned))
        .await
        .map_err(js_error_message)?;
    serde_wasm_bindgen::from_value(returned)
        .map_err(|error| format!("wallet callback returned an invalid payment result: {error}"))
}

fn assert_upload_node(
    hello: &BrowserHello,
    expected: &BrowserPaymentNetwork,
) -> Result<(), String> {
    if !hello
        .capabilities
        .iter()
        .any(|value| value == "quote_chunk")
        || !hello.capabilities.iter().any(|value| value == "put_chunk")
    {
        return Err("node does not advertise paid browser uploads".to_string());
    }
    let advertised: BrowserPaymentNetwork = serde_json::from_value(hello.payment.clone())
        .map_err(|error| format!("node advertises invalid payment configuration: {error}"))?;
    let advertised_rpc = url::Url::parse(&advertised.rpc_url)
        .map_err(|error| format!("node advertises invalid payment RPC URL: {error}"))?;
    let expected_rpc = url::Url::parse(&expected.rpc_url)
        .map_err(|error| format!("manifest has invalid payment RPC URL: {error}"))?;
    if advertised_rpc != expected_rpc
        || !advertised
            .payment_token_address
            .eq_ignore_ascii_case(&expected.payment_token_address)
        || !advertised
            .payment_vault_address
            .eq_ignore_ascii_case(&expected.payment_vault_address)
    {
        return Err("node advertises a different payment network than the manifest".to_string());
    }
    Ok(())
}

/// One authenticated browser-to-node WebRTC Direct client implemented in Rust.
#[wasm_bindgen(js_name = BrowserNodeClient)]
pub struct BrowserNodeClient {
    inner: Rc<BrowserNodeClientCore>,
}

#[wasm_bindgen(js_class = BrowserNodeClient)]
impl BrowserNodeClient {
    /// Construct a client from a raw or structured WebRTC Direct endpoint.
    #[wasm_bindgen(constructor)]
    pub fn new(endpoint: JsValue) -> Result<Self, JsValue> {
        let endpoint: BrowserEndpointInput = serde_wasm_bindgen::from_value(endpoint)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let endpoint = parse_webrtc_direct_multiaddr(endpoint.multiaddr())
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        Ok(Self {
            inner: Rc::new(BrowserNodeClientCore::new(endpoint)),
        })
    }

    /// Authenticated peer ID, when HELLO has completed.
    #[wasm_bindgen(getter, js_name = peerId)]
    pub fn peer_id(&self) -> Option<String> {
        self.inner.peer_id()
    }

    /// Open the direct DataChannel without issuing an application request.
    pub async fn connect(&self) -> Result<(), JsValue> {
        let _guard = self.inner.request_lock.lock().await;
        self.inner
            .ensure_connected()
            .await
            .map_err(|error| JsValue::from_str(&error))
    }

    /// Authenticate the connected node.
    pub async fn hello(&self) -> Result<JsValue, JsValue> {
        let hello = self
            .inner
            .hello()
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        hello
            .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Request nodes closest to a 32-byte target.
    #[wasm_bindgen(js_name = findNode)]
    pub async fn find_node(&self, target: &str, count: usize) -> Result<JsValue, JsValue> {
        let nodes = self
            .inner
            .find_node(target, count)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&nodes).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Retrieve and BLAKE3-verify one content-addressed record.
    #[wasm_bindgen(js_name = getChunk)]
    pub async fn get_chunk(&self, address: &str) -> Result<JsValue, JsValue> {
        let (content, hash) = self
            .inner
            .get_chunk(address)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&BrowserChunk { content, hash })
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Request a signed storage quote.
    #[wasm_bindgen(js_name = quoteChunk)]
    pub async fn quote_chunk(&self, address: &str, size: usize) -> Result<JsValue, JsValue> {
        let (quote, already_stored) = self
            .inner
            .quote_chunk(address, size)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&BrowserQuoteResponse {
            quote,
            already_stored,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Store a paid content-addressed record.
    #[wasm_bindgen(js_name = putChunk)]
    pub async fn put_chunk(
        &self,
        address: &str,
        content: &[u8],
        quote: JsValue,
        transaction_hash: &str,
    ) -> Result<JsValue, JsValue> {
        let quote: BrowserQuoteArtifact = serde_wasm_bindgen::from_value(quote)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let (address, already_stored) = self
            .inner
            .put_chunk(address, content, quote, transaction_hash)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&BrowserPutResponse {
            address,
            already_stored,
        })
        .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Close the DataChannel and peer connection.
    pub fn close(&self) {
        self.inner.close();
    }
}

async fn read_response(
    receiver: ResponseInbox,
    initial_timeout_ms: u32,
) -> Result<BrowserResponseFrame, String> {
    let mut frame = Vec::with_capacity(8 * 1024);
    let mut expected_length = None;
    let response_started_ms = js_sys::Date::now();
    let mut response_deadline_ms = response_started_ms + f64::from(initial_timeout_ms);
    loop {
        let remaining_ms = remaining_timeout_ms(response_deadline_ms);
        let next = timeout_with_ms(
            async { Ok(receiver.lock().await.next().await) },
            "WebRTC request timed out",
            remaining_ms,
        )
        .await?;
        let message = next
            .ok_or_else(|| "response ended before its declared frame was complete".to_string())??;
        let next_length = frame
            .len()
            .checked_add(message.len())
            .ok_or_else(|| "response length overflow".to_string())?;
        if next_length > MAX_BROWSER_RESPONSE_BYTES {
            return Err(format!(
                "response exceeded the {MAX_BROWSER_RESPONSE_BYTES}-byte client limit"
            ));
        }
        frame.extend_from_slice(&message);
        if expected_length.is_none() {
            expected_length = response_frame_length(&frame).map_err(|error| error.to_string())?;
            if let Some(expected) = expected_length {
                response_deadline_ms = response_deadline_ms
                    .max(response_started_ms + f64::from(transfer_timeout_ms(expected)));
            }
        }
        if let Some(expected) = expected_length {
            if frame.len() > expected {
                return Err("response contains bytes after its declared frame".to_string());
            }
            if frame.len() == expected {
                return parse_response_frame(&frame).map_err(|error| error.to_string());
            }
        }
    }
}

async fn wait_for_capacity(channel: &RtcDataChannel, timeout_ms: u32) -> Result<(), String> {
    if channel.ready_state() != RtcDataChannelState::Open {
        return Err("WebRTC DataChannel closed while draining".to_string());
    }
    if channel.buffered_amount() <= MAX_BUFFERED_AMOUNT {
        return Ok(());
    }
    channel.set_buffered_amount_low_threshold(MAX_BUFFERED_AMOUNT / 2);
    let (sender, receiver) = oneshot::channel::<()>();
    let sender = Rc::new(RefCell::new(Some(sender)));
    let ready_sender = Rc::clone(&sender);
    let on_ready = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
        if let Some(sender) = ready_sender.borrow_mut().take() {
            let _ = sender.send(());
        }
    });
    channel.set_onbufferedamountlow(Some(on_ready.as_ref().unchecked_ref()));
    // The buffer can cross the threshold between the first check and callback
    // installation. Re-check after installing it so that race cannot turn a
    // completed drain into a full transfer-timeout wait.
    if channel.buffered_amount() <= MAX_BUFFERED_AMOUNT {
        if let Some(sender) = sender.borrow_mut().take() {
            let _ = sender.send(());
        }
    }
    let result = timeout_with_ms(
        async move {
            receiver
                .await
                .map_err(|_| "WebRTC DataChannel closed while draining".to_string())
        },
        "WebRTC DataChannel drain timed out",
        timeout_ms,
    )
    .await;
    channel.set_onbufferedamountlow(None);
    drop(on_ready);
    result
}

async fn timeout<T, F>(future: F, message: &'static str) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    timeout_with_ms(future, message, REQUEST_TIMEOUT_MS).await
}

async fn timeout_with_ms<T, F>(
    future: F,
    message: &'static str,
    timeout_ms: u32,
) -> Result<T, String>
where
    F: Future<Output = Result<T, String>>,
{
    let operation = Box::pin(future);
    let timer = Box::pin(TimeoutFuture::new(timeout_ms));
    match select(operation, timer).await {
        Either::Left((result, _)) => result,
        Either::Right(((), _)) => Err(message.to_string()),
    }
}

fn transfer_timeout_ms(content_bytes: usize) -> u32 {
    u32::try_from(transfer_timeout(content_bytes).as_millis()).unwrap_or(u32::MAX)
}

fn remaining_timeout_ms(deadline_ms: f64) -> u32 {
    let remaining_ms = (deadline_ms - js_sys::Date::now()).ceil();
    if !remaining_ms.is_finite() || remaining_ms <= 0.0 {
        0
    } else if remaining_ms >= f64::from(u32::MAX) {
        u32::MAX
    } else {
        remaining_ms as u32
    }
}

fn random_ice_credential() -> Result<String, String> {
    let mut random = [0u8; 32];
    getrandom::getrandom(&mut random)
        .map_err(|error| format!("browser entropy failed: {error}"))?;
    let mut credential = String::with_capacity(ICE_CREDENTIAL_PREFIX.len() + random.len());
    credential.push_str(ICE_CREDENTIAL_PREFIX);
    for byte in random {
        credential.push(char::from(
            ICE_ALPHABET[usize::from(byte) % ICE_ALPHABET.len()],
        ));
    }
    Ok(credential)
}

fn js_error_message(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(&value, &JsValue::from_str("message"))
                .ok()?
                .as_string()
        })
        .unwrap_or_else(|| format!("browser WebRTC operation failed: {value:?}"))
}

impl From<BrowserProtocolError> for JsValue {
    fn from(error: BrowserProtocolError) -> Self {
        JsValue::from_str(&error.to_string())
    }
}
