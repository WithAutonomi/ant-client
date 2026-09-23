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
use crate::data::client::merkle::PaymentMode;
#[cfg(feature = "test-utils")]
use crate::transfer_policy::PutRejection;
use crate::transfer_policy::RpcError;
#[cfg(feature = "test-utils")]
use ant_protocol::transport::xor_distance;
use ant_protocol::transport::{
    collect_after_first_with_grace, run_iterative_lookup, IterativeLookup, LookupConfig, LookupKey,
    LookupNode, LookupQuery, LookupQueryOutcome,
};
use futures_channel::oneshot;
use futures_util::{
    future::{join_all, select, Either},
    lock::{Mutex, MutexGuard},
    stream::FuturesUnordered,
};
use gloo_timers::future::TimeoutFuture;
use js_sys::{Array, Promise, Uint8Array};
use saorsa_transport::webrtc::{
    decode_pq_frame, encode_pq_frame, pq_frame_length, transfer_timeout, PqClientHandshake,
    PqSession, TransferDeadline, PQ_ENCRYPTED_OVERHEAD_BYTES, PQ_SERVER_ACCEPT_BYTES,
};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::Deref;
use std::rc::{Rc, Weak};
use std::time::Duration;
use tokio::sync::watch;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    Event, MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelInit, RtcDataChannelState,
    RtcDataChannelType, RtcPeerConnection, RtcSdpType, RtcSessionDescriptionInit,
};

const REQUEST_TIMEOUT_MS: u32 = 10_000;
// A queued operation may wait behind a maximum-sized request and response
// (180 seconds each), plus connection/authentication setup. Admission has its
// own ceiling and never spends the caller's response allowance.
const RPC_ADMISSION_TIMEOUT: Duration = Duration::from_secs(400);
const CONNECTION_SETUP_TIMEOUT_MS: u32 = 30_000;
const MAX_BUFFERED_AMOUNT: u32 = 2 * 1024 * 1024;
// Retain useful associations across concurrent close-group walks. Active leases
// remain non-evictable, and the pool still imposes a hard resource bound.
const DEFAULT_MAX_POOLED_CLIENTS: usize = 64;
const MAX_LOOKUP_PRECONNECTS: usize = 8;
const ENDPOINT_FAILURE_COOLDOWN: Duration = Duration::from_secs(30 * 60);
const MAX_BROWSER_ROUTING_ENTRIES: usize = 256;
const MAX_BROWSER_ENDPOINT_FAILURES: usize = 256;
const DEFAULT_BROWSER_QUOTE_CONCURRENCY: usize = 4;
// Reserve encrypted input, decrypted frame and decoded content for every
// physical GET, including speculative reads. This bounds transient response
// memory; full-file retention is a separate API-level cost.
const MAX_READ_RESPONSE_MEMORY: usize = 128 * 1024 * 1024;
const READ_RESPONSE_RESERVATION: usize =
    3 * (MAX_BROWSER_RESPONSE_BYTES + PQ_ENCRYPTED_OVERHEAD_BYTES);
const MAX_BROWSER_RANGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_RANGE_CACHE_BYTES: usize = 32 * 1024 * 1024;
const MAX_UPLOAD_CHECKPOINT_BYTES: usize = 128 * 1024 * 1024;
// A 1 GB self-encrypted file needs only a few hundred records. Keep malformed
// JavaScript metadata from creating unbounded quote work.
const MAX_UPLOAD_RECORDS: usize = 4096;

mod failed_payment;
mod inbox;
mod multiplex;
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
    data_client: Rc<BrowserNodeClientCore>,
    last_used: u64,
}

type DialFailures = Rc<RefCell<crate::client_engine::EndpointFailureCache<String>>>;

struct BrowserClientPool {
    read_budget: std::sync::Arc<crate::client_engine::read_budget::ReadBudget>,
    fetch_limiter: RefCell<Option<crate::data::client::adaptive::Limiter>>,
    dial_failures: DialFailures,
    max_clients: usize,
    clients: RefCell<HashMap<String, PoolEntry>>,
    clock: Cell<u64>,
    availability: Rc<PoolAvailability>,
    preconnecting: RefCell<HashSet<String>>,
    #[cfg(feature = "test-utils")]
    preconnect_errors: RefCell<Vec<String>>,
}

struct BrowserClientLease {
    client: Rc<BrowserNodeClientCore>,
    availability: Rc<PoolAvailability>,
    admission: TransferDeadline,
}

struct PoolAvailability {
    closed: Cell<bool>,
    sender: watch::Sender<()>,
}

impl PoolAvailability {
    async fn wait_closed(&self) {
        let mut changed = self.sender.subscribe();
        while !self.closed.get() {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }

    fn notify_waiters(&self) {
        if self.closed.get() {
            return;
        }
        // A watch notification has no backlog. Every registered waiter rechecks
        // capacity, including when several leases are released together.
        self.sender.send_replace(());
    }

    fn close(&self) {
        self.closed.set(true);
        self.sender.send_replace(());
    }
}

impl Deref for BrowserClientLease {
    type Target = BrowserNodeClientCore;

    fn deref(&self) -> &Self::Target {
        &self.client
    }
}

impl BrowserClientLease {
    async fn authenticated(&self) -> Result<LockedBrowserClient<'_>, RpcError> {
        self.client.authenticated_before(&self.admission).await
    }

    async fn hello(&self) -> Result<BrowserHello, String> {
        self.authenticated()
            .await
            .map(|client| client.hello.borrow().clone().unwrap())
            .map_err(|error| error.to_string())
    }

    async fn find_node(&self, target: &str, count: usize) -> Result<Vec<BrowserNode>, String> {
        self.authenticated()
            .await
            .map_err(|error| error.to_string())?
            .find_node(target, count)
            .await
    }

    /// A lookup's grace period limits waiting for a vote, not the lifetime of
    /// an admitted transport exchange. Drain that exchange under its ordinary
    /// deadlines so its authenticated association (or actual dial failure) can
    /// be reused. Queued work is still cancelled, and closing the pool aborts
    /// even an in-progress connection setup immediately.
    async fn lookup(self, target: String, count: usize) -> Result<Vec<BrowserNode>, String> {
        let (mut sender, receiver) = oneshot::channel();
        wasm_bindgen_futures::spawn_local(async move {
            let result = {
                let request = async {
                    let client = match select(
                        Box::pin(self.client.lock_before(&self.admission)),
                        Box::pin(sender.cancellation()),
                    )
                    .await
                    {
                        Either::Left((client, _)) => client.map_err(|error| error.to_string())?,
                        Either::Right(_) => return Err("lookup waiter cancelled".to_string()),
                    };
                    if sender.is_canceled() {
                        return Err("lookup waiter cancelled".to_string());
                    }
                    client.hello().await?;
                    drop(client);
                    let client = match select(
                        Box::pin(self.client.authenticated_before(&self.admission)),
                        Box::pin(sender.cancellation()),
                    )
                    .await
                    {
                        Either::Left((client, _)) => client.map_err(|error| error.to_string())?,
                        Either::Right(_) => return Err("lookup waiter cancelled".into()),
                    };
                    client.find_node(&target, count).await
                };
                match select(Box::pin(request), Box::pin(self.availability.wait_closed())).await {
                    Either::Left((result, _)) => result,
                    Either::Right(_) => Err("WebRTC client pool is closed".to_string()),
                }
            };
            let _ = sender.send(result);
        });
        receiver
            .await
            .map_err(|_| "WebRTC lookup task closed".to_string())?
    }
}

impl Drop for BrowserClientLease {
    fn drop(&mut self) {
        self.availability.notify_waiters();
    }
}

