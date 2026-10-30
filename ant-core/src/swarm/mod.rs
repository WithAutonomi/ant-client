//! Clients downloading the same file share its records, the way `BitTorrent`
//! peers share pieces, and the nodes are the last seed.
//!
//! Records are content addressed and every client checks each one against
//! its address, so a record from another client is as good as one from a
//! node. While a client downloads a file it is a member of the file's *room*
//! at the nodes closest to the room's key (ant-protocol's swarm messages):
//! those nodes introduce it to other members by carrying WebRTC offers and
//! answers, and its node STUN tells it the address other clients reach it
//! at. The clients then talk over WebRTC data channels: each says which
//! records it holds, and asks the others for records before asking nodes.
//!
//! - A record a peer sends is checked against its address; one that fails
//!   ends that link, and the record is fetched from nodes as before.
//! - A peer that is busy or gone costs at most a short wait
//!   ([`link::FETCH_STALL`]) before nodes are asked, and one read spends at
//!   most [`link::FETCH_TOTAL`] on peers in all. A peer slower than nodes
//!   would likely be is passed over for a while.
//! - Only records fetched from nodes, or from peers and checked, are served,
//!   from a store of bounded size ([`STORE_BYTES`]), to a bounded number of
//!   peers at a time.
//! - A record from a peer never answers whether a record is *stored on the
//!   network*: existence checks ask nodes.
//!
//! Browser and native clients take part alike: a browser uses its own
//! `RTCPeerConnection`, a native client the webrtc-rs stack.

// Browser builds are single-threaded; `Arc` there is a plain shared pointer.
#![cfg_attr(not(feature = "native"), allow(clippy::arc_with_non_send_sync))]

#[cfg(all(
    not(feature = "native"),
    target_arch = "wasm32",
    feature = "browser-wasm"
))]
mod browser_rtc;
mod link;
#[cfg(feature = "native")]
mod native_rtc;
mod room;
pub(crate) mod rtc;
mod store;
mod wire;

use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
#[cfg(feature = "native")]
use std::sync::LazyLock;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::UNIX_EPOCH;

use ant_protocol::{MemberId, Membership, OfferId, RoomKey};
use bytes::Bytes;
use futures::future::{select, Either};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use tokio::sync::{Notify, Semaphore};

use crate::client_engine::files::MIN_FILE_CHUNKS;
use crate::data::{DataMap, Network, XorName};
use link::{GetOutcome, Link};
// Browser reads report their room's tag; native code has no page to tell.
#[cfg(any(test, not(feature = "native")))]
pub(crate) use room::room_tag;
use room::Room;
use rtc::{Channel, Rtc};
use self_encryption::DataMapIndex;
use store::RecordStore;

/// Bytes of records a client keeps to serve to other clients.
#[cfg(feature = "native")]
pub(crate) const STORE_BYTES: usize = 256 * 1024 * 1024;
/// Bytes of records a client keeps to serve to other clients.
#[cfg(not(feature = "native"))]
pub(crate) const STORE_BYTES: usize = 64 * 1024 * 1024;
/// Rooms a client is in at once; joining another leaves the oldest. Matches
/// the rooms a node lets one connection join.
const MAX_ROOMS: usize = 4;
/// How long a room must go unread before a join past [`MAX_ROOMS`] may leave
/// it. A room still read is never left for another, so a client reading more
/// files than that at once cannot make every read a new membership (a key, a
/// lookup, links that never open); a file it cannot join yet is read from the
/// nodes, and joined at a later read of it once a room has gone unread.
const ROOM_TURNOVER: std::time::Duration = std::time::Duration::from_secs(30);
/// Joins waiting for a room to make way; past this, the oldest is dropped.
const MAX_WAITING_JOINS: usize = 2;
/// Links a client keeps in one room.
const MAX_LINKS_PER_ROOM: usize = 8;
/// Links a client keeps in all.
const MAX_LINKS: usize = 16;
/// Records a client sends to other clients at once.
const MAX_UPLOADS: usize = 4;
/// Records a client sends at once while reads of its own are running: a
/// record sent costs the sender's CPU and line, which its own download needs
/// first. A client done reading sends [`MAX_UPLOADS`].
const UPLOADS_WHILE_READING: usize = 1;
/// Offers a client answers at once, each until the node has taken its answer.
const MAX_ANSWERS: usize = 8;
/// Records a client takes from other clients at once; reads past this go to
/// the nodes. With records grown as their pieces come, the records being
/// received hold at most this many records' bytes.
const MAX_RECEIVING: usize = 8;
/// Rooms left that may be telling their nodes so at once; one left past this
/// leaves without telling them (its places there expire on their own), so a
/// burst of reads of other files costs no more leaves.
const MAX_LEAVING: usize = 8;
/// Peers asked for one record before nodes are.
const FETCH_PEERS: usize = 2;
/// How often a read waiting for a settled link's twin looks again.
const TWIN_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// What a client's swarm has done since it started.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwarmStats {
    /// Records other clients sent this one, each checked.
    pub records_from_peers: u64,
    /// Their bytes.
    pub bytes_from_peers: u64,
    /// Records peers sent that failed the check, and were fetched from nodes.
    pub records_rejected: u64,
    /// Asks a peer answered it was too busy for.
    pub asks_busy: u64,
    /// Asks a peer answered it no longer held the record for.
    pub asks_missing: u64,
    /// Asks that failed: the link closed, or the record ran out of time.
    pub asks_failed: u64,
    /// Records this client sent to other clients.
    pub records_served: u64,
    /// Their bytes.
    pub bytes_served: u64,
    /// Links to other clients open now.
    pub links: u64,
    /// Rooms this client is in now.
    pub rooms: u64,
    /// What finding and linking other clients took.
    pub discovery: DiscoveryStats,
}

/// What a client's swarm has spent finding other clients and linking to
/// them, since it started: its requests to rendezvous nodes, and each step of
/// the connections they led to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveryStats {
    /// Lookups for a room's rendezvous nodes.
    pub lookups: u64,
    /// Announces sent to rendezvous nodes.
    pub announces: u64,
    /// Announces that failed, timed out or were refused.
    pub announces_failed: u64,
    /// Of those, announces that had no reply in time.
    pub announces_timed_out: u64,
    /// Of those, announces the node refused or found malformed.
    pub announces_refused: u64,
    /// Bytes of the swarm requests sent to rendezvous nodes.
    pub bytes_sent: u64,
    /// Bytes of the session descriptions and STUN server names in their
    /// replies.
    pub bytes_received: u64,
    /// Offers made for other members.
    pub offers_made: u64,
    /// Of those, offers a node handed on.
    pub offers_delivered: u64,
    /// Offers other members made this client.
    pub offers_received: u64,
    /// Answers to those a node took.
    pub answers_sent: u64,
    /// Answers to this client's offers.
    pub answers_received: u64,
    /// Data channels to other members that opened.
    pub channels_opened: u64,
    /// Links whose peer proved itself.
    pub links_proven: u64,
    /// Links given up before their peer proved itself.
    pub links_failed: u64,
}

