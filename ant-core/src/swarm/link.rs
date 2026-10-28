//! One WebRTC link to another client in a room: what it says it holds, the
//! records this client asked it for, and the records it asked this client for.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use ant_protocol::pqc::api::MlDsaPublicKey;
use ant_protocol::{OfferId, RoomKey};
use bytes::Bytes;
use futures::channel::mpsc;
use futures::StreamExt;
use web_time::Instant;

use super::room::{count, member_key, name_of, signed_by, Room};
use super::rtc::{ChannelEvent, ChannelSender};
use super::wire::{self, Assembly, Message, MAX_HAVE_ADDRESSES};
use super::{or_stopped, spawn, Counters, DiscoveryCounters, Inner, Pace, PeerRef, Stop};
use crate::data::XorName;

/// How long a new link may take to open.
pub(super) const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
/// How long a record may go without a piece arriving.
pub(super) const FETCH_STALL: std::time::Duration = std::time::Duration::from_secs(2);
/// How long one read may spend on peers, all of them together, before it asks
/// the nodes.
pub(super) const FETCH_TOTAL: std::time::Duration = std::time::Duration::from_secs(12);
/// The slowest a peer may send a record, in bytes a second: past this, the
/// nodes are likely faster. With [`FETCH_STALL`] it sets a record's deadline
/// once its size is known.
const MIN_PEER_RATE: u64 = 512 * 1024;
/// How long a peer that ran out of time is passed over.
const SLOW_PEER_PAUSE: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a link closed as the duplicate of another stays connected. The
/// peer may not have this end's proof on it yet, and needs it to settle on the
/// same link itself and to learn that both reach this client: a connection
/// torn down at once looks to it like a link that failed, and it offers the
/// same member again, and again.
const SETTLE_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// How long sending one record may take before the link is dropped: longer
/// than any asker waits for it.
const SEND_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
const _: () = assert!(SEND_DEADLINE.as_secs() > FETCH_TOTAL.as_secs());
/// Pieces of one record sent before it lets others have the channel: at 16
/// KiB each, a record waits at most 64 KiB behind another's.
pub(super) const PIECES_PER_TURN: usize = 4;
/// Records this client asks one link for at once. Further reads of that
/// link's records wait here for their turn, where their wait is seen, rather
/// than in the peer's queue, where it is not.
///
/// One: Chromium's data channel stalled for seconds when sent two records
/// at once, a few pieces of each at a time (a 2 MiB record took 3.6 to 5.3 s
/// that way, and 66 to 656 ms alone). A record's pieces still fill the
/// channel, and a read spreads over links.
const MAX_GETS_PER_LINK: usize = 1;
/// Reads that may wait for a turn on one link, behind the one it is asked
/// for: enough to keep the link busy back to back. Reads past that go to the
/// nodes rather than queue behind a peer that may be slower than they are
/// (on a local network, queueing made the last of four readers starting 1.5 s
/// apart 11.5 s slower than with no peers at all).
const MAX_WAITING_PER_LINK: usize = 1;
/// Records one link may be sent by this client at once; past this it is told
/// the client is busy. One more than a peer asks for at once: a record the
/// peer gave up on is still being sent until the next piece sees it, and the
/// peer's next ask may come first.
const MAX_SENDING_PER_LINK: usize = MAX_GETS_PER_LINK + 1;
/// Other names remembered for the peer on one link: one per rendezvous
/// node, twice over.
const MAX_ALIASES: usize = 2 * ant_protocol::SWARM_RENDEZVOUS_NODES;
/// Addresses remembered for what one link holds.
const MAX_PEER_HAS: usize = 16_384;
/// Addresses sent in the first `Have` on a new link.
pub(super) const INITIAL_HAVE: usize = 4096;
/// Messages waiting to be sent on one link before it is dropped.
const MAX_OUTBOX: usize = 1024;

/// What became of one record asked for.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum GetOutcome {
    /// The whole record, not yet checked against its address.
    Record(Bytes),
    /// The peer does not have it.
    Missing,
    /// The peer has it but is busy, or this link has as many asks in flight
    /// as it may.
    Busy,
    /// This link has as many reads asking and waiting as it takes: the read
    /// goes elsewhere.
    Full,
    /// The link failed, or the record did not arrive in time.
    Failed,
}