impl BrowserClientPool {
    fn new(max_clients: usize) -> Result<Self, String> {
        if max_clients == 0 {
            return Err("WebRTC client pool size must be a positive integer".to_string());
        }
        let (availability_tx, _) = watch::channel(());
        Ok(Self {
            read_budget: crate::client_engine::read_budget::ReadBudget::new(
                MAX_READ_RESPONSE_MEMORY,
                READ_RESPONSE_RESERVATION,
            ),
            fetch_limiter: RefCell::new(None),
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
            preconnecting: RefCell::new(HashSet::new()),
            #[cfg(feature = "test-utils")]
            preconnect_errors: RefCell::new(Vec::new()),
        })
    }

    async fn client(&self, endpoint: &BrowserEndpoint) -> Result<BrowserClientLease, String> {
        self.client_before(endpoint, TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
            .await
            .map_err(|error| error.to_string())
    }

    async fn client_before(
        &self,
        endpoint: &BrowserEndpoint,
        admission: TransferDeadline,
    ) -> Result<BrowserClientLease, RpcError> {
        self.client_on_lane(endpoint, admission, false).await
    }

    async fn data_client_before(
        &self,
        endpoint: &BrowserEndpoint,
        admission: TransferDeadline,
    ) -> Result<BrowserClientLease, RpcError> {
        self.client_on_lane(endpoint, admission, true).await
    }

    async fn client_on_lane(
        &self,
        endpoint: &BrowserEndpoint,
        admission: TransferDeadline,
        data_lane: bool,
    ) -> Result<BrowserClientLease, RpcError> {
        if admission.remaining().is_zero() {
            return Err(RpcError::Timeout(
                "WebRTC pool admission deadline expired".into(),
            ));
        }
        crate::runtime::timeout(
            admission.remaining(),
            self.wait_for_client(endpoint, admission, data_lane),
        )
        .await
        .map_err(|_| {
            RpcError::Timeout("WebRTC admission timed out waiting for pool capacity".into())
        })?
        .map_err(RpcError::Transport)
    }

    async fn wait_for_client(
        &self,
        endpoint: &BrowserEndpoint,
        admission: TransferDeadline,
        data_lane: bool,
    ) -> Result<BrowserClientLease, String> {
        let endpoint = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
            .map_err(|error| error.to_string())?;
        let key = endpoint.multiaddr.clone();
        // Subscribe before checking the predicate so a release cannot be missed.
        let mut availability = self.availability.sender.subscribe();
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
                    Some(Rc::clone(if data_lane {
                        &entry.data_client
                    } else {
                        &entry.client
                    }))
                } else {
                    if clients.len() >= self.max_clients {
                        let evict = clients
                            .iter()
                            .filter(|(_, entry)| {
                                Rc::strong_count(&entry.client) == 1
                                    && Rc::strong_count(&entry.data_client) == 1
                                    && !entry.client.has_pending_requests()
                                    && !entry.data_client.has_pending_requests()
                            })
                            .min_by_key(|(_, entry)| entry.last_used)
                            .map(|(key, _)| key.clone());
                        if let Some(evict) = evict {
                            if let Some(entry) = clients.remove(&evict) {
                                entry.client.close();
                                entry.data_client.close();
                            }
                        }
                    }
                    if clients.len() < self.max_clients {
                        let mut client = BrowserNodeClientCore::new(endpoint.clone());
                        client.dial_failures = Some(Rc::clone(&self.dial_failures));
                        client.pool_availability = Some(Rc::clone(&self.availability));
                        let mut data_client = BrowserNodeClientCore::new(endpoint.clone());
                        data_client.association = Rc::clone(&client.association);
                        data_client.dial_failures = Some(Rc::clone(&self.dial_failures));
                        data_client.pool_availability = Some(Rc::clone(&self.availability));
                        let data_client = Rc::new(data_client);
                        let client = Rc::new(client);
                        clients.insert(
                            key.clone(),
                            PoolEntry {
                                client: Rc::clone(&client),
                                data_client: Rc::clone(&data_client),
                                last_used: now,
                            },
                        );
                        Some(if data_lane { data_client } else { client })
                    } else {
                        None
                    }
                }
            };
            if let Some(client) = client {
                return Ok(BrowserClientLease {
                    client,
                    availability: Rc::clone(&self.availability),
                    admission,
                });
            }
            availability
                .changed()
                .await
                .map_err(|_| "WebRTC client pool closed".to_string())?;
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

    /// Overlap ICE/PQ/HELLO with the iterative walk, using only owner-signed
    /// addresses that `find_node` has already verified. These are connection
    /// hints, not successful lookup votes or storage acknowledgements.
    fn preconnect(self: &Rc<Self>, nodes: &[BrowserNode]) {
        for node in nodes {
            let at_capacity = {
                let clients = self.clients.borrow();
                let pending = self.preconnecting.borrow();
                // A task's reservation stops consuming another slot once its
                // client has entered the pool.
                let reserved = pending
                    .iter()
                    .filter(|key| !clients.contains_key(*key))
                    .count();
                clients.len() + reserved >= self.max_clients
            };
            if self.availability.closed.get()
                || self.preconnecting.borrow().len() >= MAX_LOOKUP_PRECONNECTS
                || at_capacity
            {
                break;
            }
            let Some(endpoint) = node
                .webrtc_direct
                .as_ref()
                .filter(|_| node.address_record.is_some())
            else {
                continue;
            };
            if self.preconnecting.borrow().contains(&endpoint.multiaddr)
                || !self.is_lookup_eligible(&node.peer_id, endpoint)
                || self
                    .clients
                    .borrow()
                    .get(&endpoint.multiaddr)
                    .is_some_and(|entry| {
                        entry.client.is_connected() || Rc::strong_count(&entry.client) > 1
                    })
            {
                continue;
            }
            let pool = Rc::clone(self);
            let endpoint = endpoint.clone();
            self.preconnecting
                .borrow_mut()
                .insert(endpoint.multiaddr.clone());
            wasm_bindgen_futures::spawn_local(async move {
                {
                    let connect = async {
                        let lease = pool
                            .client_before(
                                &endpoint,
                                TransferDeadline::new(Duration::from_millis(u64::from(
                                    REQUEST_TIMEOUT_MS,
                                ))),
                            )
                            .await?;
                        lease.hello().await.map_err(RpcError::Transport)
                    };
                    let outcome =
                        select(Box::pin(connect), Box::pin(pool.availability.wait_closed())).await;
                    #[cfg(feature = "test-utils")]
                    if let Either::Left((Err(error), _)) = &outcome {
                        pool.preconnect_errors.borrow_mut().push(error.to_string());
                    }
                    drop(outcome);
                }
                pool.preconnecting.borrow_mut().remove(&endpoint.multiaddr);
            });
        }
    }

    fn read_limit(&self) -> usize {
        let desired = self.fetch_limiter.borrow().as_ref().map_or_else(
            || crate::data::client::adaptive::ChannelStart::default().fetch,
            |limiter| limiter.current(),
        );
        self.read_budget.limit(desired)
    }

