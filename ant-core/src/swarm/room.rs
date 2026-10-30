//! A client's membership of one room, kept at the rendezvous nodes: it
//! announces itself, hands over offers, answers the offers other members
//! make, and turns answers into links.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use futures::future::{join_all, select};

use ant_protocol::pqc::api::{
    ml_dsa_65, MlDsaPublicKey, MlDsaSecretKey, MlDsaSignature, MlDsaVariant,
};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::MAX_SWARM_WAIT_MS;
use ant_protocol::{
    canonical_ip, constrain_swarm_sdp, is_global_ip, ChunkMessage, ChunkMessageBody, MemberId,
    Membership, OfferId, RoomKey, SwarmAnnounceRequest, SwarmAnnounceResponse, SwarmAnswerRequest,
    SwarmAnswerResponse, SwarmLeaveRequest, SwarmOffer, SwarmRefusal, SwarmSignal,
    MAX_SWARM_OFFERS, MAX_SWARM_SDP_BYTES, SWARM_RENDEZVOUS_NODES, SWARM_SIGNAL_TTL_MS,
};
use tokio::sync::OwnedSemaphorePermit;
use web_time::Instant;

use super::link::{self, CONNECT_TIMEOUT};
use super::rtc::{channel_label, PendingOffer, RtcError};
use super::{
    or_stopped, spawn, update_usize, Connecting, Counters, DiscoveryCounters, Inner, PeerRef, Stop,
};
use crate::data::error::Error;
use crate::data::network::{send_and_await_chunk_response, ChunkRequest, RequestDispatched};
use crate::data::Network;
use crate::data::XorName;

/// How long one lookup of a room's rendezvous nodes serves its places: they
/// start together, and a place whose node let it go looks for the same
/// nodes soon after.
const LOOKUP_REUSE: Duration = Duration::from_secs(30);

/// Nodes a room's lookup names, closest first: its rendezvous nodes are the
/// closest of them that host rooms, so readers still meet while some of the
/// closest nodes predate swarm rooms.
const RENDEZVOUS_CANDIDATES: usize = 4 * SWARM_RENDEZVOUS_NODES;

/// How long a room passes over a node that would not let this client join:
/// one known not to host rooms, or one whose membership ended with no join.
const PASS_OVER: Duration = RENDEZVOUS_LIFETIME;

/// Nodes a room passes over at most: as many as its lookups can name within
/// [`PASS_OVER`], one lookup per [`LOOKUP_REUSE`] at most.
const MAX_PASSED_OVER: usize =
    (PASS_OVER.as_secs() / LOOKUP_REUSE.as_secs()) as usize * RENDEZVOUS_CANDIDATES;

/// Offers one announce carries at most.
const OFFERS_PER_ANNOUNCE: usize = 2;
const _: () = assert!(OFFERS_PER_ANNOUNCE <= MAX_SWARM_OFFERS);
/// Time added to an announce's wait for the request and reply themselves.
const REPLY_MARGIN: Duration = Duration::from_secs(10);
/// How long an answer or a leave may take.
const SHORT_REQUEST: Duration = Duration::from_secs(10);
/// How long a node keeps an offer it delivered waiting for its answer.
const SIGNAL_TTL: Duration = Duration::from_millis(SWARM_SIGNAL_TTL_MS);
/// The longest one local WebRTC step takes: making an offer or an answer
/// (gathering candidates, within `GATHER_TIMEOUT`, after a browser page's
/// turn to gather), or applying an answer.
const SDP_STEP: Duration = Duration::from_secs(8);
/// The longest a member goes between announces: a browser waits at most
/// [`BROWSER_POLL_IDLE`] and a native client none, and either first makes
/// the round's offers, within one [`SDP_STEP`] in all.
const MAX_POLL_GAP: Duration = Duration::from_secs(3 + SDP_STEP.as_secs());
/// The longest any client's announce takes, waiting included: a native one,
/// which may wait for a signal.
const LONGEST_ANNOUNCE: Duration =
    Duration::from_secs(MAX_SWARM_WAIT_MS as u64 / 1000 + REPLY_MARGIN.as_secs());
/// The longest this client's own announce takes.
const OWN_ANNOUNCE: Duration =
    Duration::from_secs(ANNOUNCE_WAIT_MS as u64 / 1000 + REPLY_MARGIN.as_secs());
/// The slowest any client answers an offer a node delivered: it sees the
/// offer at an announce that may already be on its way, makes its answer,
/// and sends it. Within the node's wait for it, or the answer would be
/// thrown away.
const SLOWEST_ANSWER: Duration = Duration::from_secs(
    MAX_POLL_GAP.as_secs()
        + LONGEST_ANNOUNCE.as_secs()
        + SDP_STEP.as_secs()
        + SHORT_REQUEST.as_secs(),
);
const _: () = assert!(SLOWEST_ANSWER.as_secs() < SIGNAL_TTL.as_secs());
/// How long a link this client answered has to open and be proven: its answer
/// goes to the node, the offerer collects it at an announce of its own,
/// applies it, and only then can the connection open.
pub(super) const ANSWERED_OPEN_TIMEOUT: Duration = Duration::from_secs(
    SHORT_REQUEST.as_secs()
        + MAX_POLL_GAP.as_secs()
        + LONGEST_ANNOUNCE.as_secs()
        + SDP_STEP.as_secs()
        + CONNECT_TIMEOUT.as_secs(),
);
/// How long an offer waits for its answer: the announce that carries it to
/// the node (which may arrive late in its time), the node's wait for the
/// answer, and the announce that brings the answer back.
const OFFER_TTL: Duration = Duration::from_secs(
    OWN_ANNOUNCE.as_secs() + SIGNAL_TTL.as_secs() + MAX_POLL_GAP.as_secs() + OWN_ANNOUNCE.as_secs(),
);
/// How long a room waits for its read's first record before it starts
/// finding its file's other readers anyway (see [`Room::until_started`]).
const FIRST_RECORD_WAIT: Duration = Duration::from_secs(30);
/// Failures in a row after which the rendezvous nodes are looked up again.
const MAX_FAILURES: u32 = 3;
/// How long the same rendezvous nodes are used before they are looked up
/// again, so a room follows the network as nodes come and go.
const RENDEZVOUS_LIFETIME: Duration = Duration::from_secs(10 * 60);
/// The longest pause after failures.
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// STUN servers kept from each rendezvous node.
const ICE_SERVERS_PER_NODE: usize = 2;
/// Record addresses kept for one room's file: a 1 TiB file of 4 MiB records.
/// A file with more records has no room (see [`super::room_of`]).
pub(super) const MAX_ROOM_RECORDS: usize = 262_144;