enum GetEvent {
    /// A piece of a record `total` bytes long arrived, `received` of them
    /// so far.
    Progress {
        total: u32,
        received: u32,
    },
    Done(Bytes),
    Missing,
    Busy,
    Failed,
}

struct Get {
    address: XorName,
    assembly: Option<Assembly>,
    events: mpsc::UnboundedSender<GetEvent>,
}

#[derive(Default)]
struct State {
    open: bool,
    /// The peer's name in the room, from the key its `Hello` named, once its
    /// `Proof` showed it holds that key: an unproven link has no name, and
    /// so settles no duplicate.
    peer_nonce: Option<[u8; 16]>,
    has: HashSet<XorName>,
    has_order: VecDeque<XorName>,
    gets: HashMap<u32, Get>,
    next_get: u32,
    /// The peer's asks being answered, by id, each with its transfer's
    /// number: an id cancelled and asked again names a new transfer, and the
    /// old one stops.
    serving: HashMap<u32, u64>,
    next_transfer: u64,
    /// Transfers running, cancelled ones included until they stop: the
    /// link's share of the upload turns.
    transfers: usize,
    /// Other names the peer has at the room's nodes, learnt from links to it
    /// settled away.
    aliases: VecDeque<PeerRef>,
    /// Until when the link is passed over, after a record from it ran out of
    /// time.
    slow_until: Option<Instant>,
}

#[derive(Default)]
struct Outbox {
    control: VecDeque<Message>,
    haves: Vec<XorName>,
}

/// A link to one other client in one room.
pub(super) struct Link {
    pub(super) id: u64,
    pub(super) room: RoomKey,
    /// This end's DTLS fingerprint on the link's connection, as its own SDP
    /// gave it: how a page watching its connections tells which one an open
    /// link is.
    pub(super) fingerprint: String,
    /// What the link's room membership fetched and served.
    pub(super) room_counters: Arc<Counters>,
    /// Whether this client made the offer the link came from.
    pub(super) offered: bool,
    /// The offer's id: with the offerer's nonce, it decides which of two links
    /// between the same pair of clients stays.
    pub(super) offer_id: OfferId,
    /// This client's name in the link's room: its own in each room, so peers
    /// meeting it in two rooms cannot tell it is the same client by its name
    /// (its addresses, in its WebRTC candidates, may still let them). It also
    /// tells one membership of a room from a later one.
    pub(super) nonce: [u8; 16],
    /// The peer as the node that introduced it names it, if known.
    pub(super) peer: Option<PeerRef>,
    /// This client's proof for the peer, and the one it expects back.
    own_proof: blake3::Hash,
    peer_proof: blake3::Hash,
    /// This client's key in the room, and its signature over this link.
    own_key: Vec<u8>,
    own_signature: Vec<u8>,
    /// What the peer signs over this link.
    peer_signed: [u8; 32],
    sender: Box<dyn ChannelSender>,
    state: Mutex<State>,
    outbox: Mutex<Outbox>,
    wake: tokio::sync::Notify,
    /// This client's turns to ask the link for a record.
    asking: Arc<tokio::sync::Semaphore>,
    /// Reads waiting for one of those turns.
    waiting: AtomicUsize,
    /// Turns to send one message on the channel, handed out in order, so
    /// records sent at once interleave: in the single-threaded browser a send
    /// that need not wait never yields, and one record would take every turn.
    sending: tokio::sync::Mutex<()>,
    closed: Stop,
    /// Closed as the duplicate of a link kept.
    settled: AtomicBool,
}