    fn close(&self) {
        self.read_budget.close();
        self.availability.close();
        for (_, entry) in self.clients.borrow_mut().drain() {
            entry.client.close();
            entry.data_client.close();
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

/// Two independent authenticated channels share ICE/DTLS/SCTP. Dropping a
/// cancelled exchange retires only its channel; the other lane remains usable.
struct PeerAssociation {
    connection: RtcPeerConnection,
    channels: RefCell<Vec<RtcDataChannel>>,
}

impl Deref for PeerAssociation {
    type Target = RtcPeerConnection;
    fn deref(&self) -> &Self::Target {
        &self.connection
    }
}

impl PeerAssociation {
    fn has_open_channel(&self) -> bool {
        self.channels
            .borrow()
            .iter()
            .any(|channel| channel.ready_state() == RtcDataChannelState::Open)
    }
}

impl Drop for PeerAssociation {
    fn drop(&mut self) {
        self.connection.close();
    }
}

type SharedAssociation = Rc<Mutex<Weak<PeerAssociation>>>;

struct Connection {
    peer_connection: Rc<PeerAssociation>,
    data_channel: RtcDataChannel,
    inbox: Rc<ResponseInbox>,
    pq_session: Rc<RefCell<Option<PqSession>>>,
    rpc: RefCell<Option<Rc<multiplex::RpcSession>>>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(Event)>,
    _on_close: Closure<dyn FnMut(Event)>,
    _on_open: Closure<dyn FnMut(Event)>,
}

impl Connection {
    async fn open(
        endpoint: &WebRtcDirectEndpoint,
        source: &SharedAssociation,
        attempted_dial: &Cell<bool>,
    ) -> Result<Self, String> {
        // Serialize only setup. Each lane has its own RPC lock, inbox, request
        // IDs and PQ session; no stream cipher/response state is shared.
        let mut association = source.lock().await;
        let existing = association
            .upgrade()
            .filter(|connection| connection.has_open_channel());
        let fresh = existing.is_none();
        attempted_dial.set(fresh);
        let peer_connection = if let Some(existing) = existing {
            existing
        } else {
            let configuration = RtcConfiguration::new();
            configuration.set_ice_servers(&Array::new());
            Rc::new(PeerAssociation {
                connection: RtcPeerConnection::new_with_configuration(&configuration)
                    .map_err(js_error_message)?,
                channels: RefCell::new(Vec::new()),
            })
        };
        // Wait for a retired lane to finish closing before replacing it. The
        // server admits two channels, including channels still unwinding.
        while peer_connection
            .channels
            .borrow()
            .iter()
            .filter(|channel| channel.ready_state() != RtcDataChannelState::Closed)
            .count()
            >= 2
        {
            crate::runtime::sleep(Duration::from_millis(10)).await;
        }
        let channel_configuration = RtcDataChannelInit::new();
        channel_configuration.set_ordered(true);
        let data_channel = peer_connection.create_data_channel_with_data_channel_dict(
            WEBRTC_DIRECT_DATA_CHANNEL,
            &channel_configuration,
        );
        data_channel.set_binary_type(RtcDataChannelType::Arraybuffer);
        {
            let mut channels = peer_connection.channels.borrow_mut();
            channels.retain(|channel| channel.ready_state() != RtcDataChannelState::Closed);
            channels.push(data_channel.clone());
        }

        let inbox = ResponseInbox::new();
        let (open_tx, open_rx) = oneshot::channel::<Result<(), String>>();
        let open_tx = Rc::new(RefCell::new(Some(open_tx)));
        let message_inbox = Rc::clone(&inbox);
        let message_channel = data_channel.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
            if let Err(error) = message_inbox.push(event.data()) {
                message_inbox.fail(error);
                message_channel.close();
            }
        });
        data_channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));

        let error_inbox = Rc::clone(&inbox);
        let error_open = Rc::clone(&open_tx);
        let on_error = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            error_inbox.fail("WebRTC DataChannel failed".to_string());
            if let Some(sender) = error_open.borrow_mut().take() {
                let _ = sender.send(Err("WebRTC DataChannel failed before opening".into()));
            }
        });
        data_channel.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        let close_inbox = Rc::clone(&inbox);
        let close_open = Rc::clone(&open_tx);
        let on_close = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            close_inbox.fail("WebRTC DataChannel closed".to_string());
            if let Some(sender) = close_open.borrow_mut().take() {
                let _ = sender.send(Err("WebRTC DataChannel closed before opening".into()));
            }
        });
        data_channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));

        let open_sender = Rc::clone(&open_tx);
        let on_open = Closure::<dyn FnMut(Event)>::new(move |_event: Event| {
            if let Some(sender) = open_sender.borrow_mut().take() {
                let _ = sender.send(Ok(()));
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
            pq_session: Rc::new(RefCell::new(None)),
            rpc: RefCell::new(None),
            _on_message: on_message,
            _on_error: on_error,
            _on_close: on_close,
            _on_open: on_open,
        };

        if fresh {
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
            let client_pwd =
                ice_password_from_sdp(&local_sdp).map_err(|error| error.to_string())?;
            let server_credential =
                v2_server_ice_credential(&client_pwd).map_err(|error| error.to_string())?;
            let answer_sdp = server_answer_sdp(endpoint, &server_credential)
                .map_err(|error| error.to_string())?;
            let remote = RtcSessionDescriptionInit::new(RtcSdpType::Answer);
            remote.set_sdp(&answer_sdp);
            JsFuture::from(connection.peer_connection.set_remote_description(&remote))
                .await
                .map_err(js_error_message)?;
        }

        timeout(
            async move {
                open_rx
                    .await
                    .map_err(|_| "WebRTC DataChannel closed before opening".to_string())?
            },
            "WebRTC DataChannel opening timed out",
        )
        .await?;
        connection.data_channel.set_onopen(None);

        let session = establish_pq_session(&connection, endpoint).await?;
        connection.pq_session.replace(Some(session));
        connection.rpc.replace(Some(multiplex::RpcSession::new(
            connection.data_channel.clone(),
            Rc::clone(&connection.inbox),
            Rc::clone(&connection.pq_session),
        )));
        *association = Rc::downgrade(&connection.peer_connection);

        Ok(connection)
    }

    fn close(&self) {
        if let Some(rpc) = self.rpc.borrow().as_ref() {
            rpc.close("WebRTC connection closed".into());
        }
        self.inbox.fail("WebRTC connection closed".into());
        self.data_channel.close();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.close();
        self.inbox.fail("WebRTC connection closed".to_string());
        self.data_channel.set_onmessage(None);
        self.data_channel.set_onerror(None);
        self.data_channel.set_onclose(None);
        self.data_channel.set_onopen(None);
        self.data_channel.set_onbufferedamountlow(None);
        self.data_channel.close();
    }
}

pub(super) struct BrowserNodeClientCore {
    dial_failures: Option<DialFailures>,
    pool_availability: Option<Rc<PoolAvailability>>,
    endpoint: WebRtcDirectEndpoint,
    association: SharedAssociation,
    connection: RefCell<Option<Rc<Connection>>>,
    request_lock: Mutex<()>,
    response_processing: Cell<Duration>,
    next_request_id: Cell<u64>,
    generation: Cell<u64>,
    hello: RefCell<Option<BrowserHello>>,
    peer_id: RefCell<Option<String>>,
}

impl BrowserNodeClientCore {
    pub(super) fn new(endpoint: WebRtcDirectEndpoint) -> Self {
        Self {
            dial_failures: None,
            pool_availability: None,
            endpoint,
            association: Rc::new(Mutex::new(Weak::new())),
            connection: RefCell::new(None),
            request_lock: Mutex::new(()),
            response_processing: Cell::new(Duration::ZERO),
            next_request_id: Cell::new(1),
            generation: Cell::new(0),
            hello: RefCell::new(None),
            peer_id: RefCell::new(None),
        }
    }

    pub(super) fn peer_id(&self) -> Option<String> {
        self.peer_id.borrow().clone()
    }