/// How long an announce may wait at the node for a signal. A native client
/// waits; a browser's waiting request would hold one of its session's few
/// request slots at the node, so a browser does not wait and asks again soon.
#[cfg(feature = "native")]
const ANNOUNCE_WAIT_MS: u32 = MAX_SWARM_WAIT_MS;
#[cfg(not(feature = "native"))]
const ANNOUNCE_WAIT_MS: u32 = 0;
/// How long a browser waits between announces while signals are on their way
/// (offers out, or signals just received), and otherwise.
#[cfg(not(feature = "native"))]
const BROWSER_POLL_BUSY: Duration = Duration::from_secs(1);
#[cfg(not(feature = "native"))]
const BROWSER_POLL_IDLE: Duration = Duration::from_secs(3);
/// How long after making an offer a browser keeps the busy pace for its
/// answer: a member that takes an offer up answers within a few announces.
/// Past that the offer waits at the idle pace, so the members of a room
/// larger than its links can hold, whose offers mostly go unanswered, do not
/// all ask their nodes every second.
#[cfg(not(feature = "native"))]
const BROWSER_BUSY_AFTER_OFFER: Duration = Duration::from_secs(10);
#[cfg(not(feature = "native"))]
const _: () = assert!(BROWSER_POLL_IDLE.as_secs() + SDP_STEP.as_secs() <= MAX_POLL_GAP.as_secs());
/// How long a native client waits before announcing again after a reply
/// with nothing in it came back well before the wait it asked for: the node
/// had no place for it to wait (or would not hold it), and asking again at
/// once would make a loop.
#[cfg(feature = "native")]
const NATIVE_EARLY_PAUSE: Duration = Duration::from_secs(2);
/// The shortest time between the starts of two native announces at one node,
/// whatever the replies hold: a node that answers at once with signals,
/// well formed or not, is asked at most once a second, as a busy browser
/// asks.
#[cfg(feature = "native")]
const NATIVE_ANNOUNCE_GAP: Duration = Duration::from_secs(1);
/// At most this many answers to other members' offers start in a room
/// within [`ANSWER_WINDOW`]: offers come through the nodes, which name their
/// senders, so a node handing out offers of members it makes up costs this
/// client a bounded number of negotiations, however fast it replies.
pub(super) const ANSWERS_PER_WINDOW: usize = 8;
const ANSWER_WINDOW: Duration = Duration::from_secs(20);

type Node = (PeerId, Vec<MultiAddr>);

/// A room's lookup of its rendezvous nodes: when it ended, and what it found
/// or why it failed.
type Lookup = (Instant, Result<Vec<Node>, String>);

/// What a member signs its link proofs under.
const LINK_SIGNING_CONTEXT: &[u8] = b"ant-swarm link v1";

/// This client's key for one membership of a room: its links' proofs are
/// signed with it, and its name in the room comes from it, so a member
/// cannot pass itself off as another (the room's secret, which every member
/// knows, proves nothing about which member a link reaches). A key of its
/// own for each membership, so peers meeting it in two rooms cannot tell it
/// is the same client by its key or name (its addresses, in its WebRTC
/// candidates, may still let them).
pub(super) struct Identity {
    public: Vec<u8>,
    secret: MlDsaSecretKey,
}

impl Identity {
    /// A new key, or `None` if one cannot be made.
    pub(super) fn generate() -> Option<Self> {
        let (public, secret) = ml_dsa_65().generate_keypair().ok()?;
        Some(Self {
            public: public.to_bytes(),
            secret,
        })
    }

    /// The key from `seed`: the same seed, the same member.
    #[cfg(test)]
    pub(super) fn from_seed(seed: &[u8; 32]) -> Option<Self> {
        // Infallible, unlike `generate`; an `Option` alike for its callers.
        let (public, secret) = ml_dsa_65().generate_keypair_from_seed(seed);
        Some(Self {
            public: public.to_bytes(),
            secret,
        })
    }

    /// The public key, as a `Hello` carries it.
    pub(super) fn public(&self) -> &[u8] {
        &self.public
    }

    /// Signs one link's `message` (see [`link::signed`]).
    pub(super) fn sign(&self, message: &[u8; 32]) -> Option<Vec<u8>> {
        ml_dsa_65()
            .sign_with_context(&self.secret, message, LINK_SIGNING_CONTEXT)
            .ok()
            .map(|signature| signature.to_bytes())
    }
}

/// Whether `signature` is `key`'s, over one link's `message`.
pub(super) fn signed_by(key: &MlDsaPublicKey, message: &[u8; 32], signature: &[u8]) -> bool {
    MlDsaSignature::from_bytes(MlDsaVariant::MlDsa65, signature)
        .ok()
        .and_then(|signature| {
            ml_dsa_65()
                .verify_with_context(key, message, &signature, LINK_SIGNING_CONTEXT)
                .ok()
        })
        .unwrap_or(false)
}

/// A member's key as a `Hello` carries it, if it is one.
pub(super) fn member_key(bytes: &[u8]) -> Option<MlDsaPublicKey> {
    MlDsaPublicKey::from_bytes(MlDsaVariant::MlDsa65, bytes).ok()
}

/// A member's name in a room, from its key.
pub(super) fn name_of(key: &[u8]) -> [u8; 16] {
    let hash = blake3::derive_key("ant-swarm member name v1", key);
    hash.first_chunk::<16>().copied().unwrap_or_default()
}

/// The tag of the room whose readers derive `secret`: 16 hex digits that name
/// the room without saying anything of its secret or its file.
pub(crate) fn room_tag(secret: &[u8; 32]) -> String {
    let hash = blake3::derive_key("ant-swarm room tag v1", secret);
    hex::encode(hash.first_chunk::<8>().copied().unwrap_or_default())
}

/// A room this client is in.
pub(super) struct Room {
    pub(super) key: RoomKey,
    /// This membership of the room, as the nodes know it: a newer one of the
    /// same room takes its place there, and its leave ends only this one.
    pub(super) membership: Membership,
    /// This client's key in the room, its own in each room and membership.
    pub(super) identity: Identity,
    /// Its name in the room, from that key.
    pub(super) nonce: [u8; 16],
    /// What a link proves it knows to join the room, from the file's map.
    pub(super) secret: [u8; 32],
    pub(super) stop: Stop,
    /// Left without telling its nodes: too many rooms left before it are
    /// still telling theirs.
    pub(super) quiet: AtomicBool,
    /// Its turn among the rooms telling their nodes they left, held until
    /// its tasks are done.
    leave_turn: Mutex<Option<OwnedSemaphorePermit>>,
    /// STUN servers by rendezvous node, closest first.
    ice_servers: Mutex<BTreeMap<usize, Vec<String>>>,
    /// The records of the room's file: the only ones shared in it.
    records: Mutex<HashSet<XorName>>,
    /// Connections being made in this room.
    pub(super) connecting: Arc<AtomicUsize>,
    /// Members whose offer this client is answering now.
    answering: Mutex<HashSet<PeerRef>>,
    /// When the room's latest answers started, at most
    /// [`ANSWERS_PER_WINDOW`] within [`ANSWER_WINDOW`].
    answers_started: Mutex<VecDeque<Instant>>,
    /// The node each rendezvous place uses now.
    places: Mutex<[Option<PeerId>; SWARM_RENDEZVOUS_NODES]>,
    /// The room's last lookup of its rendezvous nodes.
    lookup: tokio::sync::Mutex<Option<Lookup>>,
    /// Nodes this room passes over for now, and since when: at most
    /// [`MAX_PASSED_OVER`], the oldest forgotten first.
    passed_over: Mutex<Vec<(PeerId, Instant)>>,
    /// The room's tag, from its secret: its links' channels carry it.
    pub(super) tag: String,
    /// What this membership fetched from and served to the room's members.
    pub(super) counters: Arc<Counters>,
    /// When this client last read a record of the room's file, or joined it.
    read_at: Mutex<crate::runtime::Clock>,
    /// This client's reads of the room's file running now.
    reads: AtomicUsize,
    /// Whether the room may ask the nodes for anything yet (see
    /// [`Room::until_started`]), and the wake-up when it may.
    started: AtomicBool,
    start: tokio::sync::Notify,
}

impl Room {
    /// A new membership of room `key`, or `None` if no key could be made for
    /// it.
    pub(super) fn new(key: RoomKey, membership: Membership, secret: [u8; 32]) -> Option<Self> {
        let identity = Identity::generate()?;
        Some(Self {
            key,
            membership,
            nonce: name_of(identity.public()),
            identity,
            secret,
            stop: Stop::default(),
            quiet: AtomicBool::new(false),
            leave_turn: Mutex::default(),
            ice_servers: Mutex::default(),
            records: Mutex::default(),
            connecting: Arc::default(),
            answering: Mutex::default(),
            answers_started: Mutex::default(),
            places: Mutex::default(),
            lookup: tokio::sync::Mutex::default(),
            passed_over: Mutex::default(),
            tag: room_tag(&secret),
            counters: Arc::default(),
            read_at: Mutex::new(crate::runtime::Clock::now()),
            reads: AtomicUsize::new(0),
            started: AtomicBool::new(false),
            start: tokio::sync::Notify::new(),
        })
    }