/// The running counts behind [`DiscoveryStats`].
#[derive(Default)]
pub(crate) struct DiscoveryCounters {
    pub(crate) lookups: AtomicU64,
    pub(crate) announces: AtomicU64,
    pub(crate) announces_failed: AtomicU64,
    pub(crate) announces_timed_out: AtomicU64,
    pub(crate) announces_refused: AtomicU64,
    pub(crate) bytes_sent: AtomicU64,
    pub(crate) bytes_received: AtomicU64,
    pub(crate) offers_made: AtomicU64,
    pub(crate) offers_delivered: AtomicU64,
    pub(crate) offers_received: AtomicU64,
    pub(crate) answers_sent: AtomicU64,
    pub(crate) answers_received: AtomicU64,
    pub(crate) channels_opened: AtomicU64,
    pub(crate) links_proven: AtomicU64,
    pub(crate) links_failed: AtomicU64,
}

impl DiscoveryCounters {
    pub(crate) fn add(counter: &AtomicU64, by: usize) {
        counter.fetch_add(u64::try_from(by).unwrap_or(u64::MAX), Ordering::Relaxed);
    }

    fn snapshot(&self) -> DiscoveryStats {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        DiscoveryStats {
            lookups: load(&self.lookups),
            announces: load(&self.announces),
            announces_failed: load(&self.announces_failed),
            announces_timed_out: load(&self.announces_timed_out),
            announces_refused: load(&self.announces_refused),
            bytes_sent: load(&self.bytes_sent),
            bytes_received: load(&self.bytes_received),
            offers_made: load(&self.offers_made),
            offers_delivered: load(&self.offers_delivered),
            offers_received: load(&self.offers_received),
            answers_sent: load(&self.answers_sent),
            answers_received: load(&self.answers_received),
            channels_opened: load(&self.channels_opened),
            links_proven: load(&self.links_proven),
            links_failed: load(&self.links_failed),
        }
    }
}

/// How far the record a read is receiving from a peer has come: its bytes
/// so far and in all (none known before its first piece). A read gives a
/// peer that keeps on pace a little longer before it asks the nodes too.
#[derive(Default)]
pub(crate) struct Pace {
    received: AtomicU64,
    total: AtomicU64,
    /// What the read spent on peers, kept across its asks.
    spent: Mutex<Spent>,
}

/// What a read has spent on peers across every time it asks them: the
/// links it tried, the tries that count toward [`FETCH_PEERS`], and when its
/// time with peers ends, [`link::FETCH_TOTAL`] after it first asked one (an
/// ask that found no peer with the record spends nothing). A read
/// that asks again (a browser read whose node read still waits for its turn,
/// when a peer offers the record) so goes on from there rather than anew.
#[derive(Default)]
struct Spent {
    deadline: Option<web_time::Instant>,
    tries: usize,
    tried: Vec<u64>,
}

/// A read's [`Spent`], taken for one ask and given back however it ends.
struct Spending<'a> {
    from: &'a Mutex<Spent>,
    spent: Spent,
}

impl<'a> Spending<'a> {
    fn take(from: &'a Mutex<Spent>) -> Self {
        let spent = std::mem::take(&mut *from.lock().unwrap_or_else(PoisonError::into_inner));
        Self { from, spent }
    }
}

impl Drop for Spending<'_> {
    fn drop(&mut self) {
        *self.from.lock().unwrap_or_else(PoisonError::into_inner) = std::mem::take(&mut self.spent);
    }
}

impl Pace {
    /// `received` of `total` bytes have come; a new peer asked starts at 0.
    pub(crate) fn note(&self, received: u64, total: u64) {
        self.total.store(total, Ordering::Relaxed);
        self.received.store(received, Ordering::Relaxed);
    }

    /// Whether, at its average rate over the `elapsed` since the read asked,
    /// the whole record has come by `by` after the ask.
    pub(crate) fn finishes_by(
        &self,
        elapsed: std::time::Duration,
        by: std::time::Duration,
    ) -> bool {
        let received = u128::from(self.received.load(Ordering::Relaxed));
        let total = u128::from(self.total.load(Ordering::Relaxed));
        if received == 0 || total < received {
            return false;
        }
        elapsed.as_millis().saturating_mul(total) <= by.as_millis().saturating_mul(received)
    }
}

/// A record being sent to another client, counted until the send ends (see
/// [`Inner::start_upload`]).
pub(crate) struct Upload(Arc<AtomicUsize>);

