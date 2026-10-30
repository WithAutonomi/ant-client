//! WebRTC data channels for browser clients, on the page's own
//! `RTCPeerConnection`.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use bytes::Bytes;
use js_sys::{Array, ArrayBuffer, Object, Reflect, Uint8Array};
use tokio::sync::Notify;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    MessageEvent, RtcConfiguration, RtcDataChannel, RtcDataChannelEvent, RtcDataChannelInit,
    RtcDataChannelState, RtcDataChannelType, RtcIceGatheringState, RtcPeerConnection,
    RtcPeerConnectionState, RtcSdpType, RtcSessionDescriptionInit,
};

use super::rtc::{
    channel_events, fits, BoxFuture, Channel, ChannelEvent, ChannelSender, EventSink, Offer,
    PendingOffer, Rtc, RtcError, GATHER_TIMEOUT, MAX_BUFFERED_BYTES,
};
use crate::browser::{discovery_turn, ice_gathering_turn_after_reads, pass_ice_gathering_turn};

/// How often gathering is looked at for its candidates.
const POLL: Duration = Duration::from_millis(10);
/// How long a sender waits on a full channel for its `bufferedamountlow`
/// event before looking again anyway. The event, not this timer, keeps a
/// sending tab going: browsers slow timers in a tab that is not in front to
/// one a second, which polling would turn into a quarter of a megabyte a
/// second.
const DRAIN_FALLBACK: Duration = Duration::from_millis(250);

pub(super) struct BrowserRtc;

type Callback = Closure<dyn FnMut(JsValue)>;

fn js_error(error: JsValue) -> RtcError {
    RtcError::new(
        error
            .as_string()
            .or_else(|| {
                Reflect::get(&error, &JsValue::from_str("message"))
                    .ok()
                    .and_then(|message| message.as_string())
            })
            .unwrap_or_else(|| "WebRTC call failed".into()),
    )
}

/// One peer connection and its data channel. Dropping it detaches every
/// callback and closes the connection.
struct Connection {
    peer: RtcPeerConnection,
    channel: Rc<RefCell<Option<RtcDataChannel>>>,
    callbacks: Rc<RefCell<Vec<Callback>>>,
    sink: EventSink,
    /// Woken when the channel drains below half of what a sender waits on,
    /// or closes.
    drained: Rc<Notify>,
}

impl Connection {
    fn new(ice_servers: &[String], sink: EventSink) -> Result<Self, RtcError> {
        let configuration = RtcConfiguration::new();
        let servers = Array::new();
        if !ice_servers.is_empty() {
            let server = Object::new();
            let urls: Array = ice_servers
                .iter()
                .map(|url| JsValue::from_str(url))
                .collect();
            Reflect::set(&server, &JsValue::from_str("urls"), &urls).map_err(js_error)?;
            servers.push(&server);
        }
        configuration.set_ice_servers(&servers);
        let peer = RtcPeerConnection::new_with_configuration(&configuration).map_err(js_error)?;
        let connection = Self {
            peer,
            channel: Rc::default(),
            callbacks: Rc::default(),
            sink,
            drained: Rc::new(Notify::new()),
        };
        let watched = connection.peer.clone();
        let sink = connection.sink.clone();
        let on_state = Callback::new(move |_event: JsValue| {
            if matches!(
                watched.connection_state(),
                RtcPeerConnectionState::Failed | RtcPeerConnectionState::Closed
            ) {
                sink.close();
            }
        });
        connection
            .peer
            .set_onconnectionstatechange(Some(on_state.as_ref().unchecked_ref()));
        connection.callbacks.borrow_mut().push(on_state);
        Ok(connection)
    }