    /// Lets the room start finding its file's other readers: a read's first
    /// record of the file is in, or the client asked to join.
    pub(super) fn start_finding(&self) {
        if !self.started.swap(true, Ordering::AcqRel) {
            self.start.notify_waiters();
        }
    }

    /// Waits until the room may start finding its file's other readers, or
    /// `longest` has passed. Its requests to the nodes (the lookup, the
    /// announces, the links' signals) so never compete with a download's
    /// first record for the nodes, the client's connections or its CPU; a
    /// room whose file nobody reads starts after `longest`.
    pub(super) async fn until_started(&self, longest: Duration) {
        if self.started.load(Ordering::Acquire) {
            return;
        }
        let mut notified = std::pin::pin!(self.start.notified());
        // Registered before looking again, so a start between the two is
        // not missed.
        notified.as_mut().enable();
        if self.started.load(Ordering::Acquire) {
            return;
        }
        let _ = select(notified, std::pin::pin!(crate::runtime::sleep(longest))).await;
    }

    /// Takes a place for an answer to start now, if fewer than
    /// [`ANSWERS_PER_WINDOW`] started in the room within [`ANSWER_WINDOW`].
    pub(super) fn may_answer(&self) -> bool {
        let now = Instant::now();
        let mut started = self
            .answers_started
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        while started
            .front()
            .is_some_and(|at| now.duration_since(*at) >= ANSWER_WINDOW)
        {
            started.pop_front();
        }
        if started.len() >= ANSWERS_PER_WINDOW {
            return false;
        }
        started.push_back(now);
        true
    }

    /// Notes that this client read a record of the room's file at `now`.
    pub(super) fn note_read(&self, now: crate::runtime::Clock) {
        *self.read_at.lock().unwrap_or_else(PoisonError::into_inner) = now;
    }

    /// How long no read of this client has used the room's file, at `now`:
    /// none while one runs.
    pub(super) fn unread_for(&self, now: crate::runtime::Clock) -> Duration {
        if self.reads.load(Ordering::Acquire) > 0 {
            return Duration::ZERO;
        }
        now.saturating_duration_since(*self.read_at.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Notes that a read of the room's file started.
    pub(super) fn begin_read(&self) {
        self.begin_reads(1);
    }

    /// Notes `count` reads of the room's file running, as a room joins.
    pub(super) fn begin_reads(&self, count: usize) {
        self.reads.fetch_add(count, Ordering::AcqRel);
    }

    /// Notes that a read of the room's file ended at `now`: the room goes
    /// unread from then.
    pub(super) fn end_read(&self, now: crate::runtime::Clock) {
        self.note_read(now);
        let _ = update_usize(&self.reads, |reads| Some(reads.saturating_sub(1)));
    }

    /// The label of the data channel on each of the room's links.
    pub(super) fn channel_label(&self) -> String {
        channel_label(&self.tag)
    }

    /// Takes `records` as a new room's list of its file's records, at once
    /// (the caller keeps it to [`MAX_ROOM_RECORDS`]).
    pub(super) fn hold_all(&self, records: HashSet<XorName>) {
        let mut held = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        if held.is_empty() {
            *held = records;
        } else {
            drop(held);
            let _ = self.hold(records);
        }
    }

    /// Adds records of the room's file, up to [`MAX_ROOM_RECORDS`].
    pub(super) fn hold(&self, addresses: impl IntoIterator<Item = XorName>) -> Vec<XorName> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let mut added = Vec::new();
        for address in addresses {
            if records.len() >= MAX_ROOM_RECORDS {
                break;
            }
            if records.insert(address) {
                added.push(address);
            }
        }
        added
    }

    /// Leaves the room telling its nodes if `turn` lets it, quietly if not.
    pub(super) fn leave_with(&self, turn: Option<OwnedSemaphorePermit>) {
        if turn.is_none() {
            self.quiet.store(true, Ordering::Release);
        }
        *self
            .leave_turn
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = turn;
    }

    /// Takes a node of `nodes`, closest first, for rendezvous place `index`:
    /// of the nodes not passed over and not known to host no rooms, the one
    /// at its own place if no other place uses it, else the first no other
    /// place uses. Readers seeing the same nodes so pick the same ones.
    /// `hosts` says whether a node hosts rooms, `None` if not known.
    fn claim_node(
        &self,
        index: usize,
        nodes: &[Node],
        hosts: impl Fn(&PeerId) -> Option<bool>,
    ) -> Option<Node> {
        let passed_over: Vec<PeerId> = {
            let mut passed = self
                .passed_over
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            passed.retain(|(_, since)| since.elapsed() < PASS_OVER);
            passed.iter().map(|(peer, _)| *peer).collect()
        };
        // A node a session says hosts no rooms is passed over as one that
        // never let this client join is, so it stays passed over once that
        // session has closed.
        for node in nodes.iter().filter(|node| hosts(&node.0) == Some(false)) {
            self.pass_over(node.0);
        }
        let nodes: Vec<&Node> = nodes
            .iter()
            .filter(|node| !passed_over.contains(&node.0))
            .filter(|node| hosts(&node.0) != Some(false))
            .collect();
        let mut places = self.places.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = |node: &Node| {
            places
                .iter()
                .enumerate()
                .any(|(place, used)| place != index && *used == Some(node.0))
        };
        let node = nodes
            .get(index)
            .copied()
            .filter(|node| !taken(node))
            .or_else(|| nodes.iter().copied().find(|node| !taken(node)))
            .cloned()?;
        if let Some(place) = places.get_mut(index) {
            *place = Some(node.0);
        }
        Some(node)
    }

    /// The room's rendezvous nodes, closest first, looked up for all its
    /// places at once: one lookup, found or failed, serves every place that
    /// asks within [`LOOKUP_REUSE`] of its end, and the places' lookups never
    /// run side by side.
    async fn closest_nodes(
        &self,
        swarm: &Weak<Inner>,
        network: &Network,
    ) -> Result<Vec<Node>, Error> {
        let mut last = self.lookup.lock().await;
        if let Some((at, found)) = last.as_ref() {
            if at.elapsed() < LOOKUP_REUSE {
                return found.clone().map_err(Error::Network);
            }
        }
        // Its connections take the page's turns to gather ICE where the
        // browser makes them take turns: they start after the reads' own.
        #[cfg(not(feature = "native"))]
        crate::browser::reads_not_waiting_to_gather().await;
        count(swarm, 1, |discovery| &discovery.lookups);
        let found = network
            .find_rendezvous_peers(&self.key, RENDEZVOUS_CANDIDATES)
            .await
            .map_err(|error| error.to_string());
        *last = Some((Instant::now(), found.clone()));
        found.map_err(Error::Network)
    }

    /// Passes over `peer` for [`PASS_OVER`].
    fn pass_over(&self, peer: PeerId) {
        let mut passed = self
            .passed_over
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        passed.retain(|(known, since)| *known != peer && since.elapsed() < PASS_OVER);
        if passed.len() >= MAX_PASSED_OVER {
            passed.remove(0);
        }
        passed.push((peer, Instant::now()));
    }

    fn release_node(&self, index: usize) {
        if let Some(place) = self
            .places
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(index)
        {
            *place = None;
        }
    }

    /// Lets go of the file's records, once the room is left.
    pub(super) fn forget_records(&self) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        *records = HashSet::new();
    }

