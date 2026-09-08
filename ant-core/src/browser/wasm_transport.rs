//! `web-sys` WebRTC Direct transport and typed node operations.

use ant_protocol::transport::DEFAULT_K_VALUE as DEFAULT_LOOKUP_K;
use ant_protocol::CLOSE_GROUP_MAJORITY;

use super::manifest::{
    assert_upload_node, validate_browser_payment_network, BrowserPaymentNetwork,
    PublicFileDescriptor,
};
use super::payment::{BrowserQuoteArtifact, VerifiedStorageQuote};
use super::protocol::{
    encode_request_frame, ice_password_from_sdp, parse_response_frame,
    parse_webrtc_direct_multiaddr, server_answer_sdp, v2_server_ice_credential,
    validate_hello_metadata, BrowserEndpoint, BrowserEndpointInput, BrowserHello, BrowserNode,
    BrowserRequest, BrowserRequestBody, BrowserResponseBody, BrowserResponseFrame,
    BrowserResponseStatus, WebRtcDirectEndpoint, MAX_BROWSER_RECORD_BYTES,
    MAX_BROWSER_RESPONSE_BYTES, WEBRTC_DIRECT_DATA_CHANNEL, WEBRTC_WRITE_CHUNK_BYTES,
};
use super::{BrowserRecord, BrowserRecordInfo, BrowserStagedFile};
#[cfg(feature = "test-utils")]
use crate::transfer_policy::PutRejection;
use crate::transfer_policy::RpcError;
use ant_protocol::transport::{
    collect_after_first_with_grace, run_iterative_lookup, xor_distance, IterativeLookup,
    LookupConfig, LookupKey, LookupNode, LookupQuery, LookupQueryOutcome,
};
use futures_channel::{mpsc, oneshot};
use futures_util::{
    future::{join_all, select, Either},
    lock::Mutex,
    stream::{FuturesUnordered, StreamExt as _},
};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, Promise, Uint8Array};
use saorsa_transport::webrtc::{
    decode_pq_frame, encode_pq_frame, pq_frame_length, transfer_timeout, PqClientHandshake,
    PqSession, PQ_ENCRYPTED_OVERHEAD_BYTES, PQ_SERVER_ACCEPT_BYTES,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::rc::Rc;
use std::time::Duration;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcDataChannelType, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

const REQUEST_TIMEOUT_MS: u32 = 10_000;
const MAX_BUFFERED_AMOUNT: u32 = 2 * 1024 * 1024;
const DEFAULT_MAX_POOLED_CLIENTS: usize = 32;
const ENDPOINT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const MAX_BROWSER_ROUTING_ENTRIES: usize = 256;
const MAX_BROWSER_ENDPOINT_FAILURES: usize = 256;
const DEFAULT_BROWSER_QUOTE_CONCURRENCY: usize = 4;
const MAX_DOWNLOAD_CONCURRENCY: usize = 6;
const MAX_BROWSER_RANGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RANGE_CACHE_BYTES: usize = 32 * 1024 * 1024;

mod inbox;
mod shared;
mod upload_adapter;
use ant_protocol::transport::{client_routing, PeerId};
use shared::{native_quote_artifact, SharedNetworkAdapter};
#[cfg(feature = "test-utils")]
mod test_utils;
use inbox::ResponseInbox;

#[derive(Debug, Serialize)]
struct BrowserLookupResult {
    nodes: Vec<BrowserNode>,
    queried: Vec<String>,
    failures: Vec<BrowserLookupFailure>,
    #[serde(skip)]
    views: HashMap<LookupKey, Vec<BrowserNode>>,
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

type DialFailures = Rc<RefCell<crate::client_engine::EndpointFailureCache<String>>>;

struct BrowserClientPool {
    dial_failures: DialFailures,
    max_clients: usize,
    clients: RefCell<HashMap<String, PoolEntry>>,
    clock: Cell<u64>,
    availability: Rc<PoolAvailability>,
    availability_rx: Mutex<mpsc::Receiver<()>>,
}

struct BrowserClientLease {
    client: Rc<BrowserNodeClientCore>,
    availability: Rc<PoolAvailability>,
}

struct PoolAvailability {
    closed: Cell<bool>,
    sender: mpsc::Sender<()>,
}

impl PoolAvailability {
    fn notify_one(&self) {
        if self.closed.get() {
            return;
        }
        // The capacity-one channel coalesces repeated lease drops. Normal use
        // can retain at most one wake token, including when a blocked client
        // future is canceled before it consumes the notification.
        let mut sender = self.sender.clone();
        let _ = sender.try_send(());
    }

    fn close(&self) {
        self.closed.set(true);
        // Wake the one receiver that may currently hold the async mutex. Any
        // additional waiters observe `closed` when they acquire that mutex.
        let mut sender = self.sender.clone();
        let _ = sender.try_send(());
    }
}

impl Deref for BrowserClientLease {
    type Target = BrowserNodeClientCore;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl Drop for BrowserClientLease {
    fn drop(&mut self) {
        self.availability.notify_one();
    }
}

impl BrowserClientPool {
    fn new(max_clients: usize) -> Result<Self, String> {
        if max_clients == 0 {
            return Err("WebRTC client pool size must be a positive integer".to_string());
        }
        let (availability_tx, availability_rx) = mpsc::channel(1);
        Ok(Self {
            dial_failures: Rc::new(RefCell::new(
                crate::client_engine::EndpointFailureCache::new(
                    ENDPOINT_FAILURE_COOLDOWN,
                    MAX_BROWSER_ENDPOINT_FAILURES,
                ),
            )),
            max_clients,
            clients: RefCell::new(HashMap::new()),
            clock: Cell::new(0),
            availability: Rc::new(PoolAvailability {
                closed: Cell::new(false),
                sender: availability_tx,
            }),
            availability_rx: Mutex::new(availability_rx),
        })
    }

    async fn wait_for_availability(&self) -> Result<(), String> {
        if self.availability.closed.get() {
            return Err("WebRTC client pool is closed".to_string());
        }
        let mut receiver = self.availability_rx.lock().await;
        if self.availability.closed.get() {
            return Err("WebRTC client pool is closed".to_string());
        }
        if receiver.next().await.is_none() || self.availability.closed.get() {
            return Err("WebRTC client pool closed while waiting for capacity".to_string());
        }
        Ok(())
    }

    async fn client(&self, endpoint: &BrowserEndpoint) -> Result<BrowserClientLease, String> {
        let endpoint = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
            .map_err(|error| error.to_string())?;
        let key = endpoint.multiaddr.clone();
        loop {
            if self.availability.closed.get() {
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
                        let mut client = BrowserNodeClientCore::new(endpoint.clone());
                        client.dial_failures = Some(Rc::clone(&self.dial_failures));
                        let client = Rc::new(client);
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
                    availability: Rc::clone(&self.availability),
                });
            }
            self.wait_for_availability().await?;
        }
    }

    fn is_lookup_eligible(&self, peer: &str, endpoint: &BrowserEndpoint) -> bool {
        // Native allows an existing connection even when its advertised
        // address is in the dial-failure cache.
        if self
            .clients
            .borrow()
            .get(&endpoint.multiaddr)
            .is_some_and(|entry| entry.client.is_connected())
        {
            return true;
        }
        !self
            .dial_failures
            .borrow_mut()
            .is_suppressed(&peer.to_string(), &endpoint.multiaddr)
    }

    fn close(&self) {
        self.availability.close();
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

struct Connection {
    peer_connection: RtcPeerConnection,
    data_channel: RtcDataChannel,
    inbox: Rc<ResponseInbox>,
    pq_session: RefCell<Option<PqSession>>,
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

        let inbox = ResponseInbox::new();
        let message_inbox = Rc::clone(&inbox);
        let message_channel = data_channel.clone();
        let message_connection = peer_connection.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Err(error) = message_inbox.push(event.data()) {
                message_inbox.fail(error);
                message_channel.close();
                message_connection.close();
            }
        });
        data_channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let error_inbox = Rc::clone(&inbox);
        let on_error = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            error_inbox.fail("WebRTC DataChannel failed".to_string());
        });
        data_channel.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let close_inbox = Rc::clone(&inbox);
        let on_close = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            close_inbox.fail("WebRTC DataChannel closed".to_string());
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
            inbox,
            pq_session: RefCell::new(None),
            _on_message: on_message,
            _on_error: on_error,
            _on_close: on_close,
            _on_open: on_open,
        };

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
        let local = RtcSessionDescriptionInit::new(RtcSdpType::Offer);
        local.set_sdp(&offer_sdp);
        JsFuture::from(connection.peer_connection.set_local_description(&local))
            .await
            .map_err(js_error_message)?;
        let local_sdp = connection
            .peer_connection
            .local_description()
            .ok_or_else(|| "browser did not retain its local WebRTC offer".to_string())?
            .sdp();
        let client_pwd = ice_password_from_sdp(&local_sdp).map_err(|error| error.to_string())?;
        let server_credential =
            v2_server_ice_credential(&client_pwd).map_err(|error| error.to_string())?;
        let answer_sdp =
            server_answer_sdp(endpoint, &server_credential).map_err(|error| error.to_string())?;
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

        let session = establish_pq_session(&connection, endpoint).await?;
        connection.pq_session.replace(Some(session));

        Ok(connection)
    }

    fn close(self) {
        drop(self);
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.inbox.fail("WebRTC connection closed".to_string());
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
    dial_failures: Option<DialFailures>,
    endpoint: WebRtcDirectEndpoint,
    connection: RefCell<Option<Connection>>,
    request_lock: Mutex<()>,
    next_request_id: Cell<u64>,
    hello: RefCell<Option<BrowserHello>>,
    peer_id: RefCell<Option<String>>,
}