impl Drop for Upload {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A read of this client's that other clients may serve, counted while it
/// runs (see [`Swarm::reading`]).
pub(crate) struct Reading {
    reading: Arc<AtomicUsize>,
    /// The swarm, whose rooms holding `address` count as read while the
    /// read runs, and the record read.
    inner: Weak<Inner>,
    address: XorName,
}

impl Drop for Reading {
    fn drop(&mut self) {
        self.reading.fetch_sub(1, Ordering::AcqRel);
        if let Some(inner) = self.inner.upgrade() {
            inner.end_read(&self.address);
        }
    }
}

/// What a swarm, or one of its rooms, fetched and served.
#[derive(Default)]
pub(crate) struct Counters {
    records_from_peers: AtomicU64,
    bytes_from_peers: AtomicU64,
    records_rejected: AtomicU64,
    asks_busy: AtomicU64,
    asks_missing: AtomicU64,
    asks_failed: AtomicU64,
    records_served: AtomicU64,
    bytes_served: AtomicU64,
}

impl Counters {
    fn served(&self, bytes: usize) {
        self.records_served.fetch_add(1, Ordering::Relaxed);
        self.bytes_served.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn fetched(&self, bytes: usize) {
        self.records_from_peers.fetch_add(1, Ordering::Relaxed);
        self.bytes_from_peers
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

/// What a client's membership of one room fetched from and served to the
/// room's other members, as a page reading the room's file shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoomStats {
    /// The room's tag: its links' data channels are labelled `ant-swarm-`
    /// and then this, and a browser read reports `Swarm room <tag>` when it
    /// resolves the file.
    pub tag: String,
    /// Records other members sent this one, each checked.
    pub records_from_peers: u64,
    /// Their bytes.
    pub bytes_from_peers: u64,
    /// Records members sent that failed the check.
    pub records_rejected: u64,
    /// Records this client sent to the room's members.
    pub records_served: u64,
    /// Their bytes.
    pub bytes_served: u64,
    /// The room's links open now.
    pub links: u64,
    /// This client's DTLS fingerprint on each of those links' connections
    /// (`sha-256 AB:CD:...`), as its own SDP named it: a link is open only
    /// once its peer's proof checks out, so a page can tell a proven link's
    /// connection from one that merely carries a readers' channel.
    pub open_links: Vec<String>,
}

/// A one-way stop signal tasks can wait on.
#[derive(Default)]
pub(crate) struct Stop {
    stopped: AtomicBool,
    notify: Notify,
}

impl Stop {
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    async fn wait(&self) {
        loop {
            // A `Notified` receives `notify_waiters` from the moment it is
            // made, polled or not, so a stop between here and the await is
            // not missed.
            let notified = self.notify.notified();
            if self.is_stopped() {
                return;
            }
            notified.await;
        }
    }
}

/// `future`'s output, or `None` if `stop` fires first.
pub(crate) async fn or_stopped<F: Future>(stop: &Stop, future: F) -> Option<F::Output> {
    let future = std::pin::pin!(future);
    let stopped = std::pin::pin!(stop.wait());
    match select(future, stopped).await {
        Either::Left((output, _)) => Some(output),
        Either::Right(_) => None,
    }
}

/// Changes `atomic` to what `change` makes of its value, in one step, unless
/// `change` gives `None`: the value before, or the one `change` refused. As
/// the atomics' own `fetch_update`, which Rust 1.99 renames.
fn update_usize(
    atomic: &AtomicUsize,
    change: impl Fn(usize) -> Option<usize>,
) -> Result<usize, usize> {
    let mut current = atomic.load(Ordering::Acquire);
    loop {
        let Some(new) = change(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(before) => return Ok(before),
            Err(now) => current = now,
        }
    }
}

/// As [`update_usize`], for an `AtomicU64`.
fn update_u64(atomic: &AtomicU64, change: impl Fn(u64) -> Option<u64>) -> Result<u64, u64> {
    let mut current = atomic.load(Ordering::Acquire);
    loop {
        let Some(new) = change(current) else {
            return Err(current);
        };
        match atomic.compare_exchange_weak(current, new, Ordering::AcqRel, Ordering::Acquire) {
            Ok(before) => return Ok(before),
            Err(now) => current = now,
        }
    }
}

/// Whether [`spawn`] has an executor to run a task on.
#[cfg(feature = "native")]
fn can_spawn() -> bool {
    tokio::runtime::Handle::try_current().is_ok()
}

#[cfg(not(feature = "native"))]
fn can_spawn() -> bool {
    cfg!(all(target_arch = "wasm32", feature = "browser-wasm"))
}

/// Runs a background task on the platform's executor; `false` if there is
/// none (a native caller outside a Tokio runtime).
#[cfg(feature = "native")]
pub(crate) fn spawn<F: Future<Output = ()> + Send + 'static>(task: F) -> bool {
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => {
            drop(runtime.spawn(task));
            true
        }
        Err(_) => false,
    }
}

/// Runs a background task on the platform's executor.
#[cfg(all(
    not(feature = "native"),
    target_arch = "wasm32",
    feature = "browser-wasm"
))]
pub(crate) fn spawn<F: Future<Output = ()> + 'static>(task: F) -> bool {
    wasm_bindgen_futures::spawn_local(task);
    true
}

/// A build with neither runtime has no WebRTC stack, so it never joins a room
/// and never starts a task.
#[cfg(all(
    not(feature = "native"),
    not(all(target_arch = "wasm32", feature = "browser-wasm"))
))]
pub(crate) fn spawn<F: Future<Output = ()> + 'static>(task: F) -> bool {
    drop(task);
    false
}

/// The room every client reading the file `data_map` describes meets in, and
/// the room's secret. The room is the address the map has when stored as a
/// public record, so a public file's room is its address. The secret comes
/// from the map itself: a node that knows only the room (for a private file,
/// the hash of a map it never saw) cannot prove a link into it.
///
/// Only a valid root map (not a child map, one whose records index, and as
/// many as a file has at least) has a room, so a bad map never takes a
/// room's place; and only one a room can hold every record of
/// ([`room::MAX_ROOM_RECORDS`], about 1 TiB), so a room shares its whole
/// file. A larger file is read from the nodes alone.
pub(crate) fn room_of(data_map: &DataMap) -> Option<(RoomKey, [u8; 32])> {
    let records = file_records(data_map)?;
    if records > room::MAX_ROOM_RECORDS {
        return None;
    }
    crate::client_engine::files::public_map_record(data_map)
        .ok()
        .map(|(address, record)| {
            let secret = blake3::derive_key("ant-swarm room secret v1", &record);
            (address, secret)
        })
}

/// The newest membership number this process gave a room.
static LAST_MEMBERSHIP: AtomicU64 = AtomicU64::new(0);

/// Which of `rooms`, all of them joined, a join past [`MAX_ROOMS`] leaves at
/// `now`: the one unread longest, if no read has used it for
/// [`ROOM_TURNOVER`].
fn room_to_leave(rooms: &[Arc<Room>], now: crate::runtime::Clock) -> Option<usize> {
    rooms
        .iter()
        .enumerate()
        .map(|(position, room)| (position, room.unread_for(now)))
        .filter(|(_, unread)| *unread >= ROOM_TURNOVER)
        .max_by_key(|(_, unread)| *unread)
        .map(|(position, _)| position)
}

/// A join that found [`MAX_ROOMS`] rooms all still read: made at a later
/// read of one of its file's records, once a room has gone unread.
struct WaitingJoin {
    key: RoomKey,
    secret: [u8; 32],
    records: HashSet<XorName>,
    network: Network,
}

/// A membership number newer than every one this process gave before; see
/// [`Inner::next_membership`].
fn next_membership() -> Membership {
    let now = crate::runtime::system_time()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_micros()).unwrap_or(u64::MAX)
        });
    let mut last = LAST_MEMBERSHIP.load(Ordering::Acquire);
    loop {
        let next = now.max(last.saturating_add(1));
        match LAST_MEMBERSHIP.compare_exchange_weak(last, next, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return next,
            Err(actual) => last = actual,
        }
    }
}

/// Another client, as a rendezvous node names it: the node's peer id and the
/// member id it gave the client.
pub(crate) type PeerRef = ([u8; 32], MemberId);

/// How many records the file `data_map` describes, if it is a valid root
/// map of a file: not a child map, one whose records index, with at least as
/// many as a file has.
pub(crate) fn file_records(data_map: &DataMap) -> Option<usize> {
    if data_map.is_child() {
        return None;
    }
    let records = DataMapIndex::new(data_map).ok()?.infos().len();
    (records >= MIN_FILE_CHUNKS).then_some(records)
}