    /// Feeds `channel`'s events to the connection's sink.
    fn wire(
        channel: &RtcDataChannel,
        sink: &EventSink,
        slot: &Rc<RefCell<Option<RtcDataChannel>>>,
        callbacks: &Rc<RefCell<Vec<Callback>>>,
        drained: &Rc<Notify>,
    ) {
        channel.set_binary_type(RtcDataChannelType::Arraybuffer);
        channel.set_buffered_amount_low_threshold(
            u32::try_from(MAX_BUFFERED_BYTES / 2).unwrap_or(u32::MAX),
        );
        let low = Rc::clone(drained);
        let on_low = Callback::new(move |_event: JsValue| {
            low.notify_waiters();
        });
        let open = sink.clone();
        let on_open = Callback::new(move |_event: JsValue| {
            open.push(ChannelEvent::Open);
        });
        let messages = sink.clone();
        let on_message = Callback::new(move |event: JsValue| {
            // The wire is binary messages of bounded size; anything else ends
            // the link, before it is copied.
            let data = event.unchecked_into::<MessageEvent>().data();
            let Some(buffer) = data.dyn_ref::<ArrayBuffer>() else {
                messages.close();
                return;
            };
            if !fits(usize::try_from(buffer.byte_length()).unwrap_or(usize::MAX)) {
                messages.close();
                return;
            }
            let bytes = Uint8Array::new(buffer).to_vec();
            messages.push(ChannelEvent::Message(Bytes::from(bytes)));
        });
        let closed = sink.clone();
        let ended = Rc::clone(drained);
        let on_close = Callback::new(move |_event: JsValue| {
            closed.close();
            ended.notify_waiters();
        });
        channel.set_onbufferedamountlow(Some(on_low.as_ref().unchecked_ref()));
        channel.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        channel.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        channel.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        channel.set_onerror(Some(on_close.as_ref().unchecked_ref()));
        let mut callbacks = callbacks.borrow_mut();
        callbacks.push(on_low);
        callbacks.push(on_open);
        callbacks.push(on_message);
        callbacks.push(on_close);
        *slot.borrow_mut() = Some(channel.clone());
    }

    /// Sets the local description `kind` from `description`, gathers with
    /// the page's gathering turn, and returns the SDP with its candidates.
    async fn gathered_sdp(
        &self,
        kind: RtcSdpType,
        description: JsValue,
    ) -> Result<String, RtcError> {
        let sdp = Reflect::get(&description, &JsValue::from_str("sdp"))
            .map_err(js_error)?
            .as_string()
            .filter(|sdp| !sdp.is_empty())
            .ok_or_else(|| RtcError::new("browser made an empty session description"))?;
        let local = RtcSessionDescriptionInit::new(kind);
        local.set_sdp(&sdp);
        // After any connection to a node waiting for the turn, and one
        // discovery step at a time page-wide, held until this one has gathered:
        // finding other readers holds up a read's connection by one turn at
        // most.
        let _discovery = discovery_turn().await;
        let turn = ice_gathering_turn_after_reads().await;
        JsFuture::from(self.peer.set_local_description(&local))
            .await
            .map_err(js_error)?;
        if let Some(turn) = turn {
            pass_ice_gathering_turn(self.peer.clone(), turn);
        }
        let deadline = web_time::Instant::now() + GATHER_TIMEOUT;
        while self.peer.ice_gathering_state() != RtcIceGatheringState::Complete
            && web_time::Instant::now() < deadline
        {
            crate::runtime::sleep(POLL).await;
        }
        self.peer
            .local_description()
            .map(|description| description.sdp())
            .ok_or_else(|| RtcError::new("browser kept no local description"))
    }

    async fn set_remote(&self, kind: RtcSdpType, sdp: &str) -> Result<(), RtcError> {
        let remote = RtcSessionDescriptionInit::new(kind);
        remote.set_sdp(sdp);
        JsFuture::from(self.peer.set_remote_description(&remote))
            .await
            .map(|_| ())
            .map_err(js_error)
    }
}

impl ChannelSender for Connection {
    fn send(&self, message: Vec<u8>) -> BoxFuture<'_, Result<(), RtcError>> {
        Box::pin(async move {
            loop {
                let channel = self
                    .channel
                    .borrow()
                    .clone()
                    .ok_or_else(|| RtcError::new("channel not open"))?;
                if channel.ready_state() != RtcDataChannelState::Open {
                    return Err(RtcError::new("channel closed"));
                }
                // Made before the check, so a drain in between still wakes it.
                let drained = self.drained.notified();
                if usize::try_from(channel.buffered_amount()).unwrap_or(usize::MAX)
                    <= MAX_BUFFERED_BYTES
                {
                    return channel.send_with_u8_array(&message).map_err(js_error);
                }
                let drained = std::pin::pin!(drained);
                let fallback = std::pin::pin!(crate::runtime::sleep(DRAIN_FALLBACK));
                let _ = futures::future::select(drained, fallback).await;
            }
        })
    }

    fn close(&self) {
        if let Some(channel) = self.channel.borrow().as_ref() {
            channel.close();
        }
        self.peer.close();
        self.sink.close();
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(channel) = self.channel.borrow_mut().take() {
            channel.set_onbufferedamountlow(None);
            channel.set_onopen(None);
            channel.set_onmessage(None);
            channel.set_onclose(None);
            channel.set_onerror(None);
            channel.close();
        }
        self.peer.set_onconnectionstatechange(None);
        self.peer.set_ondatachannel(None);
        self.peer.close();
        self.sink.close();
    }
}