impl Link {
    /// A link in `room` for the offer `offer_id`, which this client made if
    /// `offered`, over the connection `binding` names; `None` if this
    /// client's signature over it cannot be made.
    pub(super) fn new(
        id: u64,
        room: &Room,
        offered: bool,
        offer_id: OfferId,
        binding: &Binding,
        peer: Option<PeerRef>,
        sender: Box<dyn ChannelSender>,
    ) -> Option<Self> {
        let fingerprint = binding.fingerprint.clone();
        let binding = &binding.proof;
        let own_signature = room.identity.sign(&signed(binding, &offer_id, offered))?;
        Some(Self {
            id,
            room: room.key,
            fingerprint,
            room_counters: Arc::clone(&room.counters),
            offered,
            offer_id,
            nonce: room.nonce,
            peer,
            own_proof: proof(&room.secret, binding, &offer_id, offered),
            peer_proof: proof(&room.secret, binding, &offer_id, !offered),
            own_key: room.identity.public().to_vec(),
            own_signature,
            peer_signed: signed(binding, &offer_id, !offered),
            sender,
            state: Mutex::default(),
            outbox: Mutex::default(),
            wake: tokio::sync::Notify::new(),
            asking: Arc::new(tokio::sync::Semaphore::new(MAX_GETS_PER_LINK)),
            waiting: AtomicUsize::new(0),
            sending: tokio::sync::Mutex::new(()),
            closed: Stop::default(),
            settled: AtomicBool::new(false),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) fn is_open(&self) -> bool {
        self.state().open && !self.closed.is_stopped()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.is_stopped()
    }

    /// Whether the peer said it holds `address`, and has not lately been too
    /// slow to ask.
    pub(super) fn offers(&self, address: &XorName) -> bool {
        let state = self.state();
        let slow = state.slow_until.is_some_and(|until| Instant::now() < until);
        !slow && state.has.contains(address)
    }

    #[cfg(test)]
    pub(super) fn transfers_running(&self) -> usize {
        self.state().transfers
    }

    #[cfg(test)]
    pub(super) fn has(&self, address: &XorName) -> bool {
        self.state().has.contains(address)
    }

    pub(super) fn peer_nonce(&self) -> Option<[u8; 16]> {
        self.state().peer_nonce
    }

    /// Notes another name the peer has, at another rendezvous node: its
    /// newest at each node, for the newest few nodes, as a long read's
    /// rendezvous nodes change.
    pub(super) fn also_known_as(&self, peer: Option<PeerRef>) {
        let Some(peer) = peer else {
            return;
        };
        let mut state = self.state();
        if self.peer != Some(peer) {
            state.aliases.retain(|(node, _)| *node != peer.0);
            if state.aliases.len() >= MAX_ALIASES {
                state.aliases.pop_front();
            }
            state.aliases.push_back(peer);
        }
    }

    /// Whether the peer is a member of rendezvous node `node`, by a name
    /// this client knows.
    pub(super) fn known_at(&self, node: &[u8; 32]) -> bool {
        self.peer.is_some_and(|(at, _)| at == *node)
            || self.state().aliases.iter().any(|(at, _)| at == node)
    }

    /// Closes the link as the duplicate of another to the same peer, kept:
    /// a read of it goes on to that one. The link stops at once; its
    /// connection goes after [`SETTLE_GRACE`], and until then the link stays
    /// among `swarm`'s links, closed, so it still counts against their
    /// limits.
    pub(super) fn settle_away(self: &Arc<Self>, swarm: &Weak<Inner>) {
        // The first to settle it sees to its end, even of a link stopped
        // already (by a room left meanwhile, say), which `finish` now leaves
        // to the timer; a later call leaves that grace be.
        if self.settled.swap(true, Ordering::AcqRel) {
            return;
        }
        self.stop();
        let link = Arc::clone(self);
        let timer_swarm = Weak::clone(swarm);
        let later = spawn(async move {
            crate::runtime::sleep(SETTLE_GRACE).await;
            link.sender.close();
            if let Some(inner) = timer_swarm.upgrade() {
                inner.remove_link(link.id);
            }
        });
        if !later {
            self.sender.close();
            if let Some(inner) = swarm.upgrade() {
                inner.remove_link(self.id);
            }
        }
    }

    /// Whether the link was closed as the duplicate of one kept.
    pub(super) fn settled_away(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }

    /// Closes the link and its connection, and fails every record asked of
    /// it.
    pub(super) fn close(&self) {
        self.stop();
        self.sender.close();
    }

    /// Stops the link, leaving its connection be, and fails every record
    /// asked of it. Returns whether it was running until now.
    fn stop(&self) -> bool {
        if self.closed.is_stopped() {
            return false;
        }
        self.closed.stop();
        let gets = std::mem::take(&mut self.state().gets);
        for get in gets.into_values() {
            let _ = get.events.unbounded_send(GetEvent::Failed);
        }
        self.wake.notify_waiters();
        true
    }

    /// Queues a message for the writer; a link that cannot keep up is closed.
    fn queue(&self, message: Message) -> bool {
        {
            let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
            if outbox.control.len() >= MAX_OUTBOX {
                drop(outbox);
                self.close();
                return false;
            }
            outbox.control.push_back(message);
        }
        self.wake.notify_one();
        true
    }

    /// Tells the peer about records this client holds.
    pub(super) fn announce(&self, addresses: &[XorName]) {
        {
            let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
            outbox.haves.extend_from_slice(addresses);
            let excess = outbox.haves.len().saturating_sub(MAX_PEER_HAS);
            outbox.haves.drain(..excess);
        }
        self.wake.notify_one();
    }

    /// Whether a read may ask this link now: a turn is free, or fewer reads
    /// wait for one than may.
    pub(super) fn takes_asks(&self) -> bool {
        self.asking.available_permits() > 0
            || self.waiting.load(Ordering::Acquire) < MAX_WAITING_PER_LINK
    }

    /// Asks the peer for a record and waits for it, until `deadline` at the
    /// latest. A peer that runs out of time is passed over for a while.
    pub(super) async fn get(&self, address: XorName, deadline: Instant, pace: &Pace) -> GetOutcome {
        pace.note(0, 0);
        // A turn now, or a place among the few waiting for one while the
        // link's ask finishes; past that the read goes elsewhere.
        let _turn = match Arc::clone(&self.asking).try_acquire_owned() {
            Ok(turn) => turn,
            Err(_) => {
                let Some(_waiting) = Waiting::enter(&self.waiting) else {
                    return GetOutcome::Full;
                };
                let left = deadline.saturating_duration_since(Instant::now());
                let turn = crate::runtime::timeout(
                    left,
                    or_stopped(&self.closed, Arc::clone(&self.asking).acquire_owned()),
                )
                .await;
                let Ok(Some(Ok(turn))) = turn else {
                    return GetOutcome::Failed;
                };
                turn
            }
        };
        if !self.offers(&address) {
            return GetOutcome::Missing;
        }
        let (sender, mut events) = mpsc::unbounded();
        let id = {
            let mut state = self.state();
            if !state.open || state.gets.len() >= MAX_GETS_PER_LINK {
                return GetOutcome::Busy;
            }
            let mut id = state.next_get;
            while state.gets.contains_key(&id) {
                id = id.wrapping_add(1);
            }
            state.next_get = id.wrapping_add(1);
            state.gets.insert(
                id,
                Get {
                    address,
                    assembly: None,
                    events: sender,
                },
            );
            id
        };
        // The reader forgets an ask the peer answered; one still known when
        // this returns was given up on, and the peer is told.
        let _guard = GetGuard { link: self, id };
        if !self.queue(Message::Get { id, address }) {
            return GetOutcome::Failed;
        }
        let started = Instant::now();
        let mut deadline = deadline;
        // The time the peer's record may take, once its size is known.
        let mut sending_by: Option<Instant> = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let wait = FETCH_STALL.min(left);
            match crate::runtime::timeout(wait, events.next()).await {
                Ok(Some(GetEvent::Progress { total, received })) => {
                    pace.note(u64::from(received), u64::from(total));
                    if sending_by.is_none() {
                        let sending = std::time::Duration::from_millis(
                            u64::from(total).saturating_mul(1000) / MIN_PEER_RATE,
                        );
                        let by = started + FETCH_STALL + sending;
                        sending_by = Some(by);
                        deadline = deadline.min(by);
                    }
                }
                Ok(Some(GetEvent::Done(record))) => return GetOutcome::Record(record),
                Ok(Some(GetEvent::Missing)) => return GetOutcome::Missing,
                Ok(Some(GetEvent::Busy)) => return GetOutcome::Busy,
                Ok(Some(GetEvent::Failed) | None) => return GetOutcome::Failed,
                Err(_) => {
                    // A peer that sent nothing for a whole stall, or ran past
                    // the time its record's size allows, is passed over a
                    // while; the read's own time running out says nothing of
                    // the peer.
                    let stalled = wait >= FETCH_STALL;
                    let overran = sending_by.is_some_and(|by| Instant::now() >= by);
                    if stalled || overran {
                        self.state().slow_until = Some(Instant::now() + SLOW_PEER_PAUSE);
                    }
                    return GetOutcome::Failed;
                }
            }
        }
    }

    /// Reads the link until it closes. `swarm` is the swarm it belongs to.
    pub(super) async fn run(
        self: Arc<Self>,
        swarm: Weak<Inner>,
        mut events: mpsc::Receiver<ChannelEvent>,
    ) {
        // One deadline for the channel to open and the peer to prove itself.
        // A link this client answered starts before the offerer has even had
        // the answer, so it waits for that too.
        let within = if self.offered {
            CONNECT_TIMEOUT
        } else {
            super::room::ANSWERED_OPEN_TIMEOUT
        };
        let deadline = Instant::now() + within;
        // A message comes only on an open channel, and some stacks report
        // the opening after it (webrtc-rs runs its open callback on a task of
        // its own, beside the one reading messages): a message before `Open`
        // opens the link, and is read first.
        let opened = crate::runtime::timeout(
            within,
            or_stopped(&self.closed, async {
                match events.next().await {
                    Some(ChannelEvent::Open) => Some(None),
                    Some(ChannelEvent::Message(bytes)) => Some(Some(bytes)),
                    Some(ChannelEvent::Closed) | None => None,
                }
            }),
        )
        .await;
        let Ok(Some(Some(mut early))) = opened else {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        };
        count(&swarm, 1, |discovery| &discovery.channels_opened);
        // This end's name and proof go out before the peer's are read, not
        // through the writer: a link settled away as a duplicate the moment
        // the peer's proof is read has still given the peer its own, so the
        // peer settles the same way rather than offering again.
        let introduced = crate::runtime::timeout(
            deadline.saturating_duration_since(Instant::now()),
            or_stopped(&self.closed, async {
                self.send(
                    Message::Hello {
                        key: self.own_key.clone(),
                    }
                    .encode(),
                )
                .await?;
                self.send(
                    Message::Proof {
                        mac: *self.own_proof.as_bytes(),
                        signature: self.own_signature.clone(),
                    }
                    .encode(),
                )
                .await
            }),
        )
        .await;
        if !matches!(introduced, Ok(Some(Ok(())))) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        if !spawn(Arc::clone(&self).write()) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        // The peer proves it knows the room's secret, and holds the key it
        // names itself by, for this connection, before anything else: until
        // then nothing of the room is said or served, and its name settles no
        // duplicate.
        let proven = crate::runtime::timeout(
            deadline.saturating_duration_since(Instant::now()),
            or_stopped(&self.closed, async {
                // The name and key the peer's `Hello` gave, until its proof.
                let mut named: Option<([u8; 16], MlDsaPublicKey)> = None;
                loop {
                    let event = match early.take() {
                        Some(bytes) => ChannelEvent::Message(bytes),
                        None => match events.next().await {
                            Some(event) => event,
                            None => return false,
                        },
                    };
                    let bytes = match event {
                        ChannelEvent::Message(bytes) => bytes,
                        ChannelEvent::Open => continue,
                        ChannelEvent::Closed => return false,
                    };
                    // `Hello` first, once, then `Proof`: anything else ends
                    // the link, so every open link has its peer's name.
                    match Message::decode(&bytes) {
                        Ok(Message::Hello { key }) => {
                            if named.is_some() {
                                return false;
                            }
                            let Some(peer_key) = member_key(&key) else {
                                return false;
                            };
                            named = Some((name_of(&key), peer_key));
                        }
                        Ok(Message::Proof { mac, signature }) => {
                            let Some((name, peer_key)) = named.take() else {
                                return false;
                            };
                            if blake3::Hash::from_bytes(mac) != self.peer_proof
                                || !signed_by(&peer_key, &self.peer_signed, &signature)
                            {
                                return false;
                            }
                            self.state().peer_nonce = Some(name);
                            return true;
                        }
                        _ => return false,
                    }
                }
            }),
        )
        .await;
        if !matches!(proven, Ok(Some(true))) {
            count(&swarm, 1, |discovery| &discovery.links_failed);
            self.finish(&swarm);
            return;
        }
        let Some(inner) = swarm.upgrade() else {
            self.close();
            return;
        };
        DiscoveryCounters::add(&inner.discovery.links_proven, 1);
        self.state().open = true;
        inner.settle_duplicates(&self);
        // Only the records of this link's room's file.
        if let Some(room) = inner.room(&self.room) {
            self.announce(
                &inner
                    .store
                    .newest_where(INITIAL_HAVE, |address| room.holds(address)),
            );
        }
        drop(inner);
        while let Some(Some(event)) = or_stopped(&self.closed, events.next()).await {
            let keep = match event {
                ChannelEvent::Message(bytes) => match Message::decode(&bytes) {
                    Ok(message) => self.handle(message, &swarm),
                    Err(_) => false,
                },
                ChannelEvent::Open => true,
                ChannelEvent::Closed => false,
            };
            if !keep {
                break;
            }
        }
        self.finish(&swarm);
    }

    fn finish(&self, swarm: &Weak<Inner>) {
        // A link settled away keeps its connection, and its place among the
        // links, for the grace: its timer lets both go.
        if self.settled_away() {
            self.stop();
            return;
        }
        self.close();
        if let Some(inner) = swarm.upgrade() {
            inner.remove_link(self.id);
        }
    }

    /// Handles one message; returns whether to keep the link.
    fn handle(self: &Arc<Self>, message: Message, swarm: &Weak<Inner>) -> bool {
        match message {
            // Only before the link opens: its peer's name is fixed by then.
            Message::Hello { .. } => true,
            Message::Have { addresses } => {
                let mut state = self.state();
                for address in addresses {
                    if state.has.insert(address) {
                        state.has_order.push_back(address);
                    }
                }
                while state.has_order.len() > MAX_PEER_HAS {
                    if let Some(oldest) = state.has_order.pop_front() {
                        state.has.remove(&oldest);
                    }
                }
                true
            }
            Message::Get { id, address } => {
                self.serve(id, address, swarm);
                true
            }
            Message::Piece {
                id,
                total,
                offset,
                data,
            } => self.piece(id, total, offset, &data),
            Message::Missing { id } => {
                let mut state = self.state();
                if let Some(get) = state.gets.remove(&id) {
                    // Out of the queue too, so a later `Have` of it is not
                    // undone by the stale entry.
                    if state.has.remove(&get.address) {
                        state.has_order.retain(|address| *address != get.address);
                    }
                    let _ = get.events.unbounded_send(GetEvent::Missing);
                }
                true
            }
            Message::Busy { id } => {
                if let Some(get) = self.state().gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Busy);
                }
                true
            }
            Message::Cancel { id } => {
                self.state().serving.remove(&id);
                true
            }
            // Its proof checked out before the link opened.
            Message::Proof { .. } => true,
        }
    }

    /// Adds a piece of a record this client asked for. A piece out of order
    /// or of the wrong size ends the link; one for an ask this client gave up
    /// on is ignored.
    fn piece(&self, id: u32, total: u32, offset: u32, data: &[u8]) -> bool {
        let mut state = self.state();
        let Some(get) = state.gets.get_mut(&id) else {
            return true;
        };
        if get.assembly.is_none() {
            get.assembly = Assembly::new(total);
        }
        let added = match get.assembly.as_mut() {
            Some(assembly) => assembly.add(total, offset, data),
            None => Err(wire::WireError::PieceOutOfBounds),
        };
        match added {
            Ok(None) => {
                let received = offset.saturating_add(u32::try_from(data.len()).unwrap_or(u32::MAX));
                let _ = get
                    .events
                    .unbounded_send(GetEvent::Progress { total, received });
                true
            }
            Ok(Some(record)) => {
                if let Some(get) = state.gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Done(record));
                }
                true
            }
            Err(_) => {
                if let Some(get) = state.gets.remove(&id) {
                    let _ = get.events.unbounded_send(GetEvent::Failed);
                }
                false
            }
        }
    }

    /// Answers the peer's ask for a record: its pieces, or why not.
    fn serve(self: &Arc<Self>, id: u32, address: XorName, swarm: &Weak<Inner>) {
        let Some(inner) = swarm.upgrade() else {
            return;
        };
        let record = if inner.room_holds(&self.room, &address) {
            inner.store.get(&address)
        } else {
            None
        };
        let Some(record) = record else {
            self.queue(Message::Missing { id });
            return;
        };
        let number = {
            let mut state = self.state();
            if state.transfers >= MAX_SENDING_PER_LINK || state.serving.contains_key(&id) {
                None
            } else {
                let number = state.next_transfer;
                state.next_transfer = state.next_transfer.wrapping_add(1);
                state.serving.insert(id, number);
                state.transfers += 1;
                Some(number)
            }
        };
        let Some(number) = number else {
            self.queue(Message::Busy { id });
            return;
        };
        // Gives the place back however the transfer ends, even unstarted.
        let transfer = Transfer {
            link: Arc::clone(self),
            id,
            number,
        };
        // One record at a time while this client's own reads run: its
        // download needs the CPU and line a record sent takes.
        let Some(permit) = inner.start_upload() else {
            drop(transfer);
            self.queue(Message::Busy { id });
            return;
        };
        let counters = Arc::clone(&inner.counters);
        let room_counters = Arc::clone(&self.room_counters);
        drop(inner);
        let link = Arc::clone(self);
        spawn(async move {
            let _permit = permit;
            let transfer = transfer;
            // A peer that stops reading would hold the send, and its upload
            // slot, for ever: past the deadline the link goes.
            let sending = async {
                for (sent, piece) in wire::pieces(id, &record).enumerate() {
                    // Turns go in order only among records already waiting
                    // for one: every few pieces, this record lets the others,
                    // and a browser's newly arrived asks, have theirs.
                    if sent > 0 && sent % PIECES_PER_TURN == 0 {
                        crate::runtime::yield_now().await;
                    }
                    // Looked at last thing before each piece. An asker never
                    // asks again under an id it cancelled (its ids only
                    // grow), so a piece of a cancelled transfer already on
                    // its way can confuse only an asker that did.
                    if !transfer.wanted() {
                        return false;
                    }
                    if link.send(piece.encode()).await.is_err() {
                        link.close();
                        return false;
                    }
                }
                true
            };
            let sent = match crate::runtime::timeout(SEND_DEADLINE, sending).await {
                Ok(sent) => sent,
                Err(_) => {
                    link.close();
                    false
                }
            };
            drop(transfer);
            if sent {
                counters.served(record.len());
                room_counters.served(record.len());
            }
        });
    }

    /// Sends one message in its turn.
    async fn send(&self, message: Vec<u8>) -> Result<(), super::rtc::RtcError> {
        let _turn = self.sending.lock().await;
        self.sender.send(message).await
    }

    /// Sends queued messages until the link closes, gathering the addresses
    /// waiting to be announced into as few `Have` messages as fit.
    async fn write(self: Arc<Self>) {
        loop {
            let notified = self.wake.notified();
            let (control, haves) = {
                let mut outbox = self.outbox.lock().unwrap_or_else(PoisonError::into_inner);
                (
                    std::mem::take(&mut outbox.control),
                    std::mem::take(&mut outbox.haves),
                )
            };
            if self.closed.is_stopped() {
                return;
            }
            if control.is_empty() && haves.is_empty() {
                if or_stopped(&self.closed, notified).await.is_none() {
                    return;
                }
                continue;
            }
            let haves = haves
                .chunks(MAX_HAVE_ADDRESSES)
                .map(|addresses| Message::Have {
                    addresses: addresses.to_vec(),
                });
            for message in control.into_iter().chain(haves) {
                // A peer that stops reading would hold the writer for ever.
                let sent =
                    crate::runtime::timeout(SEND_DEADLINE, self.send(message.encode())).await;
                if !matches!(sent, Ok(Ok(()))) {
                    self.close();
                    return;
                }
            }
        }
    }
}