    fn has_pending_requests(&self) -> bool {
        self.connection.borrow().as_ref().is_some_and(|connection| {
            connection
                .rpc
                .borrow()
                .as_ref()
                .is_some_and(|rpc| rpc.is_busy())
        })
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
        let generation = self.generation.get();
        let attempted_dial = Cell::new(false);
        let connection = match timeout_with_ms(
            Connection::open(&self.endpoint, &self.association, &attempted_dial),
            "WebRTC connection/authentication setup timed out",
            CONNECTION_SETUP_TIMEOUT_MS,
        )
        .await
        {
            Ok(connection) => connection,
            Err(error) => {
                if let Some(cache) = self.dial_failures.as_ref().filter(|_| attempted_dial.get()) {
                    cache.borrow_mut().record_failure(
                        self.endpoint.peer_id.clone(),
                        self.endpoint.multiaddr.clone(),
                    );
                }
                return Err(error);
            }
        };
        // close() can run while SDP, ICE or PQ authentication is awaiting JS.
        // Never publish a connection belonging to an invalidated generation.
        if generation != self.generation.get() || self.pool_is_closed() {
            connection.close();
            return Err("WebRTC client closed during connection setup".into());
        }
        if let Some(cache) = &self.dial_failures {
            cache.borrow_mut().record_success(&self.endpoint.peer_id);
        }
        if let Some(rpc) = connection.rpc.borrow().as_ref() {
            rpc.set_pool(self.pool_availability.as_ref());
        }
        self.connection.replace(Some(Rc::new(connection)));
        Ok(())
    }

    fn pool_is_closed(&self) -> bool {
        self.pool_availability
            .as_ref()
            .is_some_and(|pool| pool.closed.get())
    }

    async fn lock_before(
        &self,
        admission: &TransferDeadline,
    ) -> Result<LockedBrowserClient<'_>, RpcError> {
        if admission.remaining().is_zero() {
            return Err(RpcError::Timeout(
                "WebRTC peer admission deadline expired".into(),
            ));
        }
        let guard = crate::runtime::timeout(admission.remaining(), self.request_lock.lock())
            .await
            .map_err(|_| RpcError::Timeout("WebRTC admission timed out waiting for peer".into()))?;
        if self.pool_is_closed() {
            return Err("WebRTC client pool is closed".to_string().into());
        }
        Ok(LockedBrowserClient {
            client: self,
            _guard: Some(guard),
            slot: RefCell::new(None),
            authenticated_generation: None,
        })
    }

    async fn authenticated_before(
        &self,
        admission: &TransferDeadline,
    ) -> Result<LockedBrowserClient<'_>, RpcError> {
        loop {
            let mut client = self.lock_before(admission).await?;
            client.hello().await?;
            client._guard.take();
            let rpc = self
                .connection
                .borrow()
                .as_ref()
                .and_then(|c| c.rpc.borrow().clone())
                .ok_or_else(|| "WebRTC session unavailable".to_string())?;
            let generation = self.generation.get();
            match rpc.admit(admission.remaining()).await {
                Ok(slot) => {
                    if generation != self.generation.get() {
                        continue;
                    }
                    client.slot.replace(Some(slot));
                    client.authenticated_generation = Some(generation);
                    return Ok(client);
                }
                Err(_)
                    if rpc.is_closed()
                        && !self.pool_is_closed()
                        && !admission.remaining().is_zero() =>
                {
                    continue
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn authenticated(&self) -> Result<LockedBrowserClient<'_>, RpcError> {
        self.authenticated_before(&TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
            .await
    }

    pub(super) async fn hello(&self) -> Result<BrowserHello, String> {
        self.authenticated()
            .await
            .map(|client| client.hello.borrow().clone().unwrap())
            .map_err(|error| error.to_string())
    }

    #[cfg(feature = "test-utils")]
    async fn find_node(&self, target: &str, count: usize) -> Result<Vec<BrowserNode>, String> {
        self.authenticated()
            .await
            .map_err(|error| error.to_string())?
            .find_node(target, count)
            .await
    }

    pub(super) fn close(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        if let Some(connection) = self.connection.borrow_mut().take() {
            connection.close();
        }
        self.hello.borrow_mut().take();
        self.peer_id.borrow_mut().take();
    }
}

impl Drop for BrowserNodeClientCore {
    fn drop(&mut self) {
        self.close();
    }
}

// Setup is serialized through HELLO. Authenticated callers release the setup
// guard and use the session's bounded request admission and response dispatcher.
struct LockedBrowserClient<'a> {
    client: &'a BrowserNodeClientCore,
    _guard: Option<MutexGuard<'a, ()>>,
    slot: RefCell<Option<tokio::sync::OwnedSemaphorePermit>>,
    authenticated_generation: Option<u64>,
}

impl Deref for LockedBrowserClient<'_> {
    type Target = BrowserNodeClientCore;
    fn deref(&self) -> &Self::Target {
        self.client
    }
}