/// A connection being made, an offer or an answer, counted in its room and in
/// all until it becomes a link or is given up.
pub(crate) struct Connecting {
    all: Arc<AtomicUsize>,
    room: Arc<AtomicUsize>,
}

impl Connecting {
    fn new(all: &Arc<AtomicUsize>, room: &Arc<AtomicUsize>) -> Self {
        all.fetch_add(1, Ordering::AcqRel);
        room.fetch_add(1, Ordering::AcqRel);
        Self {
            all: Arc::clone(all),
            room: Arc::clone(room),
        }
    }
}

impl Drop for Connecting {
    fn drop(&mut self) {
        self.all.fetch_sub(1, Ordering::AcqRel);
        self.room.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A client's part in the swarm. Clones share it.
#[derive(Clone)]
pub(crate) struct Swarm {
    inner: Arc<Inner>,
}

pub(crate) struct Inner {
    rtc: Option<Arc<dyn Rtc>>,
    store: RecordStore,
    links: Mutex<Vec<Arc<Link>>>,
    /// Oldest first.
    rooms: Mutex<Vec<Arc<Room>>>,
    /// Turns for rooms left to tell their nodes so.
    leaving: Arc<Semaphore>,
    /// Records being sent to other clients now.
    uploads: Arc<AtomicUsize>,
    /// This client's reads running now that other clients may serve.
    reading: Arc<AtomicUsize>,
    /// The records those reads are reading, and how many reads each: a room
    /// joined meanwhile counts as read by those of its file's records too.
    /// Taken while holding `rooms`.
    active_reads: Mutex<HashMap<XorName, usize>>,
    /// Turns to answer an offer, held until the answer is delivered.
    answers: Arc<Semaphore>,
    /// Turns to take a record from other clients.
    receiving: Arc<Semaphore>,
    /// Connections being made now, offers and answers, in every room.
    connecting: Arc<AtomicUsize>,
    next_link: AtomicU64,
    counters: Arc<Counters>,
    discovery: DiscoveryCounters,
    /// How long reads that went to the nodes took to have their records
    /// lately, in milliseconds: a running average, 0 before the first.
    node_read_ms: AtomicU64,
    /// Joins waiting for a room to make way, oldest first, at most
    /// [`MAX_WAITING_JOINS`].
    waiting_joins: Mutex<VecDeque<WaitingJoin>>,
    /// Whether any join waits, so a read looks at them only then.
    joins_wait: AtomicBool,
    stop: Arc<Stop>,
}

impl Default for Swarm {
    fn default() -> Self {
        Self::with_rtc(rtc::platform(), STORE_BYTES)
    }
}

/// The swarms of this process, by the native node identity they speak as.
#[cfg(feature = "native")]
static SHARED: LazyLock<Mutex<HashMap<[u8; 32], Weak<Inner>>>> = LazyLock::new(Mutex::default);

impl Swarm {
    /// The swarm for clients on `network`. Rendezvous nodes know a native
    /// client's memberships by its node identity, so clients built on one
    /// native node share one swarm, rather than taking each other's places.
    #[cfg(feature = "native")]
    pub(crate) fn for_network(network: &Network) -> Self {
        let identity = *network.node().peer_id().to_bytes();
        let mut shared = SHARED.lock().unwrap_or_else(PoisonError::into_inner);
        shared.retain(|_, inner| inner.strong_count() > 0);
        if let Some(inner) = shared.get(&identity).and_then(Weak::upgrade) {
            return Self { inner };
        }
        let swarm = Self::default();
        shared.insert(identity, Arc::downgrade(&swarm.inner));
        swarm
    }

    /// The swarm for a browser client: its sessions with the nodes are its
    /// own, so it has its own swarm. On an adapter that cannot take part
    /// (see `BrowserNetwork::supports_swarm`), one that joins no room.
    #[cfg(not(feature = "native"))]
    pub(crate) fn for_network(network: &Network) -> Self {
        if network.supports_swarm() {
            Self::default()
        } else {
            Self::with_rtc(None, 0)
        }
    }

    pub(crate) fn with_rtc(rtc: Option<Arc<dyn Rtc>>, store_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Inner {
                rtc,
                store: RecordStore::new(store_bytes),
                links: Mutex::default(),
                rooms: Mutex::default(),
                leaving: Arc::new(Semaphore::new(MAX_LEAVING)),
                uploads: Arc::default(),
                reading: Arc::default(),
                active_reads: Mutex::default(),
                answers: Arc::new(Semaphore::new(MAX_ANSWERS)),
                receiving: Arc::new(Semaphore::new(MAX_RECEIVING)),
                connecting: Arc::default(),
                next_link: AtomicU64::new(1),
                counters: Arc::default(),
                discovery: DiscoveryCounters::default(),
                node_read_ms: AtomicU64::new(0),
                waiting_joins: Mutex::default(),
                joins_wait: AtomicBool::new(false),
                stop: Arc::default(),
            }),
        }
    }

    /// Joins the room for a file being read, if not in it already, through
    /// `network`, with `records`, the file's records: the only ones shared
    /// in it, held by the room before any other read can see it (a room
    /// already joined keeps its list). Returns whether it joined the room now
    /// (it was not in it, or had left it since). A room already joined counts
    /// as read now. Joining a room past [`MAX_ROOMS`] leaves the one unread
    /// longest, once no read has used it for [`ROOM_TURNOVER`]; with none
    /// such, the join waits (at most [`MAX_WAITING_JOINS`] do) and is made at
    /// a later read of one of the file's records, once a room has gone
    /// unread.
    pub(crate) fn join(
        &self,
        key: RoomKey,
        secret: [u8; 32],
        records: impl IntoIterator<Item = XorName>,
        network: &Network,
    ) -> bool {
        // Joined, or waiting already: a later read of the file makes a
        // waiting join, so the records are not gathered again meanwhile.
        if !self.can_join() || self.read_if_joined(&key) || self.waits_to_join(&key) {
            return false;
        }
        let records: HashSet<XorName> = records.into_iter().take(room::MAX_ROOM_RECORDS).collect();
        // A join made now takes the place of one that waited.
        let _ = self.stop_waiting(&key);
        match self.enter(key, secret, records, network) {
            Ok(joined) => joined,
            Err(records) => {
                self.wait_to_join(WaitingJoin {
                    key,
                    secret,
                    records,
                    network: network.clone(),
                });
                false
            }
        }
    }