/// A read waiting for a turn on a link, counted while it waits.
struct Waiting<'a>(&'a AtomicUsize);

impl<'a> Waiting<'a> {
    /// A place among those waiting, if there is one left.
    fn enter(count: &'a AtomicUsize) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |waiting| {
                (waiting < MAX_WAITING_PER_LINK).then_some(waiting + 1)
            })
            .ok()
            .map(|_| Self(count))
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// The proof a link's offerer (`offerer`) or answerer sends: that it knows
/// the room's `secret`, for this connection alone. The secret comes from the
/// file's root map, which a node that only knows the room's key does not
/// have (for a private file); `binding` comes from both ends' DTLS
/// fingerprints, so a node that put itself between two clients, with an
/// offer and an answer of its own, cannot pass one client's proof on to the
/// other.
pub(super) fn proof(
    secret: &[u8; 32],
    binding: &[u8; 32],
    offer_id: &OfferId,
    offerer: bool,
) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new_keyed(secret);
    hasher.update(b"ant-swarm link proof v1");
    hasher.update(&[u8::from(offerer)]);
    hasher.update(binding);
    hasher.update(offer_id);
    hasher.finalize()
}

/// What a link's offerer (`offerer`) or answerer signs with its key in the
/// room: this connection (`binding`, see [`binding`]) and offer, and its side
/// of it, so a signature made for one link proves nothing on another, and a
/// member cannot name itself by another member's key.
pub(super) fn signed(binding: &[u8; 32], offer_id: &OfferId, offerer: bool) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("ant-swarm link signature v1");
    hasher.update(&[u8::from(offerer)]);
    hasher.update(binding);
    hasher.update(offer_id);
    *hasher.finalize().as_bytes()
}

