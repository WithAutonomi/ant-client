//! WebRTC data channels for native clients, on the webrtc-rs stack.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use bytes::Bytes;
use webrtc::api::setting_engine::SettingEngine;
use webrtc::api::{APIBuilder, API};
use webrtc::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;
use webrtc::data_channel::RTCDataChannel;
use webrtc::ice::mdns::MulticastDnsMode;
use webrtc::ice_transport::ice_server::RTCIceServer;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::peer_connection_state::RTCPeerConnectionState;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::peer_connection::RTCPeerConnection;

use super::rtc::{
    channel_events, fits, BoxFuture, Channel, ChannelEvent, ChannelSender, EventSink, Offer,
    PendingOffer, Rtc, RtcError, GATHER_TIMEOUT, MAX_BUFFERED_BYTES,
};

/// How often a sender looks again at a full channel.
const DRAIN_POLL: Duration = Duration::from_millis(10);

pub(super) struct NativeRtc {
    api: API,
}

impl NativeRtc {
    pub(super) fn new() -> Self {
        // DTLS asks rustls for the process-wide provider, which rustls cannot
        // pick by itself with both compiled in. Name the one QUIC uses; a
        // provider the application installed first stays.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut settings = SettingEngine::default();
        // Accept the mDNS host names browsers give their candidates; offer
        // plain addresses of this host's own.
        settings.set_ice_multicast_dns_mode(MulticastDnsMode::QueryOnly);
        // Two clients on one host meet over loopback too.
        settings.set_include_loopback_candidate(true);
        Self {
            api: APIBuilder::new().with_setting_engine(settings).build(),
        }
    }

    async fn connection(
        &self,
        ice_servers: Vec<String>,
        sink: &EventSink,
    ) -> Result<Arc<RTCPeerConnection>, RtcError> {
        let ice_servers = if ice_servers.is_empty() {
            Vec::new()
        } else {
            vec![RTCIceServer {
                urls: ice_servers,
                ..RTCIceServer::default()
            }]
        };
        let connection = self
            .api
            .new_peer_connection(RTCConfiguration {
                ice_servers,
                ..RTCConfiguration::default()
            })
            .await
            .map_err(RtcError::new)?;
        let sink = sink.clone();
        connection.on_peer_connection_state_change(Box::new(move |state| {
            if matches!(
                state,
                RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
            ) {
                sink.close();
            }
            Box::pin(async {})
        }));
        Ok(Arc::new(connection))
    }
}

/// Feeds a data channel's events to `sink`.
fn wire(channel: &Arc<RTCDataChannel>, sink: &EventSink) {
    let open = sink.clone();
    channel.on_open(Box::new(move || {
        open.push(ChannelEvent::Open);
        Box::pin(async {})
    }));
    let messages = sink.clone();
    channel.on_message(Box::new(move |message: DataChannelMessage| {
        // The wire is binary messages of bounded size; anything else ends
        // the link.
        if message.is_string || !fits(message.data.len()) {
            messages.close();
        } else {
            messages.push(ChannelEvent::Message(message.data));
        }
        Box::pin(async {})
    }));
    let closed = sink.clone();
    channel.on_close(Box::new(move || {
        closed.close();
        Box::pin(async {})
    }));
}

/// Sets `description` and waits for ICE gathering, then returns the SDP with
/// the candidates found.
async fn gathered_sdp(
    connection: &RTCPeerConnection,
    description: RTCSessionDescription,
) -> Result<String, RtcError> {
    let mut gathered = connection.gathering_complete_promise().await;
    connection
        .set_local_description(description)
        .await
        .map_err(RtcError::new)?;
    let _ = tokio::time::timeout(GATHER_TIMEOUT, gathered.recv()).await;
    connection
        .local_description()
        .await
        .map(|description| description.sdp)
        .ok_or_else(|| RtcError::new("no local description"))
}

/// Sends on a link's data channel; dropping it closes the connection.
struct NativeSender {
    connection: Arc<RTCPeerConnection>,
    /// The answerer's channel arrives once the connection is up.
    channel: Arc<Mutex<Option<Arc<RTCDataChannel>>>>,
}

impl ChannelSender for NativeSender {
    fn send(&self, message: Vec<u8>) -> BoxFuture<'_, Result<(), RtcError>> {
        Box::pin(async move {
            let channel = self
                .channel
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone()
                .ok_or_else(|| RtcError::new("channel not open"))?;
            while channel.buffered_amount().await > MAX_BUFFERED_BYTES {
                if channel.ready_state() != RTCDataChannelState::Open {
                    return Err(RtcError::new("channel closed"));
                }
                tokio::time::sleep(DRAIN_POLL).await;
            }
            channel
                .send(&Bytes::from(message))
                .await
                .map(|_| ())
                .map_err(RtcError::new)
        })
    }

    fn close(&self) {
        // Closing awaits the stack, which needs a runtime; at exit there may
        // be none, and the process ends the connection anyway.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let connection = Arc::clone(&self.connection);
            drop(runtime.spawn(async move {
                let _ = connection.close().await;
            }));
        }
    }
}