    /// Whether a room's task can run: there is WebRTC, the swarm runs, and
    /// an executor takes tasks.
    fn can_join(&self) -> bool {
        self.inner.rtc.is_some() && !self.inner.stop.is_stopped() && can_spawn()
    }

    /// Counts the room `key` as read now, if this client is in it.
    fn read_if_joined(&self, key: &RoomKey) -> bool {
        let mut rooms = self.inner.rooms();
        let Some(position) = rooms.iter().position(|room| room.key == *key) else {
            return false;
        };
        let room = rooms.remove(position);
        room.note_read(crate::runtime::Clock::now());
        rooms.push(room);
        true
    }

    /// Joins room `key` with `records` unless this client is in it: `Ok`
    /// with whether it joined now, or `Err` with the records back when every
    /// one of [`MAX_ROOMS`] rooms is still read and none made way.
    fn enter(
        &self,
        key: RoomKey,
        secret: [u8; 32],
        records: HashSet<XorName>,
        network: &Network,
    ) -> Result<bool, HashSet<XorName>> {
        let inner = &self.inner;
        if !self.can_join() {
            return Ok(false);
        }
        let now = crate::runtime::Clock::now();
        let (room, left) = {
            let mut rooms = inner.rooms();
            if let Some(room) = rooms.iter().find(|room| room.key == key) {
                room.note_read(now);
                return Ok(false);
            }
            let leave = if rooms.len() >= MAX_ROOMS {
                let Some(position) = room_to_leave(&rooms, now) else {
                    return Err(records);
                };
                Some(position)
            } else {
                None
            };
            // Its key first: a room that cannot have one is not joined, and
            // pushes no other room out.
            let Some(room) = Room::new(key, inner.next_membership(), secret) else {
                return Ok(false);
            };
            let left = leave.map(|position| rooms.remove(position));
            (inner.admit(&mut rooms, room, records), left)
        };
        if let Some(left) = left {
            left.leave_with(Arc::clone(&inner.leaving).try_acquire_owned().ok());
            inner.leave_room(&left);
        }
        if !room::start(inner, Arc::clone(&room), network.clone()) {
            // No executor to keep the room: forget it, so a later join tries
            // again.
            inner.rooms().retain(|joined| !Arc::ptr_eq(joined, &room));
            return Ok(false);
        }
        Ok(true)
    }

    /// Lets room `key`, if joined, start finding its file's other readers now
    /// rather than after its read's first record (see `Room::until_started`).
    pub(crate) fn start_finding(&self, key: &RoomKey) {
        if let Some(room) = self.inner.room(key) {
            room.start_finding();
        }
    }

    /// Whether a join of room `key` waits for a room to make way.
    fn waits_to_join(&self, key: &RoomKey) -> bool {
        self.inner.joins_wait.load(Ordering::Acquire)
            && self
                .inner
                .waiting_joins
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .iter()
                .any(|join| join.key == *key)
    }

    /// Keeps a join no room made way for, to make at a later read of its
    /// file; the oldest waiting is dropped past [`MAX_WAITING_JOINS`].
    fn wait_to_join(&self, join: WaitingJoin) {
        let mut waiting = self
            .inner
            .waiting_joins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        waiting.retain(|waiting| waiting.key != join.key);
        if waiting.len() >= MAX_WAITING_JOINS {
            waiting.pop_front();
        }
        waiting.push_back(join);
        self.inner.joins_wait.store(true, Ordering::Release);
    }

    /// Makes the waiting join whose file holds `address`, if any, now that a
    /// record of it is read: if a room has gone unread meanwhile. Returns
    /// whether this client is in that room now. The join leaves the waiting
    /// list while it is tried, so only one read makes it, and goes back if
    /// still no room makes way.
    fn join_waiting_for(&self, address: &XorName) -> bool {
        if !self.inner.joins_wait.load(Ordering::Acquire) {
            return false;
        }
        let taken = {
            let mut waiting = self
                .inner
                .waiting_joins
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let taken = waiting
                .iter()
                .position(|join| join.records.contains(address))
                .and_then(|position| waiting.remove(position));
            self.inner
                .joins_wait
                .store(!waiting.is_empty(), Ordering::Release);
            taken
        };
        let Some(WaitingJoin {
            key,
            secret,
            records,
            network,
        }) = taken
        else {
            return false;
        };
        match self.enter(key, secret, records, &network) {
            Ok(_) => self.inner.room(&key).is_some(),
            Err(records) => {
                self.wait_to_join(WaitingJoin {
                    key,
                    secret,
                    records,
                    network,
                });
                false
            }
        }
    }

    /// Forgets the waiting join for `key`, returning it.
    fn stop_waiting(&self, key: &RoomKey) -> Option<WaitingJoin> {
        let mut waiting = self
            .inner
            .waiting_joins
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let position = waiting.iter().position(|join| join.key == *key)?;
        let join = waiting.remove(position)?;
        self.inner
            .joins_wait
            .store(!waiting.is_empty(), Ordering::Release);
        Some(join)
    }

    /// Adds records of the room's file: only these are fetched from, kept
    /// for, announced to and served to the room's other members.
    #[cfg(test)]
    pub(crate) fn register(&self, key: &RoomKey, addresses: impl IntoIterator<Item = XorName>) {
        let Some(room) = self.inner.room(key) else {
            return;
        };
        // Those this client holds already (read before, in a room left since)
        // are told to the room's links: one that opened before they were
        // added left them out of what it said first. The store is looked at
        // after the room's records are let go, the order every other lookup
        // takes them in.
        let held: Vec<XorName> = room
            .hold(addresses)
            .into_iter()
            .filter(|address| self.inner.store.contains(address))
            .collect();
        if held.is_empty() {
            return;
        }
        for link in self
            .inner
            .links()
            .iter()
            .filter(|link| link.room == *key && link.is_open())
        {
            link.announce(&held);
        }
    }

    /// A record from another client in a room whose file holds it, checked
    /// against `address`, or `None` to ask nodes.
    #[cfg(test)]
    pub(crate) async fn fetch(&self, address: &XorName) -> Option<Bytes> {
        self.fetch_paced(address, &Pace::default()).await
    }