struct BrowserOffer {
    connection: Connection,
    events: futures::channel::mpsc::Receiver<ChannelEvent>,
}

impl PendingOffer for BrowserOffer {
    fn accept(self: Box<Self>, answer: String) -> BoxFuture<'static, Result<Channel, RtcError>> {
        Box::pin(async move {
            let BrowserOffer { connection, events } = *self;
            connection.set_remote(RtcSdpType::Answer, &answer).await?;
            Ok(Channel {
                sender: Box::new(connection),
                events,
            })
        })
    }
}

impl Rtc for BrowserRtc {
    fn offer(
        &self,
        label: String,
        ice_servers: Vec<String>,
    ) -> BoxFuture<'_, Result<Offer, RtcError>> {
        Box::pin(async move {
            let (sink, events) = channel_events();
            let connection = Connection::new(&ice_servers, sink)?;
            let init = RtcDataChannelInit::new();
            init.set_ordered(true);
            // The offerer makes the link's one channel; one the peer opens
            // breaks the protocol and ends the link.
            let refuse = connection.sink.clone();
            let on_channel = Callback::new(move |event: JsValue| {
                event
                    .unchecked_into::<RtcDataChannelEvent>()
                    .channel()
                    .close();
                refuse.close();
            });
            connection
                .peer
                .set_ondatachannel(Some(on_channel.as_ref().unchecked_ref()));
            connection.callbacks.borrow_mut().push(on_channel);
            let channel = connection
                .peer
                .create_data_channel_with_data_channel_dict(&label, &init);
            Connection::wire(
                &channel,
                &connection.sink,
                &connection.channel,
                &connection.callbacks,
                &connection.drained,
            );
            let offer = JsFuture::from(connection.peer.create_offer())
                .await
                .map_err(js_error)?;
            let sdp = connection.gathered_sdp(RtcSdpType::Offer, offer).await?;
            let pending = BrowserOffer { connection, events };
            Ok((Box::new(pending) as Box<dyn PendingOffer>, sdp))
        })
    }

    fn answer(
        &self,
        label: String,
        ice_servers: Vec<String>,
        offer: String,
    ) -> BoxFuture<'_, Result<(Channel, String), RtcError>> {
        Box::pin(async move {
            let (sink, events) = channel_events();
            let connection = Connection::new(&ice_servers, sink)?;
            let sink = connection.sink.clone();
            let slot = Rc::clone(&connection.channel);
            let callbacks = Rc::downgrade(&connection.callbacks);
            let drained = Rc::clone(&connection.drained);
            let on_channel = Callback::new(move |event: JsValue| {
                let channel = event.unchecked_into::<RtcDataChannelEvent>().channel();
                let Some(callbacks) = callbacks.upgrade() else {
                    return;
                };
                // One channel, by its label (its room's); any other breaks
                // the protocol and ends the link.
                if channel.label() == label && slot.borrow().is_none() {
                    Connection::wire(&channel, &sink, &slot, &callbacks, &drained);
                } else {
                    channel.close();
                    sink.close();
                }
            });
            connection
                .peer
                .set_ondatachannel(Some(on_channel.as_ref().unchecked_ref()));
            connection.callbacks.borrow_mut().push(on_channel);
            connection.set_remote(RtcSdpType::Offer, &offer).await?;
            let answer = JsFuture::from(connection.peer.create_answer())
                .await
                .map_err(js_error)?;
            let sdp = connection.gathered_sdp(RtcSdpType::Answer, answer).await?;
            Ok((
                Channel {
                    sender: Box::new(connection),
                    events,
                },
                sdp,
            ))
        })
    }
}
