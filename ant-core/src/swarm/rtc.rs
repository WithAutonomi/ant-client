//! The WebRTC data channels clients share records over, the same on every
//! platform: a browser uses its own `RTCPeerConnection`, a native client the
//! webrtc-rs stack.
//!
//! Signalling is non-trickle: an offer or answer is sent once its ICE
//! candidates are gathered, so one exchange through a node is enough.

use std::time::Duration;

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bytes::Bytes;
use futures::channel::mpsc;

#[cfg(feature = "native")]
pub(crate) use futures::future::BoxFuture;
#[cfg(not(feature = "native"))]
pub(crate) use futures::future::LocalBoxFuture as BoxFuture;

/// `Send + Sync` where tasks run on a thread pool (native); nothing in the
/// single-threaded browser.
#[cfg(feature = "native")]
pub(crate) trait MaybeSend: Send + Sync {}
#[cfg(feature = "native")]
impl<T: Send + Sync + ?Sized> MaybeSend for T {}
#[cfg(not(feature = "native"))]
pub(crate) trait MaybeSend {}
#[cfg(not(feature = "native"))]
impl<T: ?Sized> MaybeSend for T {}

/// The longest ICE gathering may take before an offer or answer goes with the
/// candidates found so far.
pub(crate) const GATHER_TIMEOUT: Duration = Duration::from_secs(3);
/// Bytes a channel may hold unsent before a sender waits. Small, so the first
/// piece of a record started while another is sending waits behind little:
/// browsers move a data channel at a few MB/s, and an ask with no piece for
/// [`super::link::FETCH_STALL`] is given up.
pub(crate) const MAX_BUFFERED_BYTES: usize = 256 * 1024;
/// Messages a channel may hold unread before the link is dropped: a peer
/// sending faster than this client reads is not one to keep. Each is at most
/// [`super::wire::MAX_MESSAGE_BYTES`], checked before it is queued, so a link
/// holds at most about 4 MiB unread.
pub(crate) const MAX_QUEUED_MESSAGES: usize = 256;
/// The start of the label of a link's one data channel. The rest is its
/// room's tag, so a channel opened for another room is refused, and a page
/// watching its own connections can tell which file a link shares.
pub(crate) const CHANNEL_LABEL_PREFIX: &str = "ant-swarm-";

/// The label of the one data channel on a link of the room tagged `tag`.
pub(crate) fn channel_label(tag: &str) -> String {
    format!("{CHANNEL_LABEL_PREFIX}{tag}")
}

/// Why a WebRTC step failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(crate) struct RtcError(pub(crate) String);

impl RtcError {
    pub(crate) fn new(message: impl std::fmt::Display) -> Self {
        Self(message.to_string())
    }
}

/// Whether a message of `length` bytes may be queued: a longer one breaks the
/// wire protocol, and the link is dropped rather than the message copied.
pub(crate) fn fits(length: usize) -> bool {
    length <= super::wire::MAX_MESSAGE_BYTES
}

/// What happens on a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ChannelEvent {
    /// The channel opened: messages can be sent.
    Open,
    /// One binary message.
    Message(Bytes),
    /// The channel or its connection closed, or the peer sent too much.
    Closed,
}

/// Sends on one data channel.
pub(crate) trait ChannelSender: MaybeSend {
    /// Sends one message, first waiting while the channel holds more than
    /// [`MAX_BUFFERED_BYTES`] unsent.
    fn send(&self, message: Vec<u8>) -> BoxFuture<'_, Result<(), RtcError>>;
    /// Closes the channel and its connection.
    fn close(&self);
}

/// One data channel to another client. Dropping it closes the connection.
pub(crate) struct Channel {
    pub(crate) sender: Box<dyn ChannelSender>,
    pub(crate) events: mpsc::Receiver<ChannelEvent>,
}

/// An offer waiting for its answer. Dropping it closes the connection.
pub(crate) trait PendingOffer: MaybeSend {
    /// Applies the answer; the channel opens once ICE connects.
    fn accept(self: Box<Self>, answer: String) -> BoxFuture<'static, Result<Channel, RtcError>>;
}

/// An offer waiting for its answer, and its SDP.
pub(crate) type Offer = (Box<dyn PendingOffer>, String);