    /// As [`Swarm::fetch`], telling `pace` how far the record being received
    /// has come.
    pub(crate) async fn fetch_paced(&self, address: &XorName, pace: &Pace) -> Option<Bytes> {
        // A read of the room's file, wherever its record comes from.
        let rooms = self.inner.rooms_read(address);
        if rooms.is_empty() {
            return None;
        }
        // Held already, checked when it came: from another client on the same
        // node, or read before and kept out of the chunk cache. A record in
        // for the read, so its rooms may start finding other readers.
        if let Some(record) = self.inner.store.get(address) {
            for key in &rooms {
                self.start_finding(key);
            }
            return Some(record);
        }
        // A hidden page's timers are slowed to a second or more, which would
        // stretch the peers' deadlines far past their bounds: its reads go
        // to the nodes. It still serves, as its sends wait on channel events.
        if crate::runtime::page_hidden() {
            return None;
        }
        let Ok(_receiving) = Arc::clone(&self.inner.receiving).try_acquire_owned() else {
            return None;
        };
        // A page hidden meanwhile has its timers slowed too: the read leaves
        // the peers for the nodes at once.
        let asked = std::pin::pin!(self.ask_peers(address, &rooms, pace));
        let hidden = std::pin::pin!(crate::runtime::until_page_hidden());
        match select(asked, hidden).await {
            Either::Left((record, _)) => record,
            Either::Right(_) => None,
        }
    }

    /// Asks the room's linked peers that hold `address` for it, checking what
    /// comes back: [`FETCH_PEERS`] of them at most, within
    /// [`link::FETCH_TOTAL`], and only links that take another ask now. Both
    /// bounds are the read's, counted across its asks (`pace` carries them).
    async fn ask_peers(&self, address: &XorName, rooms: &[RoomKey], pace: &Pace) -> Option<Bytes> {
        let mut spending = Spending::take(&pace.spent);
        let Spent {
            deadline,
            tries,
            tried,
        } = &mut spending.spent;
        let counters = &self.inner.counters;
        // Peers are picked afresh for each try, so a link that opens or
        // closes during the read (two links to one peer settling on one, say)
        // is seen.
        // After a link was settled away mid-read, its twin may not have said
        // what it holds yet: the read waits a little for it.
        let mut twin_wait: Option<web_time::Instant> = None;
        while *tries < FETCH_PEERS && deadline.is_none_or(|until| web_time::Instant::now() < until)
        {
            let candidates: Vec<Arc<Link>> = self
                .inner
                .links()
                .iter()
                .filter(|link| {
                    rooms.contains(&link.room)
                        && !tried.contains(&link.id)
                        && link.is_open()
                        && link.offers(address)
                        && link.takes_asks()
                })
                .cloned()
                .collect();
            let Some(link) = candidates.choose(&mut rand::thread_rng()).cloned() else {
                let now = web_time::Instant::now();
                match twin_wait.map(|until| deadline.map_or(until, |end| until.min(end))) {
                    Some(until) if now < until => {
                        crate::runtime::sleep(TWIN_POLL.min(until - now)).await;
                        continue;
                    }
                    _ => break,
                }
            };
            tried.push(link.id);
            let until =
                *deadline.get_or_insert_with(|| web_time::Instant::now() + link::FETCH_TOTAL);
            let record = match link.get(*address, until, pace).await {
                GetOutcome::Record(record) => record,
                GetOutcome::Busy => {
                    counters.asks_busy.fetch_add(1, Ordering::Relaxed);
                    *tries += 1;
                    continue;
                }
                // Asked and waited on by as many reads as it takes: this read
                // tries another link, or the nodes.
                GetOutcome::Full => continue,
                GetOutcome::Missing => {
                    counters.asks_missing.fetch_add(1, Ordering::Relaxed);
                    *tries += 1;
                    continue;
                }
                GetOutcome::Failed => {
                    counters.asks_failed.fetch_add(1, Ordering::Relaxed);
                    // A link closed only as the duplicate of one kept is no
                    // failed peer: the read goes on to its twin.
                    if link.settled_away() {
                        twin_wait
                            .get_or_insert_with(|| web_time::Instant::now() + link::FETCH_STALL);
                    } else {
                        *tries += 1;
                    }
                    continue;
                }
            };
            if crate::record::verify(address, &record).is_err() {
                counters.records_rejected.fetch_add(1, Ordering::Relaxed);
                link.room_counters
                    .records_rejected
                    .fetch_add(1, Ordering::Relaxed);
                link.close();
                *tries += 1;
                continue;
            }
            counters.fetched(record.len());
            link.room_counters.fetched(record.len());
            self.seed(*address, record.clone());
            return Some(record);
        }
        None
    }

    /// Counts a read of `address` as running until the guard goes: while any
    /// read of this client's runs, it sends other clients one record at a
    /// time; and a room whose file holds `address` counts as read the whole
    /// time, so a read waiting at busy nodes never loses its room to another
    /// file's (see [`room_to_leave`]).
    pub(crate) fn reading(&self, address: &XorName) -> Reading {
        self.inner.reading.fetch_add(1, Ordering::AcqRel);
        self.inner.begin_read(address);
        Reading {
            reading: Arc::clone(&self.inner.reading),
            inner: Arc::downgrade(&self.inner),
            address: *address,
        }
    }

    /// Notes that a read had its record from the nodes `took` after it turned
    /// to them.
    pub(crate) fn note_node_read(&self, took: std::time::Duration) {
        let took = u64::try_from(took.as_millis()).unwrap_or(u64::MAX);
        let _ = update_u64(&self.inner.node_read_ms, |average| {
            Some(if average == 0 {
                took.max(1)
            } else {
                average.saturating_mul(4).saturating_add(took) / 5
            })
        });
    }

    /// How long a read at most waits for a peer that keeps on pace before it
    /// asks the nodes too: as long as reads have lately taken to have their
    /// records from the nodes, at least `floor`, and never past the time a
    /// peer has to send a record ([`link::FETCH_TOTAL`]).
    pub(crate) fn patience(&self, floor: std::time::Duration) -> std::time::Duration {
        std::time::Duration::from_millis(self.inner.node_read_ms.load(Ordering::Relaxed))
            .max(floor)
            .min(link::FETCH_TOTAL.max(floor))
    }

    /// Whether a read of `address` could have it from this swarm now: it is
    /// held already, or a link of a room whose file holds it says it has it
    /// and takes an ask. A read waiting for its turn at the nodes looks.
    #[cfg(any(test, not(feature = "native")))]
    pub(crate) fn could_serve(&self, address: &XorName) -> bool {
        let rooms = self.inner.rooms_holding(address);
        if rooms.is_empty() || crate::runtime::page_hidden() {
            return false;
        }
        if self.inner.store.get(address).is_some() {
            return true;
        }
        self.inner.links().iter().any(|link| {
            rooms.contains(&link.room)
                && link.is_open()
                && link.offers(address)
                && link.takes_asks()
        })
    }