    /// Whether `address` is a record of the room's file.
    pub(super) fn holds(&self, address: &XorName) -> bool {
        self.records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(address)
    }

    /// The STUN servers of every rendezvous node, for the room's links.
    fn ice_servers(&self) -> Vec<String> {
        let by_node = self
            .ice_servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut servers: Vec<String> = Vec::new();
        for server in by_node.values().flatten() {
            if !servers.contains(server) {
                servers.push(server.clone());
            }
        }
        servers
    }

    /// Keeps the STUN servers rendezvous node `index` named, but only those
    /// at an IP address this client `reached` it on. The addresses a node
    /// advertises are its own claim, so a node naming some other host's
    /// cannot send a client's STUN requests, and so its address, there.
    fn set_ice_servers(&self, index: usize, reached: &[IpAddr], servers: Vec<String>) {
        let reached: Vec<IpAddr> = reached.iter().map(IpAddr::to_canonical).collect();
        let kept: Vec<String> = servers
            .into_iter()
            .filter(|server| {
                server
                    .strip_prefix("stun:")
                    .and_then(|address| address.parse::<SocketAddr>().ok())
                    .is_some_and(|address| reached.contains(&address.ip().to_canonical()))
            })
            .take(ICE_SERVERS_PER_NODE)
            .collect();
        self.ice_servers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(index, kept);
    }
}

/// Starts keeping `room` at its rendezvous nodes until it is left or the
/// swarm stops. Returns `false` if there is no executor to run it on.
pub(super) fn start(inner: &Arc<Inner>, room: Arc<Room>, network: Network) -> bool {
    let swarm = Arc::downgrade(inner);
    let stop = Arc::clone(&inner.stop);
    spawn(async move {
        let slots = (0..SWARM_RENDEZVOUS_NODES).map(|index| Slot {
            swarm: &swarm,
            stop: &stop,
            room: &room,
            network: &network,
            index,
        });
        join_all(slots.map(Slot::run)).await;
        // Its leaves are done: another room left may tell its nodes.
        drop(
            room.leave_turn
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take(),
        );
    })
}

/// `step`, a local WebRTC step, or `None` if it fails, takes longer than
/// `limit`, or the room or the swarm stops first. A step dropped closes the
/// connection it was making.
async fn negotiate<T>(
    stop: &Stop,
    room: &Room,
    limit: Duration,
    step: impl Future<Output = Result<T, RtcError>>,
) -> Option<T> {
    let bounded = crate::runtime::timeout(limit, step);
    match or_stopped(stop, or_stopped(&room.stop, bounded))
        .await
        .flatten()
    {
        Some(Ok(Ok(value))) => Some(value),
        _ => None,
    }
}

/// One of a room's rendezvous places: the `index`-th closest node, looked up
/// again whenever its membership there ends, so a node that goes is
/// replaced while the others carry on.
struct Slot<'a> {
    swarm: &'a Weak<Inner>,
    stop: &'a Arc<Stop>,
    room: &'a Arc<Room>,
    network: &'a Network,
    index: usize,
}

impl Slot<'_> {
    async fn run(self) {
        // Nothing goes to the nodes before the read's first record is in.
        let started = self.room.until_started(FIRST_RECORD_WAIT);
        let _ = or_stopped(self.stop, or_stopped(&self.room.stop, started)).await;
        let mut failures = 0u32;
        while !self.stop.is_stopped() && !self.room.stop.is_stopped() {
            let lookup = self.room.closest_nodes(self.swarm, self.network);
            let nodes = or_stopped(self.stop, or_stopped(&self.room.stop, lookup))
                .await
                .flatten();
            let Some(Ok(nodes)) = nodes else {
                failures = failures.saturating_add(1);
                pause(self.stop, self.room, failures).await;
                continue;
            };
            // Its own node of the lookup's nodes that host rooms, or, if the
            // other place uses that one, the first the other place does not:
            // one node's inbox for the room is one membership's, and serves
            // one place.
            let Some(node) = self
                .room
                .claim_node(self.index, &nodes, |peer| self.network.hosts_swarm(peer))
            else {
                failures = failures.saturating_add(1);
                pause(self.stop, self.room, failures).await;
                continue;
            };
            let node = &node;
            let hosts_none = AtomicBool::new(false);
            let joined = Rendezvous {
                swarm: self.swarm,
                stop: self.stop,
                room: self.room,
                network: self.network,
                node,
                index: self.index,
                hosts_none: &hosts_none,
            }
            .run()
            .await;
            // The membership is over: the node's STUN servers go with it, so
            // no link is pointed at a node this client stopped trusting while
            // a replacement is looked up.
            self.room.set_ice_servers(self.index, &[], Vec::new());
            self.room.release_node(self.index);
            // A node that never let this client join is passed over, so the
            // place moves on to the next closest: at once from one known to
            // host no rooms (it predates them, or browsers cannot reach it),
            // after a pause from another.
            if joined {
                failures = 0;
            } else {
                self.room.pass_over(node.0);
                if self.network.hosts_swarm(&node.0) != Some(false)
                    && !hosts_none.load(Ordering::Relaxed)
                {
                    failures = failures.saturating_add(1);
                    pause(self.stop, self.room, failures).await;
                }
            }
        }
    }
}

/// Waits twice as long after each failure in a row, up to [`MAX_BACKOFF`], or
/// until the room or swarm stops.
async fn pause(stop: &Stop, room: &Room, failures: u32) {
    let delay = Duration::from_secs(5)
        .saturating_mul(1 << failures.min(6))
        .min(MAX_BACKOFF);
    let _ = or_stopped(stop, or_stopped(&room.stop, crate::runtime::sleep(delay))).await;
}

struct Offered {
    id: OfferId,
    offer: Box<dyn PendingOffer>,
    /// The offer's SDP, for the link's binding.
    sdp: String,
    made: Instant,
    /// Counts the offer until it becomes a link or is given up.
    connecting: Connecting,
}

/// The room's membership at one rendezvous node.
struct Rendezvous<'a> {
    swarm: &'a Weak<Inner>,
    stop: &'a Arc<Stop>,
    room: &'a Arc<Room>,
    network: &'a Network,
    node: &'a Node,
    /// The node's place among the rendezvous nodes, closest first.
    index: usize,
    /// Set once the node says it hosts no rooms (`SwarmRefusal::NotHosting`).
    hosts_none: &'a AtomicBool,
}