/// Makes WebRTC connections.
pub(crate) trait Rtc: MaybeSend {
    /// Makes an offer with its gathered candidates, for a channel named
    /// `label`.
    fn offer(
        &self,
        label: String,
        ice_servers: Vec<String>,
    ) -> BoxFuture<'_, Result<Offer, RtcError>>;
    /// Answers an offer; the channel opens once ICE connects, if the offerer
    /// named it `label`.
    fn answer(
        &self,
        label: String,
        ice_servers: Vec<String>,
        offer: String,
    ) -> BoxFuture<'_, Result<(Channel, String), RtcError>>;
}

/// The channel events a backend feeds, and the receiver the swarm reads.
pub(crate) fn channel_events() -> (EventSink, mpsc::Receiver<ChannelEvent>) {
    let (sender, receiver) = mpsc::channel(MAX_QUEUED_MESSAGES);
    (EventSink(Arc::new(Mutex::new(sender))), receiver)
}

/// Feeds channel events from backend callbacks. A full queue closes the link.
///
/// Every callback shares the one sender: a `futures` channel holds its bound
/// plus one message per sender, so a sender cloned per message would never
/// find the queue full.
#[derive(Clone)]
pub(crate) struct EventSink(Arc<Mutex<mpsc::Sender<ChannelEvent>>>);

impl EventSink {
    fn sender(&self) -> MutexGuard<'_, mpsc::Sender<ChannelEvent>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queues `event`; returns `false` if the queue is full or gone, after
    /// which the reader sees the channel as closed.
    pub(crate) fn push(&self, event: ChannelEvent) -> bool {
        let mut sender = self.sender();
        if sender.try_send(event).is_ok() {
            return true;
        }
        sender.close_channel();
        false
    }

    pub(crate) fn close(&self) {
        let mut sender = self.sender();
        let _ = sender.try_send(ChannelEvent::Closed);
        sender.close_channel();
    }
}

/// The platform's WebRTC stack, if this build has one.
pub(crate) fn platform() -> Option<Arc<dyn Rtc>> {
    #[cfg(feature = "native")]
    {
        Some(Arc::new(super::native_rtc::NativeRtc::new()) as Arc<dyn Rtc>)
    }
    #[cfg(all(
        not(feature = "native"),
        target_arch = "wasm32",
        feature = "browser-wasm"
    ))]
    {
        Some(Arc::new(super::browser_rtc::BrowserRtc) as Arc<dyn Rtc>)
    }
    #[cfg(all(
        not(feature = "native"),
        not(all(target_arch = "wasm32", feature = "browser-wasm"))
    ))]
    {
        None
    }
}

/// An in-memory WebRTC stand-in for tests: an "SDP" names a slot in a shared
/// table, and accepting an answer joins the two ends.
#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    #[test]
    fn a_full_queue_closes_the_channel() {
        let (sink, mut events) = channel_events();
        let pushed = (0..2 * MAX_QUEUED_MESSAGES)
            .take_while(|_| sink.push(ChannelEvent::Message(Bytes::from_static(b"x"))))
            .count();
        assert_eq!(pushed, MAX_QUEUED_MESSAGES + 1);
        assert!(!sink.clone().push(ChannelEvent::Open));
        let queued = futures::executor::block_on(async {
            let mut queued = 0;
            while events.next().await.is_some() {
                queued += 1;
            }
            queued
        });
        assert_eq!(queued, MAX_QUEUED_MESSAGES + 1);
    }
}

#[cfg(test)]
pub(crate) mod memory {
    use super::*;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    type Ends = (EventSink, mpsc::Receiver<ChannelEvent>);
    /// Where the other end's sink goes once it answers.
    type PeerSlot = Arc<Mutex<Option<EventSink>>>;

    #[derive(Default)]
    pub(crate) struct MemoryRtc {
        next: Mutex<u64>,
        /// Offer ends waiting for an answerer, and their channel's label, by
        /// offer SDP.
        offers: Mutex<HashMap<String, (PeerSlot, EventSink, String)>>,
        /// Whether the next offers or answers fail.
        pub(crate) failing: AtomicBool,
        /// Whether answerers learn their channel opened only after the
        /// offerer's first message, as webrtc-rs may report it.
        pub(crate) open_late: AtomicBool,
    }

    struct MemorySender {
        peer: PeerSlot,
        own: EventSink,
        /// An `Open` the peer is still owed, given after this end's first
        /// message.
        open_after: AtomicBool,
    }