/// What ties a link's proofs to its connection: the DTLS fingerprints in the
/// offer and the answer, as both ends see them. Each end's WebRTC stack
/// checks the other's DTLS certificate against these.
/// What ties a link to its connection: a hash of both ends' DTLS
/// fingerprints, which both ends' proofs cover, and this end's own
/// fingerprint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Binding {
    pub(super) proof: [u8; 32],
    pub(super) fingerprint: String,
}

impl Binding {
    /// The binding of the connection an offer and its answer make, on the
    /// end that made the offer if `offered`.
    pub(super) fn of(offer_sdp: &str, answer_sdp: &str, offered: bool) -> Self {
        let own = if offered { offer_sdp } else { answer_sdp };
        Self {
            proof: binding(offer_sdp, answer_sdp),
            fingerprint: fingerprint(own),
        }
    }
}

/// The first DTLS fingerprint `sdp` names, as `sha-256 AB:CD:...`.
pub(super) fn fingerprint(sdp: &str) -> String {
    sdp.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("a=fingerprint:"))
        .map(|fingerprint| fingerprint.trim().to_string())
        .unwrap_or_default()
}

pub(super) fn binding(offer_sdp: &str, answer_sdp: &str) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new_derive_key("ant-swarm link binding v1");
    for (side, sdp) in [(b'o', offer_sdp), (b'a', answer_sdp)] {
        hasher.update(&[side]);
        for fingerprint in sdp
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("a=fingerprint:"))
        {
            hasher.update(fingerprint.as_bytes());
            hasher.update(b"\n");
        }
    }
    *hasher.finalize().as_bytes()
}

/// A record being sent to the peer: its place among the link's transfers,
/// given back however the transfer ends.
struct Transfer {
    link: Arc<Link>,
    id: u32,
    number: u64,
}

impl Transfer {
    /// Whether the peer still wants this transfer: not cancelled, nor asked
    /// again under the same id, and the link is open.
    fn wanted(&self) -> bool {
        self.link.state().serving.get(&self.id) == Some(&self.number) && !self.link.is_closed()
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        let mut state = self.link.state();
        state.transfers = state.transfers.saturating_sub(1);
        if state.serving.get(&self.id) == Some(&self.number) {
            state.serving.remove(&self.id);
        }
    }
}

/// Forgets an ask when its waiter stops waiting, and tells the peer.
struct GetGuard<'a> {
    link: &'a Link,
    id: u32,
}

impl Drop for GetGuard<'_> {
    fn drop(&mut self) {
        let removed = self.link.state().gets.remove(&self.id).is_some();
        if removed && !self.link.is_closed() {
            self.link.queue(Message::Cancel { id: self.id });
        }
    }
}