    /// Keeps a record checked against `address` to serve to the members of
    /// the rooms whose file holds it, and tells them. A record of no such
    /// file is not kept.
    pub(crate) fn seed(&self, address: XorName, record: Bytes) {
        // A record read keeps its rooms from making way for others' files,
        // and makes a join that waited for a room, if one has gone unread.
        let mut rooms = self.inner.rooms_read(&address);
        if rooms.is_empty() && self.join_waiting_for(&address) {
            rooms = self.inner.rooms_read(&address);
        }
        // A record in for a read: its rooms may start finding the file's
        // other readers.
        for key in &rooms {
            self.start_finding(key);
        }
        if rooms.is_empty() || !self.inner.store.insert(address, record) {
            return;
        }
        for link in self
            .inner
            .links()
            .iter()
            .filter(|link| rooms.contains(&link.room) && link.is_open())
        {
            link.announce(&[address]);
        }
    }

    /// Leaves every room and closes every link; joins after this do nothing.
    /// A native client does this when it is dropped.
    #[cfg(any(test, not(feature = "native")))]
    pub(crate) fn shutdown(&self) {
        self.inner.shutdown();
    }

    pub(crate) fn stats(&self) -> SwarmStats {
        let counters = &self.inner.counters;
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        // One lock at a time: links and rooms are each taken alone here.
        let links = self
            .inner
            .links()
            .iter()
            .filter(|link| link.is_open())
            .count() as u64;
        let rooms = self.inner.rooms().len() as u64;
        SwarmStats {
            records_from_peers: load(&counters.records_from_peers),
            bytes_from_peers: load(&counters.bytes_from_peers),
            records_rejected: load(&counters.records_rejected),
            asks_busy: load(&counters.asks_busy),
            asks_missing: load(&counters.asks_missing),
            asks_failed: load(&counters.asks_failed),
            records_served: load(&counters.records_served),
            bytes_served: load(&counters.bytes_served),
            links,
            rooms,
            discovery: self.inner.discovery.snapshot(),
        }
    }

    /// What each room this client is in now has fetched and served, in the
    /// order the rooms were last joined.
    pub(crate) fn room_stats(&self) -> Vec<RoomStats> {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        // One lock at a time: rooms, then links.
        let rooms: Vec<Arc<Room>> = self.inner.rooms().iter().cloned().collect();
        let links: Vec<(RoomKey, String)> = self
            .inner
            .links()
            .iter()
            .filter(|link| link.is_open())
            .map(|link| (link.room, link.fingerprint.clone()))
            .collect();
        rooms
            .iter()
            .map(|room| {
                let counters = &room.counters;
                RoomStats {
                    tag: room.tag.clone(),
                    records_from_peers: load(&counters.records_from_peers),
                    bytes_from_peers: load(&counters.bytes_from_peers),
                    records_rejected: load(&counters.records_rejected),
                    records_served: load(&counters.records_served),
                    bytes_served: load(&counters.bytes_served),
                    links: links.iter().filter(|(key, _)| *key == room.key).count() as u64,
                    open_links: links
                        .iter()
                        .filter(|(key, _)| *key == room.key)
                        .map(|(_, fingerprint)| fingerprint.clone())
                        .collect(),
                }
            })
            .collect()
    }
}

impl Inner {
    /// A place to send a record now, or `None`: [`MAX_UPLOADS`] at once, and
    /// [`UPLOADS_WHILE_READING`] while reads of this client's own run, taken
    /// in one step so two links never both take the last. Sends already
    /// running when a read starts finish.
    fn start_upload(&self) -> Option<Upload> {
        let limit = if self.reading.load(Ordering::Acquire) > 0 {
            UPLOADS_WHILE_READING
        } else {
            MAX_UPLOADS
        };
        update_usize(&self.uploads, |running| {
            (running < limit).then_some(running + 1)
        })
        .ok()?;
        Some(Upload(Arc::clone(&self.uploads)))
    }

    fn links(&self) -> MutexGuard<'_, Vec<Arc<Link>>> {
        self.links.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn rooms(&self) -> MutexGuard<'_, Vec<Arc<Room>>> {
        self.rooms.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn active_reads(&self) -> MutexGuard<'_, HashMap<XorName, usize>> {
        self.active_reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Notes a read of `address` starting: each room whose file holds it
    /// counts as read until it ends.
    fn begin_read(&self, address: &XorName) {
        let rooms = self.rooms();
        *self.active_reads().entry(*address).or_default() += 1;
        for room in rooms.iter().filter(|room| room.holds(address)) {
            room.begin_read();
        }
    }

    /// Notes a read of `address` ending: the rooms whose file holds it, those
    /// joined while it ran included, go unread from now unless another read
    /// keeps them.
    fn end_read(&self, address: &XorName) {
        let rooms = self.rooms();
        {
            let mut reads = self.active_reads();
            match reads.get_mut(address) {
                Some(count) if *count > 1 => *count -= 1,
                Some(_) => {
                    reads.remove(address);
                }
                None => return,
            }
        }
        let now = crate::runtime::Clock::now();
        for room in rooms.iter().filter(|room| room.holds(address)) {
            room.end_read(now);
        }
    }

    /// Adds `room` to `rooms` (the swarm's, held) with `records`, its file's:
    /// its records before any read can find it, since a room holding none
    /// would share nothing, and the reads of them running now, which keep it
    /// as they would have had it been joined before they began.
    fn admit(
        &self,
        rooms: &mut Vec<Arc<Room>>,
        room: Room,
        records: HashSet<XorName>,
    ) -> Arc<Room> {
        room.hold_all(records);
        for (address, count) in self.active_reads().iter() {
            if room.holds(address) {
                room.begin_reads(*count);
            }
        }
        let room = Arc::new(room);
        rooms.push(Arc::clone(&room));
        room
    }

    /// A membership number newer than every one this process gave before.
    /// Nodes compare only those of one connection identity, which a client
    /// makes afresh each run; one counter for the whole process keeps them
    /// growing for clients made one after another on one identity, and
    /// starting from the clock keeps them growing for an identity kept across
    /// runs, unless the clock went back.
    fn next_membership(&self) -> Membership {
        next_membership()
    }

    fn room(&self, key: &RoomKey) -> Option<Arc<Room>> {
        self.rooms().iter().find(|room| room.key == *key).cloned()
    }

    /// The rooms whose file holds `address`, for a record this client reads:
    /// each of those rooms counts as read now.
    fn rooms_read(&self, address: &XorName) -> Vec<RoomKey> {
        let now = crate::runtime::Clock::now();
        self.rooms()
            .iter()
            .filter(|room| room.holds(address))
            .map(|room| {
                room.note_read(now);
                room.key
            })
            .collect()
    }

    /// The rooms whose file holds `address`.
    #[cfg(any(test, not(feature = "native")))]
    fn rooms_holding(&self, address: &XorName) -> Vec<RoomKey> {
        self.rooms()
            .iter()
            .filter(|room| room.holds(address))
            .map(|room| room.key)
            .collect()
    }