impl Rendezvous<'_> {
    fn stopped(&self) -> bool {
        self.stop.is_stopped() || self.room.stop.is_stopped()
    }

    /// Keeps the membership until the room or swarm stops, the node fails
    /// [`MAX_FAILURES`] times in a row, or [`RENDEZVOUS_LIFETIME`] passes.
    /// Returns whether the node let this client join at least once.
    async fn run(self) -> bool {
        let mut joined = false;
        // Whether an announce went out: one the node took may have had its
        // reply lost, so a membership asked for is left even if never seen.
        let mut asked = false;
        let mut pending: Vec<Offered> = Vec::new();
        let mut members = 0usize;
        // The member this client is at the node: a new one means the node
        // dropped the old (its session to this client closed), and with it
        // every offer made before.
        let mut member = None;
        let mut wait_ms = 0u32;
        let mut failures = 0u32;
        let started = Instant::now();
        loop {
            if self.stopped() {
                if asked && !self.room.quiet.load(Ordering::Acquire) {
                    self.leave().await;
                }
                return joined;
            }
            if failures >= MAX_FAILURES || started.elapsed() >= RENDEZVOUS_LIFETIME {
                return joined;
            }
            pending.retain(|offered| offered.made.elapsed() < OFFER_TTL);
            let fresh = self.make_offers(members, &mut pending).await;
            let wait = if fresh.is_empty() {
                wait_ms
            } else {
                ANNOUNCE_WAIT_MS
            };
            let offered = fresh.len();
            let request = SwarmAnnounceRequest {
                room: self.room.key,
                node: *self.node.0.as_bytes(),
                membership: self.room.membership,
                offers: fresh,
                wait_ms: wait,
            };
            let timeout = Duration::from_millis(u64::from(wait)) + REPLY_MARGIN;
            asked = true;
            count(self.swarm, 1, |discovery| &discovery.announces);
            count(self.swarm, offered, |discovery| &discovery.offers_made);
            #[cfg(feature = "native")]
            let asked_at = Instant::now();
            let reply = or_stopped(
                self.stop,
                or_stopped(
                    &self.room.stop,
                    // The whole of it, waiting for the transport included: a
                    // round that takes longer has failed, its offers with it.
                    async {
                        let announce = call(
                            self.swarm,
                            self.network,
                            self.node,
                            ChunkMessageBody::SwarmAnnounceRequest(request),
                            timeout,
                        );
                        crate::runtime::timeout(timeout, announce)
                            .await
                            .unwrap_or_else(|_| {
                                Err(Error::Timeout("swarm announce timed out".into()))
                            })
                    },
                ),
            )
            .await
            .flatten();
            let Some(reply) = reply else {
                continue;
            };
            // An offer goes once its time is up, before and after each
            // announce, so none outlives it by more than one announce.
            pending.retain(|offered| offered.made.elapsed() < OFFER_TTL);
            // A reply past the agreed limits, or counting more offers
            // delivered than this announce carried, is not acted on.
            let reply = reply.and_then(|body| match &body {
                ChunkMessageBody::SwarmAnnounceResponse(response) => {
                    response
                        .validate()
                        .map_err(|error| Error::Protocol(error.to_string()))?;
                    if let SwarmAnnounceResponse::Joined {
                        offers_delivered, ..
                    } = response
                    {
                        if usize::try_from(*offers_delivered)
                            .map_or(true, |delivered| delivered > offered)
                        {
                            return Err(Error::Protocol(
                                "a swarm reply counts more offers delivered than were sent".into(),
                            ));
                        }
                    }
                    Ok(body)
                }
                _ => Ok(body),
            });
            match reply {
                Ok(ChunkMessageBody::SwarmAnnounceResponse(SwarmAnnounceResponse::Joined {
                    member: now_member,
                    members: now,
                    offers_delivered,
                    inbox,
                    ice_servers,
                })) => {
                    joined = true;
                    failures = 0;
                    // Joined anew: the offers of earlier rounds reached a
                    // member the node no longer has, and stop holding their
                    // places in the room; this round's went to the new one.
                    if member.is_some_and(|known| known != now_member) {
                        let earlier = pending.len().saturating_sub(offered);
                        pending.drain(..earlier);
                    }
                    member = Some(now_member);
                    // The node handed on the first `offers_delivered` of this
                    // round's offers; the rest reached no one.
                    let delivered = usize::try_from(offers_delivered).unwrap_or(usize::MAX);
                    count(self.swarm, delivered, |discovery| {
                        &discovery.offers_delivered
                    });
                    let signalled = inbox.iter().map(signal_bytes).sum::<usize>()
                        + ice_servers.iter().map(String::len).sum::<usize>();
                    count(self.swarm, signalled, |discovery| &discovery.bytes_received);
                    pending.truncate(
                        pending
                            .len()
                            .saturating_sub(offered.saturating_sub(delivered)),
                    );
                    members = usize::try_from(now).unwrap_or(usize::MAX);
                    let reached = self.network.reached_ips(&self.node.0).await;
                    self.room.set_ice_servers(self.index, &reached, ice_servers);
                    let mut acted = false;
                    for signal in inbox {
                        acted |= self.signal(signal, &mut pending, &reached);
                    }
                    // Back well before the wait asked for, with nothing to act
                    // on (none, or none that matched): the node had no place
                    // for the wait, or would not hold it.
                    #[cfg(feature = "native")]
                    let early = !acted
                        && wait > 0
                        && asked_at.elapsed() < Duration::from_millis(u64::from(wait)) / 2;
                    // A browser asks again soon only while a signal is likely
                    // on its way: it just acted on one, or made an offer lately.
                    #[cfg(not(feature = "native"))]
                    let busy = acted
                        || pending
                            .iter()
                            .any(|offered| offered.made.elapsed() < BROWSER_BUSY_AFTER_OFFER);
                    wait_ms = ANNOUNCE_WAIT_MS;
                    #[cfg(not(feature = "native"))]
                    {
                        let poll = if busy {
                            BROWSER_POLL_BUSY
                        } else {
                            BROWSER_POLL_IDLE
                        };
                        let pause = or_stopped(&self.room.stop, crate::runtime::sleep(poll));
                        let _ = or_stopped(self.stop, pause).await;
                    }
                    #[cfg(feature = "native")]
                    {
                        let pause = if early {
                            NATIVE_EARLY_PAUSE
                        } else {
                            NATIVE_ANNOUNCE_GAP.saturating_sub(asked_at.elapsed())
                        };
                        if !pause.is_zero() {
                            let pause = crate::runtime::sleep(pause);
                            let _ = or_stopped(self.stop, or_stopped(&self.room.stop, pause)).await;
                        }
                    }
                }
                failed => {
                    failures += 1;
                    count(self.swarm, 1, |discovery| &discovery.announces_failed);
                    let not_hosting = matches!(
                        failed,
                        Ok(ChunkMessageBody::SwarmAnnounceResponse(
                            SwarmAnnounceResponse::Refused(SwarmRefusal::NotHosting)
                        ))
                    );
                    match failed {
                        Err(Error::Timeout(_)) => {
                            count(self.swarm, 1, |discovery| &discovery.announces_timed_out);
                        }
                        Ok(ChunkMessageBody::SwarmAnnounceResponse(
                            SwarmAnnounceResponse::Refused(_) | SwarmAnnounceResponse::Error(_),
                        )) => count(self.swarm, 1, |discovery| &discovery.announces_refused),
                        _ => {}
                    }
                    // This round's offers reached no one, and no offer waits
                    // through the pause, holding its place in the room.
                    pending.clear();
                    wait_ms = 0;
                    // A node that hosts no rooms never will: the place moves
                    // on now, as it does after the last failure in a row.
                    if not_hosting {
                        self.hosts_none.store(true, Ordering::Relaxed);
                    }
                    if not_hosting || self.network.hosts_swarm(&self.node.0) == Some(false) {
                        return joined;
                    }
                    if failures < MAX_FAILURES {
                        pause(self.stop, self.room, failures).await;
                    }
                }
            }
        }
    }

    /// Makes the offers this announce should carry, keeping each one until
    /// its answer comes or it expires: as many as this node's members this
    /// client has no link or connection being made to. Every rendezvous node
    /// takes offers, so two readers whose views share only one node still
    /// meet; the room's count of connections being made keeps the nodes from
    /// doubling them.
    async fn make_offers(&self, members: usize, pending: &mut Vec<Offered>) -> Vec<SwarmOffer> {
        let mut offers = Vec::new();
        // One step's time for the whole round, so the announce, and the
        // signals waiting for this member, are not held back further.
        let round = Instant::now() + SDP_STEP;
        while offers.len() < OFFERS_PER_ANNOUNCE {
            let Some(left) = round.checked_duration_since(Instant::now()) else {
                break;
            };
            // The swarm is held only to count, never across a wait, so a
            // client dropped meanwhile still shuts it down. Counted before it
            // is made, so no other task overshoots.
            let wanted = self.swarm.upgrade().and_then(|inner| {
                let rtc = inner.rtc.clone()?;
                inner
                    .reserve_connection(self.room, self.node.0.as_bytes(), members)
                    .map(|connecting| (rtc, connecting))
            });
            let Some((rtc, connecting)) = wanted else {
                break;
            };
            let made = negotiate(
                self.stop,
                self.room,
                left,
                rtc.offer(self.room.channel_label(), self.room.ice_servers()),
            )
            .await;
            drop(rtc);
            let Some((offer, sdp)) = made else {
                break;
            };
            if sdp.len() > MAX_SWARM_SDP_BYTES {
                break;
            }
            let id: OfferId = rand::random();
            pending.push(Offered {
                id,
                offer,
                sdp: sdp.clone(),
                made: Instant::now(),
                connecting,
            });
            offers.push(SwarmOffer { offer_id: id, sdp });
        }
        offers
    }

    /// Answers another member's offer, or opens the link an answer completes.
    /// Acts on one signal, which came through the node this client `reached`
    /// at those addresses; returns whether it led to anything.
    fn signal(&self, signal: SwarmSignal, pending: &mut Vec<Offered>, reached: &[IpAddr]) -> bool {
        match signal {
            SwarmSignal::Offer {
                from,
                offer_id,
                sdp,
            } => {
                count(self.swarm, 1, |discovery| &discovery.offers_received);
                // One answer at a time per member, and none to a member this
                // client already answered over a link: a member offering
                // again and again starts no more than one negotiation.
                let peer = (*self.node.0.as_bytes(), from);
                let Some(answering) = Answering::begin(self.room, peer) else {
                    return false;
                };
                // Counted from now, so one reply's offers start no more
                // connections than the client wants; and at most so many
                // answers at once, each until the node has taken it.
                let reserved = self.swarm.upgrade().and_then(|inner| {
                    if inner.has_link(self.room, &peer, false) {
                        return None;
                    }
                    let turn = Arc::clone(&inner.answers).try_acquire_owned().ok()?;
                    inner
                        .reserve_connection(self.room, self.node.0.as_bytes(), usize::MAX)
                        .map(|connecting| (connecting, turn))
                });
                let Some((connecting, turn)) = reserved else {
                    return false;
                };
                if !self.room.may_answer() {
                    return false;
                }
                spawn(answer(
                    connecting,
                    (answering, turn),
                    self.swarm.clone(),
                    Arc::clone(self.stop),
                    Arc::clone(self.room),
                    self.network.clone(),
                    self.node.clone(),
                    from,
                    offer_id,
                    applicable_sdp(&sdp, reached),
                ))
            }
            SwarmSignal::Answer {
                from,
                offer_id,
                sdp,
            } => {
                let Some(position) = pending.iter().position(|offered| offered.id == offer_id)
                else {
                    return false;
                };
                let Offered {
                    id,
                    offer,
                    sdp: offer_sdp,
                    connecting,
                    ..
                } = pending.swap_remove(position);
                count(self.swarm, 1, |discovery| &discovery.answers_received);
                // Bound to the answer as it is applied: its fingerprints are
                // the ones the connection then checks.
                let sdp = applicable_sdp(&sdp, reached);
                let binding = link::Binding::of(&offer_sdp, &sdp, true);
                let swarm = self.swarm.clone();
                let stop = Arc::clone(self.stop);
                let room = Arc::clone(self.room);
                let peer = Some((*self.node.0.as_bytes(), from));
                spawn(async move {
                    let Some(channel) = negotiate(&stop, &room, SDP_STEP, offer.accept(sdp)).await
                    else {
                        return;
                    };
                    if let Some(inner) = swarm.upgrade() {
                        inner.attach(&room, true, id, binding, peer, channel);
                    }
                    // The link counts itself from here.
                    drop(connecting);
                })
            }
        }
    }

    async fn leave(&self) {
        let request = ChunkMessageBody::SwarmLeaveRequest(SwarmLeaveRequest {
            room: self.room.key,
            node: *self.node.0.as_bytes(),
            membership: self.room.membership,
        });
        // `SHORT_REQUEST` in all, reaching the node included. It names this
        // membership, so however late it arrives it cannot end a newer one.
        let leave = call(self.swarm, self.network, self.node, request, SHORT_REQUEST);
        let _ = crate::runtime::timeout(SHORT_REQUEST, leave).await;
    }
}