    impl ChannelSender for MemorySender {
        fn send(&self, message: Vec<u8>) -> BoxFuture<'_, Result<(), RtcError>> {
            let peer = self
                .peer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let open_after = self.open_after.swap(false, Ordering::AcqRel);
            Box::pin(async move {
                let peer = peer.ok_or_else(|| RtcError::new("not connected"))?;
                let sent = peer.push(ChannelEvent::Message(Bytes::from(message)));
                if open_after {
                    peer.push(ChannelEvent::Open);
                }
                if sent {
                    Ok(())
                } else {
                    Err(RtcError::new("closed"))
                }
            })
        }

        fn close(&self) {
            if let Some(peer) = self
                .peer
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                peer.close();
            }
            self.own.close();
        }
    }

    struct MemoryOffer {
        rtc: Arc<MemoryRtc>,
        sdp: String,
        ends: Option<Ends>,
        peer: PeerSlot,
    }

    impl PendingOffer for MemoryOffer {
        fn accept(
            mut self: Box<Self>,
            answer: String,
        ) -> BoxFuture<'static, Result<Channel, RtcError>> {
            let ends = self.ends.take();
            let peer = Arc::clone(&self.peer);
            let rtc = Arc::clone(&self.rtc);
            let sdp = self.sdp.clone();
            Box::pin(async move {
                let (sink, events) = ends.ok_or_else(|| RtcError::new("used"))?;
                if answer != format!("answer:{sdp}") {
                    return Err(RtcError::new("wrong answer"));
                }
                rtc.offers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&sdp);
                let connected = peer.lock().unwrap_or_else(PoisonError::into_inner).clone();
                let connected = connected.ok_or_else(|| RtcError::new("never answered"))?;
                sink.push(ChannelEvent::Open);
                let open_late = rtc.open_late.load(Ordering::Acquire);
                if !open_late {
                    connected.push(ChannelEvent::Open);
                }
                Ok(Channel {
                    sender: Box::new(MemorySender {
                        peer,
                        own: sink,
                        open_after: AtomicBool::new(open_late),
                    }),
                    events,
                })
            })
        }
    }

    /// A handle that lets several test clients share one table.
    #[derive(Clone, Default)]
    pub(crate) struct SharedMemoryRtc(pub(crate) Arc<MemoryRtc>);

    impl Rtc for SharedMemoryRtc {
        fn offer(
            &self,
            label: String,
            _ice_servers: Vec<String>,
        ) -> BoxFuture<'_, Result<Offer, RtcError>> {
            Box::pin(async move {
                if self.0.failing.load(Ordering::Relaxed) {
                    return Err(RtcError::new("failing"));
                }
                let id = {
                    let mut next = self.0.next.lock().unwrap_or_else(PoisonError::into_inner);
                    *next += 1;
                    *next
                };
                let sdp = format!("offer-{id}");
                let (sink, events) = channel_events();
                let peer = Arc::new(Mutex::new(None));
                self.0
                    .offers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(sdp.clone(), (Arc::clone(&peer), sink.clone(), label));
                let offer = MemoryOffer {
                    rtc: Arc::clone(&self.0),
                    sdp: sdp.clone(),
                    ends: Some((sink, events)),
                    peer,
                };
                Ok((Box::new(offer) as Box<dyn PendingOffer>, sdp))
            })
        }

        fn answer(
            &self,
            label: String,
            _ice_servers: Vec<String>,
            offer: String,
        ) -> BoxFuture<'_, Result<(Channel, String), RtcError>> {
            Box::pin(async move {
                if self.0.failing.load(Ordering::Relaxed) {
                    return Err(RtcError::new("failing"));
                }
                let (offer_peer, offer_sink, offered_label) = self
                    .0
                    .offers
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .get(&offer)
                    .cloned()
                    .ok_or_else(|| RtcError::new("unknown offer"))?;
                // As a browser or webrtc-rs answerer refuses the channel.
                if offered_label != label {
                    return Err(RtcError::new("a channel of another room"));
                }
                let (sink, events) = channel_events();
                *offer_peer.lock().unwrap_or_else(PoisonError::into_inner) = Some(sink.clone());
                let channel = Channel {
                    sender: Box::new(MemorySender {
                        peer: Arc::new(Mutex::new(Some(offer_sink))),
                        own: sink,
                        open_after: AtomicBool::new(false),
                    }),
                    events,
                };
                Ok((channel, format!("answer:{offer}")))
            })
        }
    }
}