    fn room_holds(&self, key: &RoomKey, address: &XorName) -> bool {
        self.room(key).is_some_and(|room| room.holds(address))
    }

    /// A place for one more connection in `room` through rendezvous node
    /// `node`, if this client wants another link to one of the `members` it
    /// has there: counting the links to members of that node (under any name
    /// they have there), and every connection being made in the room, within
    /// the room's and the client's caps. Each node's links are counted alone,
    /// so two readers whose views share only one node still meet there; a
    /// link to a member of both nodes counts at both once its duplicate
    /// settles. Connections being made anywhere count at every node, as they
    /// may reach the same peers, so a reader starting out does not offer
    /// through both nodes at once to a peer of both. Taken while the links
    /// are locked, so tasks asking at once cannot overshoot; it counts until
    /// the guard is dropped.
    fn reserve_connection(
        &self,
        room: &Room,
        node: &[u8; 32],
        members: usize,
    ) -> Option<Connecting> {
        let links = self.links();
        let connecting_here = room.connecting.load(Ordering::Acquire);
        let connecting = self.connecting.load(Ordering::Acquire);
        let in_room = links.iter().filter(|link| link.room == room.key).count();
        let at_node = links
            .iter()
            .filter(|link| {
                link.room == room.key
                    && link.nonce == room.nonce
                    && !link.is_closed()
                    && link.known_at(node)
            })
            .count();
        let wanted = members
            .saturating_sub(1)
            .saturating_sub(at_node + connecting_here)
            .min(MAX_LINKS_PER_ROOM.saturating_sub(in_room + connecting_here))
            .min(MAX_LINKS.saturating_sub(links.len() + connecting));
        (wanted > 0).then(|| Connecting::new(&self.connecting, &room.connecting))
    }

    /// Whether this membership of `room` has a link to `peer` it `offered`
    /// (or answered, if not), not closed.
    fn has_link(&self, room: &Room, peer: &PeerRef, offered: bool) -> bool {
        self.links().iter().any(|link| {
            link.room == room.key
                && link.nonce == room.nonce
                && !link.is_closed()
                && link.peer.as_ref() == Some(peer)
                && link.offered == offered
        })
    }

    /// Keeps a new link and starts reading it, unless the caps are reached,
    /// this membership of the room was left, or the swarm stopped.
    fn attach(
        self: &Arc<Self>,
        room: &Arc<Room>,
        offered: bool,
        offer_id: OfferId,
        binding: link::Binding,
        peer: Option<PeerRef>,
        channel: Channel,
    ) -> Option<Arc<Link>> {
        let Channel { sender, events } = channel;
        // The rooms stay locked until the link is in: leaving the room either
        // comes first and is seen here, or comes after and closes the link.
        // A link made for a membership left since is not kept by a later one.
        let rooms = self.rooms();
        if self.stop.is_stopped() || !rooms.iter().any(|joined| Arc::ptr_eq(joined, room)) {
            drop(rooms);
            sender.close();
            return None;
        }
        let link = {
            let mut links = self.links();
            let in_room = links.iter().filter(|link| link.room == room.key).count();
            // One link each way per member, as the node that introduced it
            // names it, until the links prove themselves: two clients offering
            // to each other at once make one each way, and settle on one once
            // open (see `settle_duplicates`), while a member cannot fill the
            // room's links by connecting again and again.
            let again = peer.is_some()
                && links.iter().any(|link| {
                    link.room == room.key
                        && link.nonce == room.nonce
                        && !link.is_closed()
                        && link.peer == peer
                        && link.offered == offered
                });
            if again || links.len() >= MAX_LINKS || in_room >= MAX_LINKS_PER_ROOM {
                drop(links);
                sender.close();
                return None;
            }
            let id = self.next_link.fetch_add(1, Ordering::Relaxed);
            // A link this client cannot sign for closes its connection as the
            // sender goes.
            let link = Arc::new(Link::new(
                id, room, offered, offer_id, &binding, peer, sender,
            )?);
            links.push(Arc::clone(&link));
            link
        };
        drop(rooms);
        if !spawn(Arc::clone(&link).run(Arc::downgrade(self), events)) {
            link.close();
            self.remove_link(link.id);
            return None;
        }
        Some(link)
    }

    fn remove_link(&self, id: u64) {
        self.links().retain(|link| link.id != id);
    }

    /// Two links between the same pair of clients in one room (met through
    /// both rendezvous nodes, or each offering to the other at once) keep the
    /// one both ends pick: the lowest offerer nonce, then offer id. A link to
    /// this client itself is closed.
    fn settle_duplicates(self: &Arc<Self>, link: &Arc<Link>) {
        let Some(peer) = link.peer_nonce() else {
            return;
        };
        if peer == link.nonce {
            link.close();
            return;
        }
        // A member the introducing node named before, already linked under
        // another name, keeps its first link: one member, one link.
        let renamed = link.peer.is_some()
            && self.links().iter().any(|other| {
                other.id != link.id
                    && other.room == link.room
                    && other.nonce == link.nonce
                    && other.is_open()
                    && other.peer == link.peer
                    && other.peer_nonce() != Some(peer)
            });
        if renamed {
            link.close();
            return;
        }
        let rank = |link: &Link| {
            let offerer = if link.offered { link.nonce } else { peer };
            (offerer, link.offer_id)
        };
        let twins: Vec<Arc<Link>> = self
            .links()
            .iter()
            .filter(|other| {
                other.id != link.id
                    && other.room == link.room
                    && other.nonce == link.nonce
                    && !other.is_closed()
                    && other.peer_nonce() == Some(peer)
            })
            .cloned()
            .collect();
        // The link kept learns the name the peer had on the one closed.
        for twin in twins {
            if rank(&twin) < rank(link) {
                link.settle_away(&Arc::downgrade(self));
                twin.also_known_as(link.peer);
                return;
            }
            twin.settle_away(&Arc::downgrade(self));
            link.also_known_as(twin.peer);
        }
    }

    fn leave_room(&self, room: &Room) {
        room.stop.stop();
        room.forget_records();
        let closing: Vec<Arc<Link>> = self
            .links()
            .iter()
            .filter(|link| link.room == room.key && link.nonce == room.nonce)
            .cloned()
            .collect();
        for link in closing {
            link.close();
        }
    }

    fn shutdown(&self) {
        self.stop.stop();
        let rooms = std::mem::take(&mut *self.rooms());
        for room in &rooms {
            room.stop.stop();
            room.forget_records();
        }
        let links = std::mem::take(&mut *self.links());
        for link in links {
            link.close();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests;