/// An answer being made to a member's offer, one per member at a time.
struct Answering {
    room: Arc<Room>,
    peer: PeerRef,
}

impl Answering {
    /// The member's turn to be answered, unless one of its offers is being
    /// answered already.
    fn begin(room: &Arc<Room>, peer: PeerRef) -> Option<Self> {
        let started = room
            .answering
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(peer);
        started.then(|| Self {
            room: Arc::clone(room),
            peer,
        })
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.room
            .answering
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.peer);
    }
}

/// Answers `from`'s offer through `node`, and keeps the link if the answer is
/// passed on.
#[allow(clippy::too_many_arguments)]
async fn answer(
    connecting: Connecting,
    // Held until the node has taken the answer, or it failed.
    answering: (Answering, OwnedSemaphorePermit),
    swarm: Weak<Inner>,
    stop: Arc<Stop>,
    room: Arc<Room>,
    network: Network,
    node: Node,
    from: MemberId,
    offer_id: OfferId,
    offer: String,
) {
    let Some(rtc) = swarm.upgrade().and_then(|inner| inner.rtc.clone()) else {
        return;
    };
    let binding_offer = offer.clone();
    let made = negotiate(
        &stop,
        &room,
        SDP_STEP,
        rtc.answer(room.channel_label(), room.ice_servers(), offer),
    )
    .await;
    drop(rtc);
    let Some((channel, sdp)) = made else {
        return;
    };
    let binding = link::Binding::of(&binding_offer, &sdp, false);
    if sdp.len() > MAX_SWARM_SDP_BYTES {
        return;
    }
    let Some(link) = swarm.upgrade().and_then(|inner| {
        let peer = Some((*node.0.as_bytes(), from));
        inner.attach(&room, false, offer_id, binding, peer, channel)
    }) else {
        return;
    };
    // The link counts itself from here.
    drop(connecting);
    let request = ChunkMessageBody::SwarmAnswerRequest(SwarmAnswerRequest {
        room: room.key,
        node: *node.0.as_bytes(),
        to: from,
        offer_id,
        sdp,
    });
    // `SHORT_REQUEST` in all, reaching the node included.
    let sent = crate::runtime::timeout(
        SHORT_REQUEST,
        call(&swarm, &network, &node, request, SHORT_REQUEST),
    )
    .await;
    let delivered = matches!(
        sent,
        Ok(Ok(ChunkMessageBody::SwarmAnswerResponse(
            SwarmAnswerResponse::Delivered
        )))
    );
    if delivered {
        count(&swarm, 1, |discovery| &discovery.answers_sent);
    } else {
        link.close();
    }
    drop(answering);
}