impl LockedBrowserClient<'_> {
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
        self.request_with_timeout(
            body,
            content,
            Duration::from_millis(u64::from(REQUEST_TIMEOUT_MS)),
        )
        .await
    }

    async fn request_with_timeout(
        &self,
        body: BrowserRequestBody,
        content: &[u8],
        response_timeout: Duration,
    ) -> Result<BrowserResponseFrame, RpcError> {
        let exclusive = matches!(&body, BrowserRequestBody::PutChunk { .. });
        self.request_reserved(body, content, response_timeout, None, exclusive)
            .await
            .map(|(response, _)| response)
    }

    async fn request_reserved(
        &self,
        body: BrowserRequestBody,
        content: &[u8],
        response_timeout: Duration,
        read_permit: Option<crate::client_engine::read_budget::ReadPermit>,
        exclusive: bool,
    ) -> Result<
        (
            BrowserResponseFrame,
            Option<crate::client_engine::read_budget::ReadPermit>,
        ),
        RpcError,
    > {
        if self
            .authenticated_generation
            .is_some_and(|generation| generation != self.generation.get())
        {
            return Err("authenticated session closed".to_string().into());
        }
        if matches!(&body, BrowserRequestBody::Hello) {
            self.ensure_connected().await?;
        } else if !self.is_connected() || self.hello.borrow().is_none() {
            return Err("authenticated session required; call connect() again"
                .to_string()
                .into());
        }
        let connection = self
            .connection
            .borrow()
            .clone()
            .ok_or_else(|| "WebRTC DataChannel is not connected".to_string())?;
        let rpc = connection
            .rpc
            .borrow()
            .clone()
            .ok_or_else(|| "WebRTC session is unavailable".to_string())?;
        let request_id = self.next_request_id.get();
        self.next_request_id.set(request_id.wrapping_add(1).max(1));
        let request = BrowserRequest::new(request_id, body, content.len());
        let plaintext =
            encode_request_frame(&request, content).map_err(|error| error.to_string())?;
        let slot = self.slot.borrow_mut().take();
        let (response, processing, read_permit) = rpc
            .request(
                request_id,
                plaintext,
                response_timeout,
                read_permit,
                slot,
                exclusive,
            )
            .await?;
        self.response_processing.set(processing);
        if response.header.status == BrowserResponseStatus::Error {
            let (code, message) = match &response.header.body {
                BrowserResponseBody::Error { code, message } => (code.clone(), message.clone()),
                _ => (
                    "invalid_response".into(),
                    "node returned an invalid error response".into(),
                ),
            };
            if code == "authentication_required" {
                rpc.close("authenticated session closed".into());
            }
            return Err(RpcError::Remote { code, message });
        }

        Ok((response, read_permit))
    }

    pub(super) async fn hello(&self) -> Result<BrowserHello, String> {
        if let Some(hello) = self.hello.borrow().clone() {
            if self.connection.borrow().as_ref().is_some_and(|connection| {
                connection.data_channel.ready_state() == RtcDataChannelState::Open
            }) {
                return Ok(hello);
            }
        }
        self.ensure_connected().await?;
        let response = timeout(
            self.request(BrowserRequestBody::Hello, &[]),
            "WebRTC HELLO authentication timed out",
        )
        .await?;
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
        if hello
            .capabilities
            .iter()
            .any(|cap| cap == multiplex::CAPABILITY)
        {
            if let Some(rpc) = self
                .connection
                .borrow()
                .as_ref()
                .and_then(|c| c.rpc.borrow().clone())
            {
                rpc.enable_multiplex();
            }
        }
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
        let require_owner_proofs = self
            .hello
            .borrow()
            .as_ref()
            .ok_or("authenticated session required")?
            .capabilities
            .iter()
            .any(|capability| capability == ant_protocol::transport::ADDRESS_V2_CAPABILITY);
        let response = self
            .request(
                BrowserRequestBody::FindNode {
                    with_address_records: true,
                    target: target.clone(),
                    count: Some(count),
                },
                &[],
            )
            .await?;
        let BrowserResponseBody::Nodes {
            target: response_target,
            mut nodes,
        } = response.header.body
        else {
            return Err("expected a NODES response".to_string());
        };
        if response_target.to_ascii_lowercase() != target {
            return Err("node returned results for a different lookup target".to_string());
        }
        if nodes.len() > count {
            return Err("node returned more peers than requested".into());
        }
        let mut owners = HashSet::new();
        for node in &mut nodes {
            node.peer_id = super::protocol::normalize_hex(&node.peer_id, 32)?;
            if !owners.insert(node.peer_id.clone()) {
                return Err("duplicate peer in lookup response".into());
            }
        }
        let proofs = ant_protocol::transport::signed_address::decode_record_bundle(
            &response.content,
            nodes.len(),
        )?;
        let mut by_owner = HashMap::new();
        for proof in proofs {
            let encoded = hex::encode(proof.encode()?);
            let verified = super::peer_records::verified_address_record(&encoded)?;
            let owner = verified.owner().to_hex();
            if by_owner.insert(owner, encoded).is_some() {
                return Err("duplicate signed address owner".into());
            }
        }
        for node in &mut nodes {
            // A JSON field cannot bypass the bounded binary proof decoder.
            node.address_record = by_owner.remove(&node.peer_id);
            if require_owner_proofs && node.address_record.is_none() {
                return Err("V2 lookup peer is missing its owner proof".into());
            }
            let verified_view = shared::peer_record(node).map_err(|e| e.to_string())?;
            *node = shared::browser_record(verified_view).map_err(|e| e.to_string())?;
        }
        if !by_owner.is_empty() {
            return Err("signed address owner absent from lookup response".into());
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
}

struct BrowserNetworkCore {
    seeds: Vec<BrowserEndpoint>,
    pool: Rc<BrowserClientPool>,
    routing: Rc<RefCell<HashMap<LookupKey, BrowserLookupCandidate>>>,
    contacted: Rc<RefCell<HashMap<LookupKey, web_time::Instant>>>,
    owner_views: Rc<RefCell<lru::LruCache<LookupKey, BrowserLookupCandidate>>>,
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
            contacted: Rc::new(RefCell::new(HashMap::new())),
            owner_views: Rc::new(RefCell::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(MAX_BROWSER_ROUTING_ENTRIES)
                    .ok_or("routing cache must be nonempty")?,
            ))),
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
        self.find_closest_with_progress(target, progress, count, None)
            .await
    }

    async fn find_closest_with_progress(
        &self,
        target: &str,
        progress: &ProgressReporter,
        count: usize,
        read_progress: Option<crate::data::network::ReadProgress>,
    ) -> Result<BrowserLookupResult, String> {
        // Only authenticated, recently contacted endpoints bootstrap another walk.
        self.routing.borrow_mut().retain(|peer, _| {
            self.contacted
                .borrow()
                .get(peer)
                .is_some_and(|last| last.elapsed() < Duration::from_secs(15 * 60))
        });
        let cached = !self.routing.borrow().is_empty();
        if !cached {
            return self
                .find_closest_attempt(target, progress, count, true, read_progress.clone())
                .await;
        }
        let first = self
            .find_closest_attempt(target, progress, count, false, read_progress.clone())
            .await;
        if first
            .as_ref()
            .is_ok_and(|result| result.nodes.len() >= count)
        {
            return first;
        }
        progress.report("Retrying lookup through configured seeds");
        let seeded = self
            .find_closest_attempt(target, progress, count, true, read_progress.clone())
            .await;
        match (first, seeded) {
            (Ok(first), Ok(second)) if first.nodes.len() > second.nodes.len() => Ok(first),
            (_, Ok(second)) => Ok(second),
            (Ok(first), Err(_)) if !first.nodes.is_empty() => Ok(first),
            (_, Err(error)) => Err(error),
        }
    }

    async fn find_closest_attempt(
        &self,
        target: &str,
        progress: &ProgressReporter,
        count: usize,
        use_seeds: bool,
        read_progress: Option<crate::data::network::ReadProgress>,
    ) -> Result<BrowserLookupResult, String> {
        let target_key = parse_lookup_key(target, "lookup target")?;
        let failures = Rc::new(RefCell::new(Vec::new()));
        let views = Rc::new(RefCell::new(HashMap::new()));
        let seed_futures = self.seeds.iter().cloned().map(|endpoint| {
            let pool = Rc::clone(&self.pool);
            let failures = Rc::clone(&failures);
            let progress = progress.clone();
            let read_progress = read_progress.clone();
            async move {
                let seed_name = endpoint.multiaddr.clone();
                let result = async {
                    let client = pool.client(&endpoint).await?;
                    let hello = client.hello().await?;
                    progress.report(&format!("Connected seed {}", hello.peer_id));
                    BrowserLookupCandidate::parse(BrowserNode {
                        address_record: None,
                        peer_record: None,
                        peer_id: hello.peer_id,
                        native_addresses: Vec::new(),
                        reliability: 1.0,
                        webrtc_direct: Some(hello.endpoint),
                    })
                }
                .await;
                match result {
                    Ok(candidate) => {
                        if let Some(progress) = &read_progress {
                            if let Ok(node) = shared::peer_record(&candidate.wire) {
                                progress.offer(vec![(node.peer_id, node.addresses_by_priority())]);
                            }
                        }
                        Some(candidate)
                    }
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
        let mut initial_candidates = self
            .routing
            .borrow()
            .values()
            .filter(|candidate| candidate.wire.webrtc_direct.is_some())
            .cloned()
            .collect::<Vec<_>>();
        if use_seeds {
            initial_candidates.clear();
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
            contacted: Rc::clone(&self.contacted),
            owner_views: Rc::clone(&self.owner_views),
            reports: HashMap::new(),
            read_progress,
        };
        run_iterative_lookup(
            &mut lookup,
            &mut query,
            TimeoutFuture::new(
                ant_protocol::transport::LOOKUP_TIMEOUT_SECS * if use_seeds { 1_000 } else { 500 },
            ),
        )
        .await
        .map_err(|error| error.to_string())?;
        let mut routes = self.routing.borrow_mut();
        if routes.len() > MAX_BROWSER_ROUTING_ENTRIES {
            let mut peers = routes.keys().copied().collect::<Vec<_>>();
            peers.sort_by_key(|peer| std::cmp::Reverse(self.contacted.borrow().get(peer).copied()));
            for peer in peers.into_iter().skip(MAX_BROWSER_ROUTING_ENTRIES) {
                routes.remove(&peer);
            }
        }
        self.contacted
            .borrow_mut()
            .retain(|peer, _| routes.contains_key(peer));
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
    read_progress: Option<crate::data::network::ReadProgress>,
    pool: Rc<BrowserClientPool>,
    progress: ProgressReporter,
    failures: Rc<RefCell<Vec<BrowserLookupFailure>>>,
    views: Rc<RefCell<HashMap<LookupKey, Vec<BrowserNode>>>>,
    known_endpoints: HashMap<LookupKey, BrowserEndpoint>,
    reports: HashMap<PeerId, client_routing::SubjectReports>,
    routing: Rc<RefCell<HashMap<LookupKey, BrowserLookupCandidate>>>,
    contacted: Rc<RefCell<HashMap<LookupKey, web_time::Instant>>>,
    owner_views: Rc<RefCell<lru::LruCache<LookupKey, BrowserLookupCandidate>>>,
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
                let routing = Rc::clone(&self.routing);
                let contacted = Rc::clone(&self.contacted);
                let read_progress = self.read_progress.clone();
                let target = target.clone();
                async move {
                    let responder = candidate.peer_id;
                    let peer_id = candidate.wire.peer_id.clone();
                    let result = async {
                        let endpoint = candidate.wire.webrtc_direct.as_ref().ok_or_else(|| {
                            "lookup candidate has no WebRTC Direct endpoint".to_string()
                        })?;
                        let client = pool.client(endpoint).await?;
                        client
                            .lookup(
                                target.clone(),
                                count.max(crate::quote_policy::SINGLE_NODE_WITNESSED_VIEW_COUNT),
                            )
                            .await
                    }
                    .await;
                    match result {
                        Ok(nodes) => {
                            if let Some(progress) = &read_progress {
                                let hints = std::iter::once(&candidate.wire)
                                    .chain(nodes.iter())
                                    .filter_map(|node| shared::peer_record(node).ok())
                                    .map(|node| (node.peer_id, node.addresses_by_priority()))
                                    .collect();
                                progress.offer(hints);
                            }
                            pool.preconnect(&nodes);
                            routing.borrow_mut().insert(responder, candidate.clone());
                            contacted
                                .borrow_mut()
                                .insert(responder, web_time::Instant::now());
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
                            routing.borrow_mut().remove(&responder);
                            contacted.borrow_mut().remove(&responder);
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
                    let Ok(mut record) = shared::peer_record(&candidate.wire) else {
                        return false;
                    };
                    let known = self
                        .owner_views
                        .borrow_mut()
                        .get(&candidate.peer_id)
                        .and_then(|current| shared::peer_record(&current.wire).ok());
                    if let Some(known) = known
                        .filter(|known| !client_routing::may_replace_owner_view(known, &record))
                    {
                        record = known;
                    }
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
                    // Ownership history prevents rollback, but is not a live routing entry.
                    self.owner_views
                        .borrow_mut()
                        .put(candidate.peer_id, candidate.clone());
                    if self
                        .routing
                        .borrow()
                        .get(&candidate.peer_id)
                        .is_some_and(|known| {
                            known.wire.webrtc_direct != candidate.wire.webrtc_direct
                        })
                    {
                        self.routing.borrow_mut().remove(&candidate.peer_id);
                        self.contacted.borrow_mut().remove(&candidate.peer_id);
                    }
                    if let Some(endpoint) = candidate.wire.webrtc_direct.clone() {
                        self.known_endpoints.insert(candidate.peer_id, endpoint);
                        true
                    } else {
                        self.known_endpoints.remove(&candidate.peer_id);
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
    Reference {
        address: String,
        #[serde(default)]
        name: String,
        #[serde(default)]
        content_type: String,
    },
}

impl BrowserPublicFileInput {
    fn into_address_and_descriptor(self) -> (String, Option<PublicFileDescriptor>) {
        match self {
            Self::Descriptor(file) => (file.address.clone(), Some(file)),
            Self::Address(address) => (address, None),
            Self::Reference {
                address,
                name,
                content_type,
            } => (
                address.clone(),
                Some(PublicFileDescriptor {
                    name: if name.is_empty() {
                        fallback_public_file_name(&address)
                    } else {
                        name
                    },
                    address,
                    content_type,
                    size: 0,
                    blake3: String::new(),
                    chunks: Vec::new(),
                    data_map_size: 0,
                    replicas: 0,
                }),
            ),
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
    /// Effective payment mode reported by the shared coordinator.
    #[serde(rename = "paymentMode")]
    payment_mode: PaymentMode,
}

/// Content-addressed records staged by the caller and paid as one batch.
#[derive(Debug, Deserialize)]
struct BrowserRecordBatch {
    records: Vec<BrowserRecordInfo>,
    /// Position of the first record in its file, keeping progress stable across batches.
    #[serde(default)]
    first_index: usize,
    /// Record count of the whole file, when the caller already knows it.
    #[serde(default)]
    total_records: Option<usize>,
}

/// Accounting for one caller-staged record batch.
#[derive(Debug, Serialize)]
struct BrowserRecordBatchResult {
    #[serde(rename = "transactionHash", skip_serializing_if = "Option::is_none")]
    transaction_hash: Option<String>,
    #[serde(rename = "storageCostAtto")]
    storage_cost_atto: String,
    records: usize,
    replicas: usize,
    #[serde(rename = "paymentMode")]
    payment_mode: PaymentMode,
}

struct BrowserStoredRecords {
    payment: BrowserPaymentSubmission,
    replicas: usize,
    records: usize,
    mode: PaymentMode,
}

impl From<BrowserStoredRecords> for BrowserRecordBatchResult {
    fn from(stored: BrowserStoredRecords) -> Self {
        Self {
            transaction_hash: stored.payment.transaction_hash,
            storage_cost_atto: stored.payment.total_amount,
            records: stored.records,
            replicas: stored.replicas,
            payment_mode: stored.mode,
        }
    }
}

/// Maps batch-local record positions onto stable positions within a file.
#[derive(Debug, Clone, Copy, Default)]
struct RecordPlacement {
    offset: usize,
    file_total: Option<usize>,
}

impl RecordPlacement {
    fn position(self, batch_position: usize) -> usize {
        self.offset + batch_position
    }

    fn total(self, batch_total: usize) -> usize {
        self.file_total
            .unwrap_or_default()
            .max(self.offset + batch_total)
    }
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

#[derive(Default, Clone)]
struct UploadCheckpoint {
    mode: PaymentMode,
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
    fn from_js(
        payment_mode: Option<String>,
        merkle_wallet: Option<js_sys::Function>,
        snapshot: Option<String>,
        callback: Option<js_sys::Function>,
    ) -> Result<Self, JsValue> {
        Ok(Self {
            mode: parse_payment_mode(payment_mode.as_deref())?,
            merkle_wallet,
            snapshot,
            callback,
        })
    }

    fn envelope(snapshot: &str) -> Result<UploadCheckpointEnvelope, String> {
        if snapshot.len() > MAX_UPLOAD_CHECKPOINT_BYTES {
            return Err("upload checkpoint too large".into());
        }
        serde_json::from_str(snapshot).map_err(|e| e.to_string())
    }

    fn encode(
        scope: &str,
        state: &crate::data::client::upload_state::UploadState,
    ) -> Result<String, String> {
        let snapshot = serde_json::to_string(&UploadCheckpointEnvelope {
            scope: scope.into(),
            state: hex::encode(state.checkpoint().map_err(|e| e.to_string())?),
        })
        .map_err(|e| e.to_string())?;
        if snapshot.len() > MAX_UPLOAD_CHECKPOINT_BYTES {
            return Err("upload checkpoint too large".into());
        }
        Ok(snapshot)
    }

    fn restore(
        &self,
        scope: &str,
    ) -> Result<crate::data::client::upload_state::UploadState, String> {
        let Some(snapshot) = &self.snapshot else {
            return Ok(Default::default());
        };
        let envelope = Self::envelope(snapshot)?;
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
        if self.callback.is_none() && state.pending_payment.is_some() {
            return Err("paid uploads require a checkpoint persistence callback".into());
        }
        if let Some(callback) = &self.callback {
            let value = Self::encode(scope, state)?;
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
        *inner.pool.fetch_limiter.borrow_mut() = Some(shared.controller().fetch.clone());
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
        concurrency: Option<usize>,
        on_progress: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let file: BrowserPublicFileInput = serde_wasm_bindgen::from_value(file)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        let progress = ProgressReporter::from_js(on_progress);
        let result = self
            .download_public_file_inner(file, concurrency.unwrap_or(usize::MAX), &progress)
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
        let payment_network = parse_payment_network(payment_network)?;
        let progress = ProgressReporter::from_js(on_progress);
        let checkpoint =
            UploadCheckpoint::from_js(payment_mode, pay_for_merkle, checkpoint, on_checkpoint)?;
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
        let payment_network = parse_payment_network(payment_network)?;
        let progress = ProgressReporter::from_js(on_progress);
        let checkpoint =
            UploadCheckpoint::from_js(payment_mode, pay_for_merkle, checkpoint, on_checkpoint)?;
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

    /// Quote, pay for, and store one batch of caller-staged records.
    ///
    /// Callers that cannot hold a whole file's encrypted records at once stage
    /// and upload consecutive batches. Each batch is its own payment and
    /// checkpoint scope; the shared coordinator selects single-node or Merkle
    /// payment for it exactly as for a complete file. Records are loaded
    /// lazily and verified against their addresses on every load.
    #[wasm_bindgen(js_name = uploadRecords)]
    #[allow(clippy::too_many_arguments)] // Mirrors the staged upload argument order.
    pub async fn upload_records(
        &self,
        batch: JsValue,
        payment_network: JsValue,
        load_record: js_sys::Function,
        pay_for_quotes: js_sys::Function,
        on_progress: Option<js_sys::Function>,
        checkpoint: Option<String>,
        on_checkpoint: Option<js_sys::Function>,
        payment_mode: Option<String>,
        pay_for_merkle: Option<js_sys::Function>,
    ) -> Result<JsValue, JsValue> {
        let mut batch: BrowserRecordBatch = serde_wasm_bindgen::from_value(batch)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        validate_record_batch(&mut batch).map_err(|error| JsValue::from_str(&error))?;
        let payment_network = parse_payment_network(payment_network)?;
        let progress = ProgressReporter::from_js(on_progress);
        let checkpoint =
            UploadCheckpoint::from_js(payment_mode, pay_for_merkle, checkpoint, on_checkpoint)?;
        let placement = RecordPlacement {
            offset: batch.first_index,
            file_total: batch.total_records,
        };
        let records = batch
            .records
            .into_iter()
            .map(UploadRecord::from)
            .collect::<Vec<_>>();
        let stored = self
            .prepare_pay_and_store_records(
                records,
                &payment_network,
                Some(&load_record),
                &pay_for_quotes,
                &progress,
                &checkpoint,
                placement,
            )
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&BrowserRecordBatchResult::from(stored))
            .map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Close all pooled WebRTC associations.
    pub fn close(&self) {
        self.inner.pool.close();
    }
}

fn parse_payment_network(value: JsValue) -> Result<BrowserPaymentNetwork, JsValue> {
    let network: BrowserPaymentNetwork = serde_wasm_bindgen::from_value(value)
        .map_err(|error| JsValue::from_str(&error.to_string()))?;
    validate_browser_payment_network(network).map_err(|error| JsValue::from_str(&error.to_string()))
}

fn parse_payment_mode(value: Option<&str>) -> Result<PaymentMode, JsValue> {
    match value {
        None | Some("auto") => Ok(PaymentMode::Auto),
        Some("single") => Ok(PaymentMode::Single),
        Some("merkle") => Ok(PaymentMode::Merkle),
        Some(_) => Err(JsValue::from_str("unknown payment mode")),
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
        let mut resolved = self.resolve_public_file(file, progress).await?;
        let content = self
            .shared
            .data_download_with_progress(
                &resolved.root_data_map,
                concurrency,
                &|completed, total| {
                    progress.report(&format!("Downloaded chunk {completed}/{total}"));
                },
            )
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
            &|| self.shared.controller().fetch.current(),
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

        let mut file = descriptor.unwrap_or_else(|| PublicFileDescriptor {
            name: fallback_public_file_name(&address),
            address: address.clone(),
            size: 0,
            content_type: "application/octet-stream".into(),
            blake3: String::new(),
            data_map_size: 0,
            chunks: Vec::new(),
            replicas: 0,
        });
        file.address = address;
        file.size = resolved_size;
        file.chunks = actual_chunks;
        file.data_map_size = encoded_data_map.len();
        file.blake3.clear(); // Computed when plaintext is read; not a second content identity.
        file.content_type = normalized_content_type(&file.content_type);
        let expected_hash = None;
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
                RecordPlacement::default(),
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
            payment_mode: stored.mode,
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
        let records = staged
            .records
            .iter()
            .cloned()
            .map(UploadRecord::from)
            .collect::<Vec<_>>();
        let fetch = |address: [u8; 32]| {
            let records = &records;
            async move {
                let key = hex::encode(address);
                let (index, record) = records
                    .iter()
                    .enumerate()
                    .find(|(_, record)| record.address == key)
                    .ok_or_else(|| format!("DataMap references missing staged record {key}"))?;
                load_upload_record(index, record, Some(load_record))
                    .await
                    .map(|bytes| bytes::Bytes::copy_from_slice(&bytes))
            }
        };
        let encoded_map = fetch(parse_lookup_key(&staged.address, "DataMap address")?).await?;
        let map = crate::client_engine::files::decode_map(&encoded_map)?;
        let root = crate::client_engine::files::resolve(&map, &fetch, &|| 1)
            .await
            .map_err(|error| error.to_string())?;
        staged.chunks = super::chunk_infos(&root);
        staged.size = staged.chunks.iter().try_fold(0usize, |size, chunk| {
            size.checked_add(chunk.src_size).ok_or("file size overflow")
        })?;
        if !(self_encryption::MIN_ENCRYPTABLE_BYTES..=super::MAX_BROWSER_FILE_BYTES)
            .contains(&staged.size)
        {
            return Err("invalid staged DataMap file size".into());
        }
        for chunk in &staged.chunks {
            if !staged
                .records
                .iter()
                .any(|record| record.address == chunk.dst_hash)
            {
                return Err(format!(
                    "DataMap references missing staged chunk {}",
                    chunk.dst_hash
                ));
            }
        }
        staged.data_map_size = encoded_map.len();
        staged.blake3.clear();
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
                RecordPlacement::default(),
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
            payment_mode: stored.mode,
        })
    }

    #[allow(clippy::too_many_arguments)] // Browser callbacks stay explicit, as at the JS boundary.
    async fn prepare_pay_and_store_records(
        &self,
        records: Vec<UploadRecord>,
        payment_network: &BrowserPaymentNetwork,
        load_record: Option<&js_sys::Function>,
        pay_for_quotes: &js_sys::Function,
        progress: &ProgressReporter,
        checkpoint: &UploadCheckpoint,
        placement: RecordPlacement,
    ) -> Result<BrowserStoredRecords, String> {
        let count = records.len();
        progress.report(&format!("Preparing upload of {count} records"));
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
        )
        .with_shared_quote_state(&self.shared);
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
            placement,
            checkpoint,
            scope: &scope,
            last_transaction: RefCell::new(None),
            payment_state: RefCell::new(None),
            recovering: Cell::new(false),
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
            mode: result.mode,
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
    if staged.records.is_empty() {
        return Err("staged upload contains no records".to_string());
    }
    validate_record_infos(&mut staged.records)?;
    staged.address = super::protocol::normalize_hex(&staged.address, 32)?;
    let public_data_map = staged
        .records
        .last()
        .ok_or_else(|| "staged upload contains no public DataMap".to_string())?;
    if public_data_map.address != staged.address {
        return Err("staged public DataMap metadata does not match its record".to_string());
    }
    Ok(())
}

fn validate_record_batch(batch: &mut BrowserRecordBatch) -> Result<(), String> {
    if batch.records.is_empty() {
        return Err("record batch contains no records".to_string());
    }
    validate_record_infos(&mut batch.records)?;
    let end = batch
        .first_index
        .checked_add(batch.records.len())
        .ok_or("record batch position overflow")?;
    if batch.total_records.is_some_and(|total| total < end) {
        return Err("record batch extends past its file's record count".to_string());
    }
    Ok(())
}

fn validate_record_infos(records: &mut [BrowserRecordInfo]) -> Result<(), String> {
    if records.len() > MAX_UPLOAD_RECORDS {
        return Err("staged upload contains too many records".to_string());
    }
    for record in records {
        record.address = super::protocol::normalize_hex(&record.address, 32)?;
        if record.size == 0 || record.size > MAX_BROWSER_RECORD_BYTES {
            return Err(format!(
                "staged record {} has invalid size {}",
                record.address, record.size
            ));
        }
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

    /// Complete PQ authentication and HELLO, returning a session for application RPCs.
    pub async fn connect(&self) -> Result<BrowserNodeSession, JsValue> {
        // Each returned capability owns a distinct association. Old handles cannot
        // close or issue requests on a later connection created by this connector.
        let inner = Rc::new(BrowserNodeClientCore::new(self.inner.endpoint.clone()));
        inner
            .hello()
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        Ok(BrowserNodeSession {
            generation: inner.generation.get(),
            inner,
        })
    }
}

/// Authenticated application session. Reconnect through `BrowserNodeClient` after closure.
#[wasm_bindgen(js_name = BrowserNodeSession)]
pub struct BrowserNodeSession {
    inner: Rc<BrowserNodeClientCore>,
    generation: u64,
}

impl BrowserNodeSession {
    async fn active(&self) -> Result<LockedBrowserClient<'_>, JsValue> {
        let mut client = self
            .inner
            .lock_before(&TransferDeadline::new(RPC_ADMISSION_TIMEOUT))
            .await
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        if self.generation != self.inner.generation.get()
            || !self.inner.is_connected()
            || self.inner.hello.borrow().is_none()
        {
            return Err(JsValue::from_str(
                "session closed; call BrowserNodeClient.connect() again",
            ));
        }
        client._guard.take();
        let rpc = self
            .inner
            .connection
            .borrow()
            .as_ref()
            .and_then(|c| c.rpc.borrow().clone())
            .ok_or_else(|| JsValue::from_str("session closed"))?;
        let slot = rpc
            .admit(RPC_ADMISSION_TIMEOUT)
            .await
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        if self.generation != self.inner.generation.get() {
            return Err(JsValue::from_str("session closed"));
        }
        client.slot.replace(Some(slot));
        client.authenticated_generation = Some(self.generation);
        Ok(client)
    }
}

#[wasm_bindgen(js_class = BrowserNodeSession)]
impl BrowserNodeSession {
    /// Authenticated remote peer identity.
    #[wasm_bindgen(getter, js_name = peerId)]
    pub fn peer_id(&self) -> Option<String> {
        self.inner.peer_id()
    }

    /// Authenticate the connected node.
    pub async fn hello(&self) -> Result<JsValue, JsValue> {
        let hello = self
            .active()
            .await?
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
            .active()
            .await?
            .find_node(target, count)
            .await
            .map_err(|error| JsValue::from_str(&error))?;
        serde_wasm_bindgen::to_value(&nodes).map_err(|error| JsValue::from_str(&error.to_string()))
    }

    /// Retrieve and BLAKE3-verify one content-addressed record.
    #[wasm_bindgen(js_name = getChunk)]
    pub async fn get_chunk(&self, address: &str) -> Result<JsValue, JsValue> {
        let (content, hash) = self
            .active()
            .await?
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
            .active()
            .await?
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
            .active()
            .await?
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
    send_data_channel_frame(&connection.data_channel, &client_hello, REQUEST_TIMEOUT_MS)
        .await
        .map_err(|error| error.to_string())?;
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
) -> Result<(), RpcError> {
    let deadline = TransferDeadline::new(Duration::from_millis(u64::from(timeout_ms)));
    for message in frame.chunks(WEBRTC_WRITE_CHUNK_BYTES) {
        if deadline.remaining().is_zero() {
            return Err(RpcError::Timeout(
                "WebRTC request transfer timed out".into(),
            ));
        }
        wait_for_buffer(channel, MAX_BUFFERED_AMOUNT, deadline.remaining_ms()).await?;
        channel
            .send_with_u8_array(message)
            .map_err(js_error_message)?;
    }
    // send() enqueues bytes. Give the final tail the same transfer deadline;
    // the operation-specific response allowance begins only after this drain.
    wait_for_buffer(channel, 0, deadline.remaining_ms()).await
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
    let deadline = TransferDeadline::new(Duration::from_millis(u64::from(initial_timeout_ms)));
    let mut frame_started: Option<web_time::Instant> = None;
    let mut frame_budget = transfer_timeout(saorsa_transport::webrtc::PQ_FRAME_PREFIX_BYTES);
    loop {
        let remaining_ms = frame_started.map_or_else(
            || deadline.remaining_ms(),
            |started| duration_ms(frame_budget.saturating_sub(started.elapsed())),
        );
        let message = match select(
            Box::pin(receiver.next()),
            Box::pin(TimeoutFuture::new(remaining_ms)),
        )
        .await
        {
            Either::Left((result, _)) => result?,
            Either::Right(((), _)) => {
                return Err(RpcError::Timeout(
                    if frame_started.is_some() {
                        "WebRTC response frame transfer timed out"
                    } else {
                        "WebRTC response timed out"
                    }
                    .into(),
                ))
            }
        };
        // Ingress is timestamped because a response may arrive during the
        // final outgoing drain. Buffered bytes do not restart the frame timer.
        let started = *frame_started.get_or_insert(message.received_at);
        let received_at = message.received_at;
        let message = message.bytes;
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
                frame_budget = frame_budget.max(transfer_timeout(expected));
            }
        }
        receiver.set_transfer(started, frame_budget);
        if received_at.duration_since(started) > frame_budget {
            return Err(RpcError::Timeout(
                "WebRTC response frame transfer timed out".into(),
            ));
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
        for event in ["close", "error"] {
            let _ = self
                .channel
                .remove_event_listener_with_callback(event, self.callback.as_ref().unchecked_ref());
        }
    }
}

async fn wait_for_buffer(
    channel: &RtcDataChannel,
    threshold: u32,
    timeout_ms: u32,
) -> Result<(), RpcError> {
    if channel.ready_state() != RtcDataChannelState::Open {
        return Err("WebRTC DataChannel closed while transmitting"
            .to_string()
            .into());
    }
    if channel.buffered_amount() <= threshold {
        return Ok(());
    }
    // Keep the existing high/low watermarks for ordinary fragmentation; the
    // final drain uses zero for both.
    channel.set_buffered_amount_low_threshold(threshold / 2);
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
    for event in ["close", "error"] {
        channel
            .add_event_listener_with_callback(event, listener.callback.as_ref().unchecked_ref())
            .map_err(js_error_message)?;
    }
    // Cover threshold/closure races while installing listeners.
    if channel.buffered_amount() <= threshold || channel.ready_state() != RtcDataChannelState::Open
    {
        if let Some(sender) = sender.borrow_mut().take() {
            let _ = sender.send(());
        }
    }
    crate::runtime::timeout(Duration::from_millis(u64::from(timeout_ms)), receiver)
        .await
        .map_err(|_| {
            RpcError::Timeout("WebRTC request transfer timed out draining send buffer".into())
        })?
        .map_err(|_| "WebRTC DataChannel closed while transmitting".to_string())?;
    if channel.ready_state() != RtcDataChannelState::Open || channel.buffered_amount() > threshold {
        return Err("WebRTC DataChannel failed while transmitting"
            .to_string()
            .into());
    }
    Ok(())
}

fn duration_ms(duration: Duration) -> u32 {
    duration
        .as_nanos()
        .div_ceil(1_000_000)
        .min(i32::MAX as u128) as u32
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