impl Drop for NativeSender {
    fn drop(&mut self) {
        self.close();
    }
}

struct NativeOffer {
    sender: NativeSender,
    events: futures::channel::mpsc::Receiver<ChannelEvent>,
}

impl PendingOffer for NativeOffer {
    fn accept(self: Box<Self>, answer: String) -> BoxFuture<'static, Result<Channel, RtcError>> {
        Box::pin(async move {
            let NativeOffer { sender, events } = *self;
            let description = RTCSessionDescription::answer(answer).map_err(RtcError::new)?;
            sender
                .connection
                .set_remote_description(description)
                .await
                .map_err(RtcError::new)?;
            Ok(Channel {
                sender: Box::new(sender),
                events,
            })
        })
    }
}

impl Rtc for NativeRtc {
    fn offer(
        &self,
        label: String,
        ice_servers: Vec<String>,
    ) -> BoxFuture<'_, Result<Offer, RtcError>> {
        Box::pin(async move {
            let (sink, events) = channel_events();
            let connection = self.connection(ice_servers, &sink).await?;
            let sender = NativeSender {
                connection: Arc::clone(&connection),
                channel: Arc::default(),
            };
            // The offerer makes the link's one channel; one the peer opens
            // breaks the protocol and ends the link.
            let refuse = sink.clone();
            connection.on_data_channel(Box::new(move |_channel: Arc<RTCDataChannel>| {
                refuse.close();
                Box::pin(async {})
            }));
            let channel = connection
                .create_data_channel(&label, None)
                .await
                .map_err(RtcError::new)?;
            wire(&channel, &sink);
            *sender
                .channel
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(channel);
            let offer = connection.create_offer(None).await.map_err(RtcError::new)?;
            let sdp = gathered_sdp(&connection, offer).await?;
            let pending = NativeOffer { sender, events };
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
            let connection = self.connection(ice_servers, &sink).await?;
            let sender = NativeSender {
                connection: Arc::clone(&connection),
                channel: Arc::default(),
            };
            let slot = Arc::clone(&sender.channel);
            connection.on_data_channel(Box::new(move |channel: Arc<RTCDataChannel>| {
                let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
                // One channel, by its label (its room's); any other breaks
                // the protocol and ends the link.
                if channel.label() == label && slot.is_none() {
                    wire(&channel, &sink);
                    *slot = Some(channel);
                } else {
                    sink.close();
                }
                Box::pin(async {})
            }));
            let description = RTCSessionDescription::offer(offer).map_err(RtcError::new)?;
            connection
                .set_remote_description(description)
                .await
                .map_err(RtcError::new)?;
            let answer = connection
                .create_answer(None)
                .await
                .map_err(RtcError::new)?;
            let sdp = gathered_sdp(&connection, answer).await?;
            Ok((
                Channel {
                    sender: Box::new(sender),
                    events,
                },
                sdp,
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swarm::rtc::channel_label;
    use futures::StreamExt;

    /// How long a loopback connection may take to open, or to refuse.
    const CONNECT: Duration = Duration::from_secs(20);

    /// An offer for a channel labelled `offered`, answered by an answerer
    /// that takes only `answered`: the answerer's first channel event.
    async fn answerers_first_event(offered: &str, answered: &str) -> Option<ChannelEvent> {
        let rtc = NativeRtc::new();
        let (offer, sdp) = rtc.offer(offered.to_string(), Vec::new()).await.ok()?;
        let (mut answerer, answer) = rtc
            .answer(answered.to_string(), Vec::new(), sdp)
            .await
            .ok()?;
        // Kept until the answerer has seen its channel, or refused it.
        let _offerer = offer.accept(answer).await.ok()?;
        tokio::time::timeout(CONNECT, answerer.events.next())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn a_channel_labelled_for_another_room_is_refused() {
        let room = channel_label("00112233aabbccdd");
        let other = channel_label("ffeeddcc44332211");
        assert_eq!(
            answerers_first_event(&room, &room).await,
            Some(ChannelEvent::Open)
        );
        assert_eq!(
            answerers_first_event(&room, &other).await,
            Some(ChannelEvent::Closed)
        );
    }
}