/// Sends one swarm request to `node` and waits for its reply.
async fn call(
    swarm: &Weak<Inner>,
    network: &Network,
    node: &Node,
    body: ChunkMessageBody,
    timeout: Duration,
) -> Result<ChunkMessageBody, Error> {
    // A browser's membership lives on its session to the node: once that
    // closed (its pool let it go, say), the node dropped the membership, and
    // an answer or a leave sent on a new session reaches no member. Only an
    // announce, which joins anew, connects again, in the discovery turn as a
    // room's lookup does.
    let announce = matches!(body, ChunkMessageBody::SwarmAnnounceRequest(_));
    #[cfg(not(feature = "native"))]
    if !announce && network.hosts_swarm(&node.0).is_none() {
        return Err(Error::Network(
            "the session that held this membership has closed".into(),
        ));
    }
    let request_id: u64 = rand::random();
    let bytes = ChunkMessage { request_id, body }
        .encode()
        .map_err(|error| Error::Protocol(error.to_string()))?;
    count(swarm, bytes.len(), |discovery| &discovery.bytes_sent);
    if announce {
        connect_in_turn(network, node).await;
    }
    send_and_await_chunk_response(
        network,
        ChunkRequest {
            target_peer: &node.0,
            peer_addrs: &node.1,
            message_bytes: bytes,
            request_id,
            timeout,
            dispatched: RequestDispatched::unobserved(),
        },
        |body| Some(Ok(body)),
        Error::Network,
        || Error::Timeout("swarm request timed out".into()),
    )
    .await
    .and_then(|body| {
        // A reply past the agreed limits is not acted on.
        let checked = match &body {
            ChunkMessageBody::SwarmAnnounceResponse(response) => response.validate(),
            ChunkMessageBody::SwarmAnswerResponse(response) => response.validate(),
            ChunkMessageBody::SwarmLeaveResponse(response) => response.validate(),
            _ => Ok(()),
        };
        checked.map_err(|error| Error::Protocol(error.to_string()))?;
        Ok(body)
    })
}

/// Connects to `node` for a request if no open session has said hello: in the
/// swarm's discovery turn where connections take turns to gather ICE, taken
/// once no read's connection waits for one, and let go before the request
/// waits on the node, as a room's lookup does.
#[cfg(not(feature = "native"))]
async fn connect_in_turn(network: &Network, node: &Node) {
    if network.hosts_swarm(&node.0).is_some() {
        return;
    }
    let _turn = crate::browser::discovery_turn().await;
    crate::browser::reads_not_waiting_to_gather().await;
    network.connect_for_swarm(&node.0, &node.1).await;
}

/// A native client connects as it sends.
#[cfg(feature = "native")]
async fn connect_in_turn(_network: &Network, _node: &Node) {}

/// Counts `by` more of one step of discovery, if the swarm is still there.
pub(super) fn count(
    swarm: &Weak<Inner>,
    by: usize,
    counter: impl Fn(&DiscoveryCounters) -> &AtomicU64,
) {
    if let Some(inner) = swarm.upgrade() {
        DiscoveryCounters::add(counter(&inner.discovery), by);
    }
}

/// The session description a signal carries, in bytes.
fn signal_bytes(signal: &SwarmSignal) -> usize {
    match signal {
        SwarmSignal::Offer { sdp, .. } | SwarmSignal::Answer { sdp, .. } => sdp.len(),
    }
}