// The request lock serializes RPCs, but releasing that lock is not enough
// after cancellation: the next response still belongs to the canceled RPC.
// Declare this guard after the lock so it closes the association first.
struct PendingRequest<'a> {
    client: &'a BrowserNodeClientCore,
    completed: bool,
}

impl Drop for PendingRequest<'_> {
    fn drop(&mut self) {
        if !self.completed {
            self.client.close();
        }
    }
}

impl BrowserNodeClientCore {
    pub(super) fn new(endpoint: WebRtcDirectEndpoint) -> Self {
        Self {
            dial_failures: None,
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

    fn is_connected(&self) -> bool {
        self.connection.borrow().as_ref().is_some_and(|connection| {
            connection.data_channel.ready_state() == RtcDataChannelState::Open
        })
    }

    async fn ensure_connected(&self) -> Result<(), String> {
        if self.is_connected() {
            return Ok(());
        }
        self.close();
        if let Some(cache) = &self.dial_failures {
            if cache
                .borrow_mut()
                .is_suppressed(&self.endpoint.peer_id, &self.endpoint.multiaddr)
            {
                return Err("WebRTC endpoint is in the failed-connection cache".to_string());
            }
        }
        let connection = match Connection::open(&self.endpoint).await {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(cache) = &self.dial_failures {
                    cache.borrow_mut().record_failure(
                        self.endpoint.peer_id.clone(),
                        self.endpoint.multiaddr.clone(),
                    );
                }
                return Err(error);
            }
        };
        if let Some(cache) = &self.dial_failures {
            cache.borrow_mut().record_success(&self.endpoint.peer_id);
        }
        self.connection.replace(Some(connection));
        Ok(())
    }

    async fn request(
        &self,
        body: BrowserRequestBody,
        content: &[u8],
    ) -> Result<BrowserResponseFrame, String> {
        self.request_typed(body, content)
            .await
            .map_err(|error| error.to_string())
    }

    async fn request_typed(
        &self,
        body: BrowserRequestBody,
        content: &[u8],
    ) -> Result<BrowserResponseFrame, RpcError> {
        let _guard = self.request_lock.lock().await;
        self.ensure_connected().await?;
        let mut pending = PendingRequest {
            client: self,
            completed: false,
        };
        let request_id = self.next_request_id.get();
        self.next_request_id.set(request_id.wrapping_add(1).max(1));
        let request = BrowserRequest::new(request_id, body, content.len());
        let plaintext =
            encode_request_frame(&request, content).map_err(|error| error.to_string())?;
        let frame = {
            let connection = self.connection.borrow();
            let connection = connection
                .as_ref()
                .ok_or_else(|| "WebRTC DataChannel is not connected".to_string())?;
            connection
                .inbox
                .expect_response(MAX_BROWSER_RESPONSE_BYTES + PQ_ENCRYPTED_OVERHEAD_BYTES)?;
            let encrypted = connection
                .pq_session
                .borrow_mut()
                .as_mut()
                .ok_or_else(|| "WebRTC PQ session is not established".to_string())?
                .seal(&plaintext)
                .map_err(|error| error.to_string())?;
            encode_pq_frame(&encrypted).map_err(|error| error.to_string())?
        };
        let transfer_timeout_ms = transfer_timeout_ms(frame.len());
        let channel = {
            let connection = self.connection.borrow();
            connection
                .as_ref()
                .map(|connection| connection.data_channel.clone())
        };
        let Some(channel) = channel else {
            self.close();
            return Err("WebRTC DataChannel is not connected".to_string().into());
        };
        let send_result = send_data_channel_frame(&channel, &frame, transfer_timeout_ms).await;
        if let Err(error) = send_result {
            self.close();
            return Err(error.into());
        }
        let receiver = {
            let connection = self.connection.borrow();
            connection
                .as_ref()
                .map(|connection| Rc::clone(&connection.inbox))
        };
        let Some(receiver) = receiver else {
            self.close();
            return Err("WebRTC response inbox is unavailable".to_string().into());
        };
        let encrypted_response = match read_pq_payload_typed(
            receiver,
            MAX_BROWSER_RESPONSE_BYTES + PQ_ENCRYPTED_OVERHEAD_BYTES,
            transfer_timeout_ms,
        )
        .await
        {
            Ok(response) => response,
            Err(error) => {
                self.close();
                return Err(error);
            }
        };
        let decrypt_result = {
            let connection = self.connection.borrow();
            let Some(connection) = connection.as_ref() else {
                return Err("WebRTC DataChannel is not connected".to_string().into());
            };
            let mut pq_session = connection.pq_session.borrow_mut();
            pq_session
                .as_mut()
                .ok_or_else(|| "WebRTC PQ session is not established".to_string())?
                .open(&encrypted_response)
                .map_err(|error| error.to_string())
        };
        let plaintext_response = match decrypt_result {
            Ok(response) => response,
            Err(error) => {
                self.close();
                return Err(error.into());
            }
        };
        let response = match parse_response_frame(&plaintext_response) {
            Ok(response) => response,
            Err(error) => {
                self.close();
                return Err(error.to_string().into());
            }
        };
        if response.header.request_id != request_id {
            let error = format!(
                "response ID {} does not match request {request_id}",
                response.header.request_id
            );
            self.close();
            return Err(error.into());
        }
        // The complete response has been consumed and authenticated. An
        // ordinary application error can safely retain the session too.
        pending.completed = true;
        if response.header.status == BrowserResponseStatus::Error {
            let (code, message) = match &response.header.body {
                BrowserResponseBody::Error { code, message } => (code.clone(), message.clone()),
                _ => (
                    "invalid_response".into(),
                    "node returned an invalid error response".into(),
                ),
            };
            if code == "authentication_required" {
                self.close();
            }
            return Err(RpcError::Remote { code, message });
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
        let response = self.request(BrowserRequestBody::Hello, &[]).await?;
        let BrowserResponseBody::Hello {
            protocol,
            peer_id,
            max_chunk_size,
            endpoint,
            payment,
            capabilities,
        } = response.header.body
        else {
            self.close();
            return Err("expected a HELLO response".to_string());
        };
        let hello = BrowserHello {
            response_type: "hello".to_string(),
            protocol,
            peer_id,
            endpoint,
            max_chunk_size,
            capabilities,
            payment,
        };
        let peer_id = match validate_hello_metadata(&hello, &self.endpoint) {
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
        let response = self
            .request(
                BrowserRequestBody::FindNode {
                    target: target.clone(),
                    count: Some(count),
                },
                &[],
            )
            .await?;
        let BrowserResponseBody::Nodes {
            target: response_target,
            nodes,
        } = response.header.body
        else {
            return Err("expected a NODES response".to_string());
        };
        if response_target.to_ascii_lowercase() != target {
            return Err("node returned results for a different lookup target".to_string());
        }
        for node in &nodes {
            let peer_id = super::protocol::normalize_hex(&node.peer_id, 32)?;
            if let Some(endpoint) = &node.webrtc_direct {
                let endpoint = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
                    .map_err(|error| error.to_string())?;
                if endpoint.peer_id != peer_id {
                    return Err(format!("node {peer_id} advertised another peer's endpoint"));
                }
            }
        }
        Ok(nodes)
    }

    pub(super) async fn get_chunk(&self, address: &str) -> Result<(Vec<u8>, String), String> {
        self.try_get_chunk(address)
            .await?
            .ok_or_else(|| format!("chunk {address} was not found on this node"))
    }

    async fn try_get_chunk(&self, address: &str) -> Result<Option<(Vec<u8>, String)>, String> {
        let address = super::protocol::normalize_hex(address, 32)?;
        let response = self
            .request(
                BrowserRequestBody::GetChunk {
                    address: address.clone(),
                },
                &[],
            )
            .await?;
        if response.header.status == BrowserResponseStatus::NotFound {
            return Ok(None);
        }
        let BrowserResponseBody::Chunk {
            address: response_address,
            size,
        } = response.header.body
        else {
            return Err("expected a CHUNK response".to_string());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node returned a different chunk address".to_string());
        }
        if size != response.content.len() {
            return Err("chunk metadata size does not match its content".to_string());
        }
        super::verify_record(&address, &response.content).map_err(|error| error.to_string())?;
        Ok(Some((response.content, address)))
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
        let response = self
            .request(
                BrowserRequestBody::QuoteChunk {
                    address: address.clone(),
                    size: u64::try_from(size).map_err(|_| format!("invalid chunk size {size}"))?,
                },
                &[],
            )
            .await?;
        let BrowserResponseBody::StorageQuote {
            address: response_address,
            already_stored,
            quote,
        } = response.header.body
        else {
            return Err("expected a STORAGE_QUOTE response".to_string());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node returned a quote for a different chunk address".to_string());
        }
        Ok((quote, already_stored))
    }

    pub(super) async fn put_chunk(
        &self,
        address: &str,
        content: &[u8],
        quote: BrowserQuoteArtifact,
        transaction_hash: &str,
    ) -> Result<(String, bool), String> {
        self.put_chunk_typed(address, content, quote, transaction_hash)
            .await
            .map_err(|error| error.to_string())
    }

    pub(super) async fn put_chunk_typed(
        &self,
        address: &str,
        content: &[u8],
        quote: BrowserQuoteArtifact,
        transaction_hash: &str,
    ) -> Result<(String, bool), RpcError> {
        let address = super::protocol::normalize_hex(address, 32)?;
        let transaction_hash = super::protocol::normalize_hex(transaction_hash, 32)?;
        super::verify_record(&address, content).map_err(|error| error.to_string())?;
        let response = self
            .request_typed(
                BrowserRequestBody::PutChunk {
                    address: address.clone(),
                    quote: Box::new(quote),
                    transaction_hash,
                },
                content,
            )
            .await?;
        let BrowserResponseBody::ChunkStored {
            address: response_address,
            already_stored,
        } = response.header.body
        else {
            return Err("expected a CHUNK_STORED response".to_string().into());
        };
        if response_address.to_ascii_lowercase() != address {
            return Err("node stored a different chunk address".to_string().into());
        }
        Ok((address, already_stored))
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
        })
    }

    async fn find_closest(
        &self,
        target: &str,
        progress: &ProgressReporter,
    ) -> Result<BrowserLookupResult, String> {
        self.find_closest_with_count(target, progress, DEFAULT_LOOKUP_K)
            .await
    }

    async fn find_closest_with_count(
        &self,
        target: &str,
        progress: &ProgressReporter,
        count: usize,
    ) -> Result<BrowserLookupResult, String> {
        let target_key = parse_lookup_key(target, "lookup target")?;
        let failures = Rc::new(RefCell::new(Vec::new()));
        let views = Rc::new(RefCell::new(HashMap::new()));
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
                        peer_record: None,
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
            initial_candidates.extend(join_all(seed_futures).await.into_iter().flatten());
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

        let config = LookupConfig::saorsa(count);
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
            views: Rc::clone(&views),
            known_endpoints,
            routing: Rc::clone(&self.routing),
            reports: HashMap::new(),
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
        let records = lookup
            .results()
            .iter()
            .map(|candidate| shared::peer_record(&candidate.wire))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let nodes = client_routing::apply_lookup_report_winners(
            records,
            &query.reports,
            &target_key,
            count,
        )
        .into_iter()
        .map(shared::browser_record)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
        let queried = lookup.queried_peers().iter().map(hex::encode).collect();
        let failures = failures.borrow().clone();
        let views = views.borrow().clone();
        Ok(BrowserLookupResult {
            nodes,
            queried,
            failures,
            views,
        })
    }
}

struct BrowserNetworkLookupQuery {
    pool: Rc<BrowserClientPool>,
    progress: ProgressReporter,
    failures: Rc<RefCell<Vec<BrowserLookupFailure>>>,
    views: Rc<RefCell<HashMap<LookupKey, Vec<BrowserNode>>>>,
    known_endpoints: HashMap<LookupKey, BrowserEndpoint>,
    reports: HashMap<PeerId, client_routing::SubjectReports>,
    routing: Rc<RefCell<HashMap<LookupKey, BrowserLookupCandidate>>>,
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
        Ok(self
            .pool
            .is_lookup_eligible(&candidate.wire.peer_id, endpoint))
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
            .map(|candidate| candidate.peer_id)
            .collect::<HashSet<_>>();
        let futures: FuturesUnordered<_> = batch
            .into_iter()
            .map(|candidate| {
                let pool = Rc::clone(&self.pool);
                let progress = self.progress.clone();
                let failures = Rc::clone(&self.failures);
                let views = Rc::clone(&self.views);
                let target = target.clone();
                async move {
                    let responder = candidate.peer_id;
                    let peer_id = candidate.wire.peer_id.clone();
                    let result = async {
                        let endpoint = candidate.wire.webrtc_direct.as_ref().ok_or_else(|| {
                            "lookup candidate has no WebRTC Direct endpoint".to_string()
                        })?;
                        let client = pool.client(endpoint).await?;
                        client.hello().await?;
                        client
                            .find_node(
                                &target,
                                count.max(crate::quote_policy::SINGLE_NODE_WITNESSED_VIEW_COUNT),
                            )
                            .await
                    }
                    .await;
                    match result {
                        Ok(nodes) => {
                            views.borrow_mut().insert(responder, nodes.clone());
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
        let mut outcomes = collect_after_first_with_grace(futures, || {
            crate::runtime::sleep(Duration::from_secs(
                ant_protocol::transport::ITERATION_GRACE_TIMEOUT_SECS,
            ))
        })
        .await;
        let responded = outcomes
            .iter()
            .map(|outcome| *outcome.responder())
            .collect::<HashSet<_>>();
        for peer in attempted {
            if !responded.contains(&peer) {
                // A grace-cancelled RPC says nothing about whether dialing
                // this endpoint works. Native does not poison its dial cache
                // here; only Connection::open failures populate that cache.
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
            if let LookupQueryOutcome::Succeeded {
                responder,
                candidates,
            } = outcome
            {
                let responder = PeerId::from_bytes(*responder);
                candidates.retain_mut(|candidate| {
                    let Ok(record) = shared::peer_record(&candidate.wire) else {
                        return false;
                    };
                    let subject = record.peer_id;
                    let reports = self.reports.entry(subject).or_default();
                    reports.insert(responder, record);
                    let Some((_, winner)) = client_routing::compute_winner(&subject, reports)
                    else {
                        return false;
                    };
                    let Ok(wire) = shared::browser_record(winner.clone()) else {
                        return false;
                    };
                    candidate.wire = wire;
                    // A missing endpoint affects dialing, never the witness transcript above.
                    if let Some(endpoint) = candidate.wire.webrtc_direct.clone() {
                        self.known_endpoints.insert(candidate.peer_id, endpoint);
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
    file: PublicFileDescriptor,
    #[serde(rename = "dataMapNode")]
    data_map_node: BrowserNode,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BrowserPublicFileInput {
    Descriptor(PublicFileDescriptor),
    Address(String),
}

impl BrowserPublicFileInput {
    fn into_address_and_descriptor(self) -> (String, Option<PublicFileDescriptor>) {
        match self {
            Self::Descriptor(file) => (file.address.clone(), Some(file)),
            Self::Address(address) => (address, None),
        }
    }
}

struct ResolvedBrowserPublicFile {
    file: PublicFileDescriptor,
    expected_hash: Option<String>,
    data_map_node: BrowserNode,
    root_data_map: self_encryption::DataMap,
}

struct UploadRecord {
    address: String,
    size: usize,
    content: Option<Rc<Vec<u8>>>,
}

impl From<BrowserRecord> for UploadRecord {
    fn from(record: BrowserRecord) -> Self {
        let size = record.content.len();
        Self {
            address: record.address,
            size,
            content: Some(Rc::new(record.content)),
        }
    }
}

impl From<BrowserRecordInfo> for UploadRecord {
    fn from(record: BrowserRecordInfo) -> Self {
        Self {
            address: record.address,
            size: record.size,
            content: None,
        }
    }
}

#[derive(Debug, Deserialize)]
struct BrowserPaymentSubmission {
    #[serde(default, rename = "transactionHashes")]
    transaction_hashes: HashMap<String, String>,
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

struct BrowserStoredRecords {
    payment: BrowserPaymentSubmission,
    replicas: usize,
    records: usize,
}

/// Random-access public-file reader for media playback and bounded downloads.
#[wasm_bindgen(js_name = BrowserFileReader)]
pub struct BrowserFileReader {
    shared: Rc<crate::data::Client>,
    file: PublicFileDescriptor,
    root_data_map: self_encryption::DataMap,
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
        self.shared.chunk_cache().clear();
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
        self.shared
            .data_download_range(&self.root_data_map, start, length)
            .await
            .map(|bytes| bytes.to_vec())
            .map_err(|error| error.to_string())
    }
}

#[derive(Default)]
struct UploadCheckpoint {
    mode: crate::data::client::merkle::PaymentMode,
    merkle_wallet: Option<js_sys::Function>,
    snapshot: Option<String>,
    callback: Option<js_sys::Function>,
}

#[derive(Serialize, Deserialize)]
struct UploadCheckpointEnvelope {
    scope: String,
    state: String,
}

impl UploadCheckpoint {
    fn restore(
        &self,
        scope: &str,
    ) -> Result<crate::data::client::upload_state::UploadState, String> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(Default::default());
        };
        if snapshot.len() > 128 * 1024 * 1024 {
            return Err("upload checkpoint too large".into());
        }
        let envelope: UploadCheckpointEnvelope =
            serde_json::from_str(snapshot).map_err(|e| e.to_string())?;
        if envelope.scope != scope {
            return Err("upload checkpoint belongs to a different file or payment network".into());
        }
        let bytes = hex::decode(&envelope.state).map_err(|e| e.to_string())?;
        crate::data::client::upload_state::UploadState::restore(&bytes).map_err(|e| e.to_string())
    }

    async fn save(
        &self,
        scope: &str,
        state: &crate::data::client::upload_state::UploadState,
    ) -> Result<(), String> {
        if let Some(callback) = &self.callback {
            let envelope = UploadCheckpointEnvelope {
                scope: scope.into(),
                state: hex::encode(state.checkpoint().map_err(|e| e.to_string())?),
            };
            let value = serde_json::to_string(&envelope).map_err(|e| e.to_string())?;
            let result = callback
                .call1(&JsValue::NULL, &JsValue::from_str(&value))
                .map_err(js_error_message)?;
            JsFuture::from(Promise::resolve(&result))
                .await
                .map_err(js_error_message)?;
        }
        Ok(())
    }
}

/// Stateful Autonomi browser client sharing Rust lookup and data workflows.
#[wasm_bindgen(js_name = BrowserNetworkClient)]
pub struct BrowserNetworkClient {
    inner: Rc<BrowserNetworkCore>,
    shared: Rc<crate::data::Client>,
    adapter: Rc<SharedNetworkAdapter>,
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
        let inner = Rc::new(inner);
        let adapter = Rc::new(SharedNetworkAdapter::new(Rc::clone(&inner)));
        let network = crate::data::Network::from_browser(adapter.clone());
        let shared = Rc::new(
            crate::data::Client::from_network(network, crate::data::ClientConfig::default())
                .with_chunk_cache(crate::data::ChunkCache::new(
                    MAX_RANGE_CACHE_BYTES / MAX_BROWSER_RECORD_BYTES,
                )),
        );
        Ok(Self {
            inner,
            shared,
            adapter,
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
        let file: BrowserPublicFileInput = serde_wasm_bindgen::from_value(file)
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
        let file: BrowserPublicFileInput = serde_wasm_bindgen::from_value(file)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        self.open_public_file_inner(file, progress)
            .await
            .map_err(|error| JsValue::from_str(&error))
    }

    /// Self-encrypt, quote, pay through a wallet callback, and store a public file.
    #[wasm_bindgen(js_name = uploadPublicFile)]
    #[allow(clippy::too_many_arguments)] // Preserve the existing JS argument order; recovery is additive.
    pub async fn upload_public_file(
        &self,
        content: &[u8],
        name: &str,
        content_type: &str,
        payment_network: JsValue,
        pay_for_quotes: js_sys::Function,
        on_progress: Option<js_sys::Function>,
        checkpoint: Option<String>,
        on_checkpoint: Option<js_sys::Function>,
        payment_mode: Option<String>,
        pay_for_merkle: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let payment_network: BrowserPaymentNetwork =
            serde_wasm_bindgen::from_value(payment_network)
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let payment_network = validate_browser_payment_network(payment_network)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        let checkpoint = UploadCheckpoint {
            mode: match payment_mode.as_deref().unwrap_or("auto") {
                "auto" => crate::data::client::merkle::PaymentMode::Auto,
                "single" => crate::data::client::merkle::PaymentMode::Single,
                "merkle" => crate::data::client::merkle::PaymentMode::Merkle,
                _ => return Err(JsValue::from_str("unknown payment mode")),
            },
            merkle_wallet: pay_for_merkle,
            snapshot: checkpoint,
            callback: on_checkpoint,
        };
        let result = self
            .upload_public_file_inner(
                content,
                name,
                content_type,
                payment_network,
                &pay_for_quotes,
                &progress,
                &checkpoint,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&result).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Quote, pay for, and upload records produced by `BrowserFileEncryptor`.
    ///
    /// Record bytes are requested lazily from the asynchronous JavaScript
    /// callback, allowing the page to keep them in IndexedDB rather than WASM.
    #[wasm_bindgen(js_name = uploadStagedPublicFile)]
    #[allow(clippy::too_many_arguments)] // Preserve the existing JS argument order; recovery is additive.
    pub async fn upload_staged_public_file(
        &self,
        staged: JsValue,
        payment_network: JsValue,
        load_record: js_sys::Function,
        pay_for_quotes: js_sys::Function,
        on_progress: Option<js_sys::Function>,
        checkpoint: Option<String>,
        on_checkpoint: Option<js_sys::Function>,
        payment_mode: Option<String>,
        pay_for_merkle: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let staged: BrowserStagedFile = serde_wasm_bindgen::from_value(staged)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let payment_network: BrowserPaymentNetwork =
            serde_wasm_bindgen::from_value(payment_network)
                .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let payment_network = validate_browser_payment_network(payment_network)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        let checkpoint = UploadCheckpoint {
            mode: match payment_mode.as_deref().unwrap_or("auto") {
                "auto" => crate::data::client::merkle::PaymentMode::Auto,
                "single" => crate::data::client::merkle::PaymentMode::Single,
                "merkle" => crate::data::client::merkle::PaymentMode::Merkle,
                _ => return Err(JsValue::from_str("unknown payment mode")),
            },
            merkle_wallet: pay_for_merkle,
            snapshot: checkpoint,
            callback: on_checkpoint,
        };
        let result = self
            .upload_staged_public_file_inner(
                staged,
                payment_network,
                &load_record,
                &pay_for_quotes,
                &progress,
                &checkpoint,
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
    async fn get_shared_chunk(
        &self,
        address: &str,
        progress: &ProgressReporter,
    ) -> Result<(Vec<u8>, BrowserNode), String> {
        let key = parse_lookup_key(address, "record address")?;
        progress.report(&format!("Fetching {address}"));
        // Source metadata is bounded separately from the content cache. A
        // metadata miss refreshes the record through the authenticated adapter.
        if !self.adapter.sources.borrow().contains(&key) {
            self.shared.chunk_cache().remove(&key);
        }
        let chunk = self
            .shared
            .chunk_get(&key)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("record {address} not found"))?;
        let node = self
            .adapter
            .sources
            .borrow_mut()
            .get(&key)
            .cloned()
            .ok_or("record source metadata missing")?;
        Ok((chunk.content.to_vec(), node))
    }

    async fn open_public_file_inner(
        &self,
        file: BrowserPublicFileInput,
        progress: ProgressReporter,
    ) -> Result<BrowserFileReader, String> {
        let resolved = self.resolve_public_file(file, &progress).await?;
        let file = resolved.file;
        progress.report(&format!(
            "Ready to stream {} ({} bytes, {} chunks)",
            file.name,
            file.size,
            file.chunks.len()
        ));
        Ok(BrowserFileReader {
            shared: Rc::clone(&self.shared),
            file,
            root_data_map: resolved.root_data_map,
            closed: Cell::new(false),
        })
    }

    async fn download_public_file_inner(
        &self,
        file: BrowserPublicFileInput,
        concurrency: usize,
        progress: &ProgressReporter,
    ) -> Result<BrowserDownloadResult, String> {
        if concurrency == 0 {
            return Err("download concurrency must be a positive integer".to_string());
        }
        let concurrency = concurrency.min(MAX_DOWNLOAD_CONCURRENCY);
        let mut resolved = self.resolve_public_file(file, progress).await?;
        let content = self
            .shared
            .data_download_with_concurrency(&resolved.root_data_map, concurrency)
            .await
            .map_err(|error| error.to_string())?
            .to_vec();
        if content.len() != resolved.file.size {
            return Err(format!(
                "reconstructed file has {} bytes, expected {}",
                content.len(),
                resolved.file.size
            ));
        }
        let hash = hex::encode(blake3::hash(&content).as_bytes());
        if let Some(expected_hash) = resolved.expected_hash.as_ref() {
            super::verify_record(expected_hash, &content).map_err(|error| error.to_string())?;
        }
        resolved.file.blake3 = hash.clone();
        progress.report(&format!(
            "Verified complete {} as {hash}",
            resolved.file.name
        ));
        Ok(BrowserDownloadResult {
            content,
            hash,
            file: resolved.file,
            data_map_node: resolved.data_map_node,
        })
    }

    async fn resolve_public_file(
        &self,
        file: BrowserPublicFileInput,
        progress: &ProgressReporter,
    ) -> Result<ResolvedBrowserPublicFile, String> {
        let (address, descriptor) = file.into_address_and_descriptor();
        let address = super::protocol::normalize_hex(&address, 32)?;
        progress.report(&format!("Fetching public DataMap {address}"));
        let (encoded_data_map, data_map_node) = self.get_shared_chunk(&address, progress).await?;
        progress.report(&format!(
            "Verified public DataMap ({} bytes)",
            encoded_data_map.len()
        ));
        let published_data_map = crate::client_engine::files::decode_map(&encoded_data_map)?;
        let root_data_map = crate::client_engine::files::resolve(
            &published_data_map,
            &|address| async move {
                progress.report(&format!(
                    "Resolving nested DataMap record {}",
                    hex::encode(address)
                ));
                self.get_shared_chunk(&hex::encode(address), progress)
                    .await
                    .map(|(content, _)| bytes::Bytes::from(content))
            },
            &|| {
                self.shared
                    .controller()
                    .fetch
                    .current()
                    .min(MAX_DOWNLOAD_CONCURRENCY)
            },
        )
        .await
        .map_err(|error| error.to_string())?;

        let mut actual_chunks = super::chunk_infos(&root_data_map);
        actual_chunks.sort_by_key(|chunk| chunk.index);
        if actual_chunks.len() < 3 {
            return Err("ant-core returned an invalid public DataMap".to_string());
        }
        let resolved_size = actual_chunks.iter().try_fold(0usize, |total, chunk| {
            total
                .checked_add(chunk.src_size)
                .ok_or_else(|| "resolved public file size overflow".to_string())
        })?;
        if !(self_encryption::MIN_ENCRYPTABLE_BYTES..=super::MAX_BROWSER_FILE_BYTES)
            .contains(&resolved_size)
        {
            return Err(format!("invalid public file size {resolved_size}"));
        }

        let (file, expected_hash) = if let Some(mut file) = descriptor {
            file.address = super::protocol::normalize_hex(&file.address, 32)?;
            file.blake3 = super::protocol::normalize_hex(&file.blake3, 32)?;
            if file.name.is_empty() {
                return Err("public file has no name".to_string());
            }
            if file.data_map_size != encoded_data_map.len() {
                return Err(format!(
                    "public DataMap has {} bytes, expected {}",
                    encoded_data_map.len(),
                    file.data_map_size
                ));
            }
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
            if resolved_size != file.size {
                return Err(format!(
                    "resolved public file has {resolved_size} bytes, expected {}",
                    file.size
                ));
            }
            file.content_type = normalized_content_type(&file.content_type);
            file.chunks = actual_chunks;
            let expected_hash = file.blake3.clone();
            (file, Some(expected_hash))
        } else {
            (
                PublicFileDescriptor {
                    name: fallback_public_file_name(&address),
                    address,
                    size: resolved_size,
                    content_type: "application/octet-stream".to_string(),
                    blake3: String::new(),
                    data_map_size: encoded_data_map.len(),
                    chunks: actual_chunks,
                    replicas: 0,
                },
                None,
            )
        };

        Ok(ResolvedBrowserPublicFile {
            file,
            expected_hash,
            data_map_node,
            root_data_map,
        })
    }

    #[allow(clippy::too_many_arguments)] // Preserve the existing JS argument order; recovery is additive.
    async fn upload_public_file_inner(
        &self,
        content: &[u8],
        name: &str,
        content_type: &str,
        payment_network: BrowserPaymentNetwork,
        pay_for_quotes: &js_sys::Function,
        progress: &ProgressReporter,
        checkpoint: &UploadCheckpoint,
    ) -> Result<BrowserUploadResult, String> {
        if name.is_empty() {
            return Err("upload file has no name".to_string());
        }
        progress.report(&format!(
            "Self-encrypting {name} with native ant-core WASM ({} bytes)",
            content.len()
        ));
        let encrypted = super::encrypt_public_file(content).map_err(|error| error.to_string())?;
        let records = encrypted
            .records
            .into_iter()
            .map(UploadRecord::from)
            .collect::<Vec<_>>();
        let stored = self
            .prepare_pay_and_store_records(
                records,
                &payment_network,
                None,
                pay_for_quotes,
                progress,
                checkpoint,
            )
            .await?;
        let descriptor = PublicFileDescriptor {
            name: name.to_string(),
            address: encrypted.address,
            size: content.len(),
            content_type: normalized_content_type(content_type),
            blake3: encrypted.blake3,
            data_map_size: encrypted.data_map_size,
            chunks: encrypted.chunks,
            replicas: stored.replicas,
        };
        Ok(BrowserUploadResult {
            file: descriptor,
            transaction_hash: stored.payment.transaction_hash,
            storage_cost_atto: stored.payment.total_amount,
            records: stored.records,
        })
    }

    async fn upload_staged_public_file_inner(
        &self,
        mut staged: BrowserStagedFile,
        payment_network: BrowserPaymentNetwork,
        load_record: &js_sys::Function,
        pay_for_quotes: &js_sys::Function,
        progress: &ProgressReporter,
        checkpoint: &UploadCheckpoint,
    ) -> Result<BrowserUploadResult, String> {
        validate_staged_file(&mut staged)?;
        progress.report(&format!(
            "Preparing paid upload for staged {} ({} bytes, {} records)",
            staged.name,
            staged.size,
            staged.records.len()
        ));
        let records = staged
            .records
            .into_iter()
            .map(UploadRecord::from)
            .collect::<Vec<_>>();
        let stored = self
            .prepare_pay_and_store_records(
                records,
                &payment_network,
                Some(load_record),
                pay_for_quotes,
                progress,
                checkpoint,
            )
            .await?;
        let descriptor = PublicFileDescriptor {
            name: staged.name,
            address: staged.address,
            size: staged.size,
            content_type: staged.content_type,
            blake3: staged.blake3,
            data_map_size: staged.data_map_size,
            chunks: staged.chunks,
            replicas: stored.replicas,
        };
        Ok(BrowserUploadResult {
            file: descriptor,
            transaction_hash: stored.payment.transaction_hash,
            storage_cost_atto: stored.payment.total_amount,
            records: stored.records,
        })
    }

    async fn prepare_pay_and_store_records(
        &self,
        records: Vec<UploadRecord>,
        payment_network: &BrowserPaymentNetwork,
        load_record: Option<&js_sys::Function>,
        pay_for_quotes: &js_sys::Function,
        progress: &ProgressReporter,
        checkpoint: &UploadCheckpoint,
    ) -> Result<BrowserStoredRecords, String> {
        let count = records.len();
        let scope = hex::encode(
            blake3::hash(
                &serde_json::to_vec(&(
                    payment_network,
                    records
                        .iter()
                        .map(|r| (&r.address, r.size))
                        .collect::<Vec<_>>(),
                ))
                .map_err(|e| e.to_string())?,
            )
            .as_bytes(),
        );
        let mut state = checkpoint.restore(&scope)?;
        let mut network = SharedNetworkAdapter::new(Rc::clone(&self.inner));
        network.payment_network = Some(payment_network.clone());
        let client = crate::data::Client::from_network(
            crate::data::Network::from_browser(Rc::new(network)),
            crate::data::ClientConfig::default(),
        );
        let metadata = records
            .iter()
            .enumerate()
            .map(|(index, record)| {
                Ok(crate::data::client::upload::UploadRecord {
                    address: parse_lookup_key(&record.address, "record address")?,
                    size: record.size as u64,
                    index,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let adapter = upload_adapter::BrowserUploadAdapter {
            network: self,
            records: &records,
            payment_network,
            loader: load_record,
            wallet: pay_for_quotes,
            merkle_wallet: checkpoint.merkle_wallet.as_ref(),
            progress,
            checkpoint,
            scope: &scope,
            last_transaction: RefCell::new(None),
        };
        let result = client
            .upload_records(metadata, &mut state, &adapter, checkpoint.mode)
            .await
            .map_err(|error| error.to_string())?;
        let transaction_hash = adapter.last_transaction.into_inner();
        Ok(BrowserStoredRecords {
            payment: BrowserPaymentSubmission {
                transaction_hash,
                transaction_hashes: HashMap::new(),
                total_amount: result.amount.to_string(),
            },
            replicas: CLOSE_GROUP_MAJORITY,
            records: count,
        })
    }
}

fn normalized_content_type(content_type: &str) -> String {
    if content_type.is_empty() {
        "application/octet-stream".to_string()
    } else {
        content_type.to_string()
    }
}

fn fallback_public_file_name(address: &str) -> String {
    format!("public-file-{}.bin", &address[..16])
}

fn validate_staged_file(staged: &mut BrowserStagedFile) -> Result<(), String> {
    if staged.name.is_empty() {
        return Err("upload file has no name".to_string());
    }
    staged.content_type = normalized_content_type(&staged.content_type);
    if staged.size < self_encryption::MIN_ENCRYPTABLE_BYTES
        || staged.size > super::MAX_BROWSER_FILE_BYTES
    {
        return Err(format!("invalid staged file size {}", staged.size));
    }
    if staged.records.is_empty() {
        return Err("staged upload contains no records".to_string());
    }
    // A 1 GB self-encrypted file currently needs only a few hundred records.
    // Keep malformed JavaScript metadata from creating unbounded quote work.
    if staged.records.len() > 4096 {
        return Err("staged upload contains too many records".to_string());
    }

    staged.address = super::protocol::normalize_hex(&staged.address, 32)?;
    staged.blake3 = super::protocol::normalize_hex(&staged.blake3, 32)?;
    for record in &mut staged.records {
        record.address = super::protocol::normalize_hex(&record.address, 32)?;
        if record.size == 0 || record.size > MAX_BROWSER_RECORD_BYTES {
            return Err(format!(
                "staged record {} has invalid size {}",
                record.address, record.size
            ));
        }
    }
    let public_data_map = staged
        .records
        .last()
        .ok_or_else(|| "staged upload contains no public DataMap".to_string())?;
    if public_data_map.address != staged.address || public_data_map.size != staged.data_map_size {
        return Err("staged public DataMap metadata does not match its record".to_string());
    }
    for chunk in &mut staged.chunks {
        chunk.dst_hash = super::protocol::normalize_hex(&chunk.dst_hash, 32)?;
        chunk.src_hash = super::protocol::normalize_hex(&chunk.src_hash, 32)?;
    }
    Ok(())
}

async fn load_upload_record(
    index: usize,
    record: &UploadRecord,
    loader: Option<&js_sys::Function>,
) -> Result<Rc<Vec<u8>>, String> {
    let content = if let Some(content) = &record.content {
        Rc::clone(content)
    } else {
        let loader = loader.ok_or_else(|| "staged record loader is unavailable".to_string())?;
        let returned = loader
            .call3(
                &JsValue::NULL,
                &JsValue::from_f64(index as f64),
                &JsValue::from_str(&record.address),
                &JsValue::from_f64(record.size as f64),
            )
            .map_err(js_error_message)?;
        let returned = JsFuture::from(Promise::resolve(&returned))
            .await
            .map_err(js_error_message)?;
        if !returned.is_instance_of::<Uint8Array>() {
            return Err(format!(
                "staged record loader returned a non-Uint8Array for record {}",
                index + 1
            ));
        }
        let returned = Uint8Array::new(&returned);
        if returned.length() as usize != record.size {
            return Err(format!(
                "staged record {} has {} bytes, expected {}",
                index + 1,
                returned.length(),
                record.size
            ));
        }
        let mut content = vec![0u8; record.size];
        returned.copy_to(&mut content);
        Rc::new(content)
    };
    super::verify_record(&record.address, content.as_slice()).map_err(|error| error.to_string())?;
    Ok(content)
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

async fn establish_pq_session(
    connection: &Connection,
    endpoint: &WebRtcDirectEndpoint,
) -> Result<PqSession, String> {
    let expected_peer_id: [u8; 32] = hex::decode(&endpoint.peer_id)
        .map_err(|error| format!("invalid endpoint peer ID: {error}"))?
        .try_into()
        .map_err(|peer_id: Vec<u8>| {
            format!("endpoint peer ID is {} bytes; expected 32", peer_id.len())
        })?;
    let (handshake, client_hello) =
        PqClientHandshake::start().map_err(|error| error.to_string())?;
    let client_hello = encode_pq_frame(&client_hello).map_err(|error| error.to_string())?;
    connection.inbox.expect_response(PQ_SERVER_ACCEPT_BYTES)?;
    send_data_channel_frame(&connection.data_channel, &client_hello, REQUEST_TIMEOUT_MS).await?;
    let server_accept = read_pq_payload(
        Rc::clone(&connection.inbox),
        PQ_SERVER_ACCEPT_BYTES,
        REQUEST_TIMEOUT_MS,
    )
    .await?;
    handshake
        .finish(&server_accept, &expected_peer_id)
        .map_err(|error| error.to_string())
}

async fn send_data_channel_frame(
    channel: &RtcDataChannel,
    frame: &[u8],
    timeout_ms: u32,
) -> Result<(), String> {
    let send_deadline_ms = js_sys::Date::now() + f64::from(timeout_ms);
    for message in frame.chunks(WEBRTC_WRITE_CHUNK_BYTES) {
        wait_for_capacity(channel, remaining_timeout_ms(send_deadline_ms)).await?;
        channel
            .send_with_u8_array(message)
            .map_err(js_error_message)?;
    }
    Ok(())
}

async fn read_pq_payload(
    receiver: Rc<ResponseInbox>,
    max_payload_bytes: usize,
    initial_timeout_ms: u32,
) -> Result<Vec<u8>, String> {
    read_pq_payload_typed(receiver, max_payload_bytes, initial_timeout_ms)
        .await
        .map_err(|error| error.to_string())
}

async fn read_pq_payload_typed(
    receiver: Rc<ResponseInbox>,
    max_payload_bytes: usize,
    initial_timeout_ms: u32,
) -> Result<Vec<u8>, RpcError> {
    let mut frame = Vec::with_capacity(8 * 1024);
    let mut expected_length = None;
    let response_started_ms = js_sys::Date::now();
    let mut response_deadline_ms = response_started_ms + f64::from(initial_timeout_ms);
    loop {
        let remaining_ms = remaining_timeout_ms(response_deadline_ms);
        let message = match select(
            Box::pin(receiver.next()),
            Box::pin(TimeoutFuture::new(remaining_ms)),
        )
        .await
        {
            Either::Left((result, _)) => result?,
            Either::Right(((), _)) => {
                return Err(RpcError::Timeout("WebRTC request timed out".into()))
            }
        };
        let next_length = frame
            .len()
            .checked_add(message.len())
            .ok_or_else(|| "response length overflow".to_string())?;
        let max_frame_bytes = max_payload_bytes
            .checked_add(4)
            .ok_or_else(|| "PQ frame limit overflow".to_string())?;
        if next_length > max_frame_bytes {
            return Err(
                format!("PQ frame exceeded the {max_payload_bytes}-byte payload limit").into(),
            );
        }
        frame.extend_from_slice(&message);
        if expected_length.is_none() {
            expected_length =
                pq_frame_length(&frame, max_payload_bytes).map_err(|error| error.to_string())?;
            if let Some(expected) = expected_length {
                response_deadline_ms = response_deadline_ms
                    .max(response_started_ms + f64::from(transfer_timeout_ms(expected)));
            }
        }
        if let Some(expected) = expected_length {
            if frame.len() > expected {
                return Err("PQ frame contains bytes after its declared payload"
                    .to_string()
                    .into());
            }
            if frame.len() == expected {
                receiver.finish_response()?;
                return decode_pq_frame(&frame, max_payload_bytes)
                    .map_err(|error| RpcError::Transport(error.to_string()));
            }
        }
    }
}

struct CapacityListener {
    channel: RtcDataChannel,
    callback: Closure<dyn FnMut(Event)>,
}

impl Drop for CapacityListener {
    fn drop(&mut self) {
        // This also runs when the send future is canceled while draining.
        // Detach before the Rust closure is freed.
        self.channel.set_onbufferedamountlow(None);
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
    let listener = CapacityListener {
        channel: channel.clone(),
        callback: on_ready,
    };
    channel.set_onbufferedamountlow(Some(listener.callback.as_ref().unchecked_ref()));
    // The buffer can cross the threshold between the first check and callback
    // installation. Re-check after installing it so that race cannot turn a
    // completed drain into a full transfer-timeout wait.
    if channel.buffered_amount() <= MAX_BUFFERED_AMOUNT {
        if let Some(sender) = sender.borrow_mut().take() {
            let _ = sender.send(());
        }
    }
    timeout_with_ms(
        async move {
            receiver
                .await
                .map_err(|_| "WebRTC DataChannel closed while draining".to_string())
        },
        "WebRTC DataChannel drain timed out",
        timeout_ms,
    )
    .await
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