/// Another member's SDP, as this client applies it: the candidates a
/// receiver may try ([`constrain_swarm_sdp`]). Local addresses and mDNS
/// names only if the node that relayed it was itself `reached` at a local
/// address (a network of its own, such as a test network), loopback ones only
/// if over loopback (a network on one host): on a network reached at public
/// addresses they would let a stranger point this client at its own home
/// network. The node keeps public addresses to the sender's own; one that
/// does not is still held to the candidate limit. Only candidates change, so
/// the link's binding to both ends' fingerprints is the same either way.
fn applicable_sdp(sdp: &str, reached: &[IpAddr]) -> String {
    let loopback = reached.iter().any(|ip| canonical_ip(*ip).is_loopback());
    let local = reached.iter().any(|ip| !is_global_ip(*ip));
    constrain_swarm_sdp(sdp, |ip| match ip {
        Some(ip) if ip.is_loopback() => loopback,
        Some(ip) if is_global_ip(ip) => true,
        _ => local,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "native")]
    use std::io::Cursor;
    use std::net::Ipv4Addr;
    #[cfg(feature = "native")]
    use webrtc::ice::candidate::{candidate_base::unmarshal_candidate, Candidate};
    #[cfg(feature = "native")]
    use webrtc::sdp::SessionDescription;

    #[test]
    fn a_remote_sdp_points_at_loopback_only_on_a_network_on_one_host() {
        let sdp = "v=0\r\n\
                   a=candidate:1 1 udp 1 127.0.0.1 5000 typ host\r\n\
                   a=candidate:2 1 udp 1 ::1 5001 typ host\r\n\
                   a=candidate:3 1 udp 1 192.168.1.4 5002 typ host\r\n\
                   a=candidate:4 1 udp 1 8.8.4.4 5003 typ srflx raddr 0.0.0.0 rport 0\r\n\
                   a=candidate:5 1 udp 1 1f2e3d4c.local 5004 typ host\r\n\
                   a=candidate:6 1 udp 1 224.0.0.251 5353 typ host\r\n\
                   a=candidate:7 1 udp 1 198.18.0.1 5005 typ host\r\n\
                   a=candidate:8 1 udp 1 fec0::1 5006 typ host\r\n";
        let kept = |reached: &[IpAddr]| -> Vec<String> {
            applicable_sdp(sdp, reached)
                .lines()
                .filter_map(|line| line.strip_prefix("a=candidate:"))
                .filter_map(|candidate| candidate.split(' ').next())
                .map(str::to_string)
                .collect()
        };
        // Nodes on public addresses: public candidates only, whatever the
        // sender says, as when this client cannot tell how it reached them.
        assert_eq!(kept(&[IpAddr::from(Ipv4Addr::new(1, 1, 1, 5))]), ["4"]);
        assert_eq!(kept(&[]), ["4"]);
        // Nodes on a network of their own: its local addresses and names
        // too, benchmarking and IPv6 site-local addresses among them.
        assert_eq!(
            kept(&[IpAddr::from(Ipv4Addr::new(10, 77, 0, 2))]),
            ["3", "4", "5", "7", "8"]
        );
        // A node reached over loopback (as an IPv4-mapped address too): a
        // network on this host, where loopback is how its members meet.
        assert_eq!(
            kept(&[IpAddr::from(Ipv4Addr::LOCALHOST.to_ipv6_mapped())]),
            ["1", "2", "3", "4", "5", "7", "8"]
        );
    }

    /// What the native WebRTC stack reads from an SDP after this client
    /// applied it: every DTLS fingerprint, and every candidate's address as
    /// its ICE agent parses it.
    #[cfg(feature = "native")]
    fn as_native_stack_reads(sdp: &str) -> (Vec<String>, Vec<String>) {
        let parsed = SessionDescription::unmarshal(&mut Cursor::new(sdp))
            .unwrap_or_else(|error| panic!("the applied SDP parses: {error}"));
        let attributes = parsed.attributes.iter().chain(
            parsed
                .media_descriptions
                .iter()
                .flat_map(|media| media.attributes.iter()),
        );
        let (mut fingerprints, mut candidates) = (Vec::new(), Vec::new());
        for attribute in attributes {
            let value = attribute.value.clone().unwrap_or_default();
            match attribute.key.as_str() {
                "fingerprint" => fingerprints.push(value),
                "candidate" => {
                    let candidate = unmarshal_candidate(&value)
                        .unwrap_or_else(|error| panic!("{value}: {error}"));
                    // As the ICE agent ranks it, which a candidate the
                    // constraint let through must not make it panic over.
                    let _ = candidate.priority();
                    candidates.push(candidate.address());
                }
                _ => {}
            }
        }
        (fingerprints, candidates)
    }

    #[cfg(feature = "native")]
    #[test]
    fn a_relayed_sdp_cannot_slip_the_native_stack_another_fingerprint_or_candidate() {
        // A node relaying an offer adds its own fingerprint before the
        // sender's, and candidates pointing at this client's own host and
        // network, in spellings the native parser still reads.
        let sdp = "v=0\r\n\
                   o=- 1 2 IN IP4 127.0.0.1\r\n\
                   s=-\r\n\
                   t=0 0\r\n\
                   a= fingerprint:sha-256 EE:EE:EE\r\n\
                   m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\n\
                   c=IN IP4 0.0.0.0\r\n\
                   a=\tfingerprint:sha-256 EE:EE:EE\r\n\
                   a=fingerprint:sha-256 AB:CD:EF\r\n\
                   a= candidate:1 1 udp 1 127.0.0.1 5000 typ host\r\n\
                   \ra=candidate:2 1 udp 1 192.168.1.4 5001 typ host\r\n\
                   a=candidate:3 1 udp 1 8.8.4.4 5002 typ host\r\n\
                   a=candidate:4 1 udp 1 10.0.0.9 5003 typ host \r\n\
                   a=candidate:5 1 udp 1\u{a0}127.0.0.1\u{a0}4444\u{a0}typ\u{a0}host 8.8.4.4 5004 typ host\r\n\
                   a=candidate:6 257 udp 0 8.8.4.4 5005 typ host\r\n";
        let reached = [IpAddr::from(Ipv4Addr::new(1, 1, 1, 5))];
        let (fingerprints, candidates) = as_native_stack_reads(&applicable_sdp(sdp, &reached));
        assert_eq!(fingerprints, ["sha-256 AB:CD:EF"]);
        assert_eq!(candidates, ["8.8.4.4"]);
        // The link's binding covers exactly that fingerprint: a node that
        // adds one in the standard form changes the binding, and the
        // members' proofs no longer agree.
        let honest = "v=0\r\na=fingerprint:sha-256 AB:CD:EF\r\n";
        let added = "v=0\r\na=fingerprint:sha-256 EE:EE:EE\r\na=fingerprint:sha-256 AB:CD:EF\r\n";
        assert_eq!(
            link::binding(honest, honest),
            link::binding(&applicable_sdp(sdp, &reached), honest)
        );
        assert_ne!(link::binding(honest, honest), link::binding(added, honest));
    }

    #[test]
    fn an_offer_outlives_the_slowest_answer_and_its_way_back() {
        assert!(SLOWEST_ANSWER < SIGNAL_TTL);
        assert!(OFFER_TTL >= OWN_ANNOUNCE + SIGNAL_TTL + MAX_POLL_GAP + OWN_ANNOUNCE);
        assert!(LONGEST_ANNOUNCE >= OWN_ANNOUNCE);
        // A link answered here lasts until the slowest offerer can have
        // applied the answer and opened it.
        assert!(
            ANSWERED_OPEN_TIMEOUT
                >= SHORT_REQUEST + MAX_POLL_GAP + LONGEST_ANNOUNCE + SDP_STEP + CONNECT_TIMEOUT
        );
    }

    #[test]
    fn a_node_names_only_stun_servers_where_it_was_reached() {
        let room = Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room"));
        // Reached at 203.0.113.5, as an IPv4-mapped address, which is the
        // same host.
        let reached = [IpAddr::from(Ipv4Addr::new(203, 0, 113, 5).to_ipv6_mapped())];
        room.set_ice_servers(
            0,
            &reached,
            vec![
                "stun:203.0.113.5:10001".into(),
                "stun:198.51.100.9:3478".into(),
                "stun:192.168.1.1:53".into(),
                "turn:203.0.113.5:10001".into(),
                "stun:not-an-address".into(),
            ],
        );
        assert_eq!(
            room.ice_servers(),
            vec!["stun:203.0.113.5:10001".to_string()]
        );
        // A node's next reply replaces what it said before.
        room.set_ice_servers(0, &reached, Vec::new());
        assert!(room.ice_servers().is_empty());
        // Reached nowhere (no live connection to tell), nothing is kept.
        room.set_ice_servers(0, &[], vec!["stun:203.0.113.5:10001".into()]);
        assert!(room.ice_servers().is_empty());
    }

    #[test]
    fn a_member_is_answered_one_offer_at_a_time() {
        let room = Arc::new(Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room")));
        let member = ([4; 32], [5; 16]);
        let first = Answering::begin(&room, member);
        assert!(first.is_some());
        assert!(Answering::begin(&room, member).is_none());
        assert!(Answering::begin(&room, ([4; 32], [6; 16])).is_some());
        drop(first);
        assert!(Answering::begin(&room, member).is_some());
    }

    #[test]
    fn two_places_never_take_the_same_node() {
        let room = Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room"));
        let node = |n: u8| -> Node { (PeerId::from_bytes([n; 32]), Vec::new()) };
        let unknown = |_: &PeerId| None;
        let first = room.claim_node(0, &[node(1), node(2)], unknown);
        assert_eq!(first.map(|node| node.0), Some(PeerId::from_bytes([1; 32])));
        // The other place's lookup now puts node 1 at its own place.
        let second = room.claim_node(1, &[node(3), node(1)], unknown);
        assert_eq!(second.map(|node| node.0), Some(PeerId::from_bytes([3; 32])));
        // With every node of a lookup taken, there is none.
        room.release_node(1);
        assert!(room.claim_node(1, &[node(1)], unknown).is_none());
    }

    #[test]
    fn places_meet_at_the_closest_nodes_that_host_rooms() {
        let node = |n: u8| -> Node { (PeerId::from_bytes([n; 32]), Vec::new()) };
        let closest = [node(1), node(2), node(3), node(4)];
        // Node 1 predates swarm rooms, as its hello says: two readers looking
        // at the same nodes both take nodes 2 and 3.
        let old = |peer: &PeerId| Some(*peer != PeerId::from_bytes([1; 32]));
        for _ in 0..2 {
            let room = Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room"));
            let places = [
                room.claim_node(0, &closest, old).map(|node| node.0),
                room.claim_node(1, &closest, old).map(|node| node.0),
            ];
            assert_eq!(
                places,
                [
                    Some(PeerId::from_bytes([2; 32])),
                    Some(PeerId::from_bytes([3; 32]))
                ]
            );
            // Once that session has closed, nothing says node 1 hosts no
            // rooms, and it is still passed over.
            room.release_node(0);
            assert_eq!(
                room.claim_node(0, &closest, |_| None).map(|node| node.0),
                Some(PeerId::from_bytes([2; 32]))
            );
        }
        // A node that never let this client join is passed over the same way,
        // whatever its hello said, until PASS_OVER has gone by.
        let room = Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room"));
        room.pass_over(PeerId::from_bytes([1; 32]));
        let unknown = |_: &PeerId| None;
        assert_eq!(
            room.claim_node(0, &closest, unknown).map(|node| node.0),
            Some(PeerId::from_bytes([2; 32]))
        );
    }

    #[tokio::test]
    async fn a_negotiation_step_ends_when_the_room_does() {
        let room = Arc::new(Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room")));
        let stop = Stop::default();
        let stuck = futures::future::pending::<Result<(), RtcError>>();
        let ending = {
            let room = Arc::clone(&room);
            tokio::spawn(async move { room.stop.stop() })
        };
        let step = tokio::time::timeout(
            Duration::from_secs(5),
            negotiate(&stop, &room, CONNECT_TIMEOUT, stuck),
        );
        assert_eq!(step.await.ok(), Some(None));
        let _ = ending.await;
    }

    #[test]
    fn a_room_left_without_a_turn_leaves_quietly() {
        let room = Room::new([1; 32], 1, [0; 32]).unwrap_or_else(|| panic!("a room"));
        room.leave_with(None);
        assert!(room.quiet.load(Ordering::Acquire));
        let turns = Arc::new(tokio::sync::Semaphore::new(1));
        let told = Room::new([2; 32], 2, [0; 32]).unwrap_or_else(|| panic!("a room"));
        told.leave_with(Arc::clone(&turns).try_acquire_owned().ok());
        assert!(!told.quiet.load(Ordering::Acquire));
        // The turn is held until the room's tasks give it back.
        assert_eq!(turns.available_permits(), 0);
    }
}
