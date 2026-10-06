//! Sequential read-ahead for the browser file reader. Media playback reads a file
//! front to back in small ranges. Fetching the next records in parallel overlaps
//! each record's discovery with playback, instead of stalling playback on it.
//! The readers of a client hold the records they fetch within one budget, so
//! neither the client's shared chunk cache nor another reader can evict them
//! before use.
//!
//! DataMap sizes are untrusted, so every bound counts records, each at most one
//! browser response, and never declared plaintext bytes. A fetch is admitted
//! only within those bounds; the read fetches any record refused here itself.
//!
//! Read-ahead fetches through a client of its own. Its GETs are speculative
//! unless a read waits for their record, and the read budget admits speculative
//! GETs, cancelled ones included, only within a share of its cap. Only fetches a
//! read waited for inform the reads' adaptive concurrency, and read-ahead keeps
//! its records out of the shared chunk cache.
use super::{BrowserNetworkCore, SharedNetworkAdapter};
use crate::client_engine::files::FileIndex;
use crate::client_engine::read_ahead::releasable;
use crate::client_engine::read_budget::ReadBudget;
use crate::data::{ChunkCache, Client, ClientConfig, Error, Network};
use bytes::Bytes;
use futures::future::{AbortHandle, Abortable, FutureExt, LocalBoxFuture, Shared};
use self_encryption::MAX_CHUNK_SIZE;
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

/// Plaintext fetched ahead of a sequential read: about a minute of 5 Mbit/s video.
const READ_AHEAD_BYTES: usize = 32 * 1024 * 1024;
/// Records in the window at most: as many as a window of full-size records
/// overlaps when it starts on a record's last byte. Undersized records declared
/// by a DataMap shorten the window rather than lengthen it.
const MAX_WINDOW_RECORDS: usize = (READ_AHEAD_BYTES - 1) / MAX_CHUNK_SIZE + 2;
/// Records a reader fetches in parallel within its read-ahead window.
const READ_AHEAD_CONCURRENCY: usize = 4;
/// Records a reader has in flight: the window's fetches plus one left behind by
/// a seek, such as the start of a file that a player returns to after reading
/// the index at its end. A new fetch cancels stale ones beyond this bound. GETs
/// a cancelled fetch already sent still drain, within the read budget's
/// speculative share.
const MAX_IN_FLIGHT: usize = READ_AHEAD_CONCURRENCY + 1;
/// Streams the held-record budget covers at once, each with its window and the
/// last record, such as two players on one page.
const CONCURRENT_STREAMS: usize = 2;
/// Records the budget keeps beyond those streams, such as recent records for
/// short seeks back.
const SEEK_BACK_RECORDS: usize = 2;
/// Records the readers of one client hold or fetch at most. Each is at most one
/// browser response, so this bounds memory whatever a DataMap declares: about
/// 100 MB of full-size records at the default chunk size. It follows the window,
/// so a build with smaller chunks still covers its streams.
const MAX_HELD_RECORDS: usize = CONCURRENT_STREAMS * (MAX_WINDOW_RECORDS + 1) + SEEK_BACK_RECORDS;
/// Recent read ends a new read may continue. Media elements interleave the
/// sequential stream with index reads, such as an MP4 `moov` at the end.
const RECENT_READS: usize = 4;
/// A reader without a read for this long is idle. Once the budget is full, all
/// its records, window included, are released before any other reader's, so a
/// reader that JavaScript drops without closing cannot keep them.
const IDLE_READER: Duration = Duration::from_secs(60);
/// The read-ahead client's chunk cache cannot be empty. Each fetch removes its
/// record from that cache once it settles, so read-ahead holds records only
/// within its own budget.
const READ_AHEAD_CACHE_RECORDS: usize = 1;

type Fetch = Shared<LocalBoxFuture<'static, ()>>;

/// Read-ahead state of one record. A record without a slot may be fetched.
enum Slot {
    /// A background fetch. The id distinguishes a cancelled fetch from a later
    /// one for the same record.
    Fetching {
        id: u64,
        fetch: Fetch,
        abort: AbortHandle,
    },
    /// Verified record content, held for the reader.
    Held(Bytes),
    /// The fetch failed. A read needing the record fetches it itself; read-ahead
    /// retries it only after a later read, never on its own.
    Failed,
}

/// Records that reads in progress wait for, counted across a client's readers.
/// A read-ahead GET for any other record is speculative.
#[derive(Default)]
pub(super) struct AwaitedRecords(RefCell<HashMap<[u8; 32], usize>>);

impl AwaitedRecords {
    pub(super) fn contains(&self, address: &[u8; 32]) -> bool {
        self.0.borrow().contains_key(address)
    }

    fn add(&self, addresses: &[[u8; 32]]) {
        let mut counts = self.0.borrow_mut();
        for address in addresses {
            *counts.entry(*address).or_default() += 1;
        }
    }

    fn remove(&self, addresses: &[[u8; 32]]) {
        let mut counts = self.0.borrow_mut();
        for address in addresses {
            if let Some(count) = counts.get_mut(address) {
                *count -= 1;
                if *count == 0 {
                    counts.remove(address);
                }
            }
        }
    }
}

/// Read-ahead shared by the readers of one client: the client it fetches
/// through, the records reads wait for, and the held-record budget.
pub(super) struct ReadAheadPool {
    /// Fetches records ahead of reads. Its GETs are speculative unless a read
    /// waits for their record.
    client: Client,
    /// The client reads fetch through. Read-ahead takes records from its cache
    /// and reports the fetches a read waited for to its adaptive limiter.
    reads: Rc<Client>,
    awaited: Rc<AwaitedRecords>,
    budget: Arc<ReadBudget>,
    readers: RefCell<Vec<Weak<ReadAhead>>>,
}

impl ReadAheadPool {
    pub(super) fn new(core: &Rc<BrowserNetworkCore>, reads: &Rc<Client>) -> Rc<Self> {
        let awaited = Rc::<AwaitedRecords>::default();
        let adapter = SharedNetworkAdapter::read_ahead(Rc::clone(core), Rc::clone(&awaited));
        let client = Client::from_network(
            Network::from_browser(Rc::new(adapter)),
            ClientConfig::default(),
        )
        .with_chunk_cache(ChunkCache::new(READ_AHEAD_CACHE_RECORDS));
        Rc::new(Self {
            client,
            reads: Rc::clone(reads),
            awaited,
            budget: Arc::clone(&core.pool.read_budget),
            readers: RefCell::default(),
        })
    }

    /// Fetch one record for read-ahead.
    async fn fetch(&self, address: [u8; 32]) -> crate::data::Result<Option<Bytes>> {
        // A record a read fetched itself needs no second fetch.
        if let Some(content) = self.reads.chunk_cache().get(&address) {
            if crate::record::verify(&address, &content).is_ok() {
                return Ok(Some(content));
            }
        }
        let epoch = self.reads.controller().fetch.observation_epoch();
        let started = Instant::now();
        let result = self.client.chunk_get(&address).await;
        self.client.chunk_cache().remove(&address);
        if self.awaited.contains(&address) {
            self.reads
                .observe_chunk_get(&result, started.elapsed(), epoch);
        }
        result.map(|chunk| chunk.map(|chunk| chunk.content))
    }

    /// Open readers, forgetting dropped and closed ones.
    fn readers(&self) -> Vec<Rc<ReadAhead>> {
        let mut readers = self.readers.borrow_mut();
        readers.retain(|reader| reader.upgrade().is_some_and(|reader| !reader.closed.get()));
        readers.iter().filter_map(Weak::upgrade).collect()
    }

    /// Make room for one more record held or in flight across the client's
    /// readers. Held records over the budget are released from idle readers
    /// first, whole; then those `requester` does not keep; then those other
    /// readers do not keep. Returns whether a fetch fits.
    fn make_room(&self, requester: &ReadAhead) -> bool {
        let readers = self.readers();
        let used = readers
            .iter()
            .map(|reader| reader.fetching() + reader.held())
            .sum::<usize>();
        let mut excess = (used + 1).saturating_sub(MAX_HELD_RECORDS);
        let now = Instant::now();
        let (own, others): (Vec<_>, Vec<_>) = readers
            .iter()
            .partition(|reader| std::ptr::eq(Rc::as_ptr(reader), requester));
        let (idle, active): (Vec<_>, Vec<_>) =
            others.into_iter().partition(|reader| reader.is_idle(now));
        let order = idle
            .into_iter()
            .map(|reader| (reader, true))
            .chain(own.into_iter().chain(active).map(|reader| (reader, false)));
        for (reader, idle) in order {
            if excess == 0 {
                break;
            }
            excess -= reader.release(excess, idle);
        }
        excess == 0
    }
}

/// Read-ahead of one file reader.
pub(super) struct ReadAhead {
    pool: Rc<ReadAheadPool>,
    /// The reader's index of the file, shared with it.
    layout: Rc<FileIndex>,
    /// Every read starts a sequential stream, as media playback's reads do.
    streaming: bool,
    /// Ends of recent reads; a read starting at one continues it.
    recent_ends: RefCell<VecDeque<u64>>,
    /// Start of the latest sequential read; the read-ahead window starts here.
    anchor: Cell<Option<u64>>,
    /// Records of each read in progress, by read id. Like the window, they are
    /// never cancelled or released.
    reads: RefCell<BTreeMap<u64, Range<usize>>>,
    /// First record of the latest read; held records behind it are released
    /// first.
    position: Cell<usize>,
    /// When a read last began or ended.
    last_read: Cell<Instant>,
    slots: RefCell<BTreeMap<usize, Slot>>,
    next_id: Cell<u64>,
    closed: Cell<bool>,
}

impl ReadAhead {
    /// Read-ahead for one reader of the file `layout` indexes. Positions are
    /// 64-bit, so read-ahead follows reads past 4 GiB on every platform.
    pub(super) fn new(
        pool: &Rc<ReadAheadPool>,
        layout: Rc<FileIndex>,
        streaming: bool,
    ) -> Rc<Self> {
        let reader = Rc::new(Self {
            pool: Rc::clone(pool),
            layout,
            streaming,
            recent_ends: RefCell::new(VecDeque::with_capacity(RECENT_READS)),
            anchor: Cell::new(None),
            reads: RefCell::default(),
            position: Cell::new(0),
            last_read: Cell::new(Instant::now()),
            slots: RefCell::default(),
            next_id: Cell::new(0),
            closed: Cell::new(false),
        });
        pool.readers.borrow_mut().push(Rc::downgrade(&reader));
        reader
    }

    /// Begin a read of `[start, start + length)`. Its records are fetched through
    /// read-ahead while fetches are admitted, so no record is fetched twice, and
    /// are neither cancelled nor released until the returned lease drops. A
    /// sequential read also moves the read-ahead window to its start, so the
    /// records ahead are fetched alongside it. A read is sequential when it
    /// continues a recent read or the reader streams. Another read fetches only
    /// the record after it, in case it starts a new stream. A streaming read from
    /// the start of the file also fetches the last record, where containers such
    /// as MP4 and WebM often keep the index a player reads next. A read that
    /// needs no records, such as one of zero length, changes nothing.
    pub(super) fn begin_read(self: &Rc<Self>, start: u64, length: usize) -> ReadLease {
        let needed = self
            .layout
            .chunks_overlapping(start..start.saturating_add(length as u64));
        if self.closed.get() || needed.is_empty() {
            return ReadLease {
                read_ahead: Rc::clone(self),
                read: None,
                indices: HashMap::new(),
                addresses: Vec::new(),
            };
        }
        let read = self.next_id();
        let addresses = needed
            .clone()
            .map(|index| self.layout.address(index))
            .collect::<Vec<_>>();
        let indices = needed
            .clone()
            .zip(addresses.iter().copied())
            .map(|(index, address)| (address, index))
            .collect();
        self.reads.borrow_mut().insert(read, needed.clone());
        self.position.set(needed.start);
        self.last_read.set(Instant::now());
        // A speculative GET already queued for one of these records is now one
        // a read waits for.
        self.pool.awaited.add(&addresses);
        self.pool.budget.notify();
        // Each read retries failed records once, except those reads in
        // progress fetch themselves.
        {
            let reads = self.reads.borrow();
            self.slots.borrow_mut().retain(|index, slot| {
                !matches!(slot, Slot::Failed) || reads.values().any(|read| read.contains(index))
            });
        }
        let sequential = self.streaming || self.recent_ends.borrow().contains(&start);
        if sequential {
            self.anchor.set(Some(start));
        }
        for index in needed.clone() {
            if !self.prefetch(index) {
                break;
            }
        }
        if sequential {
            self.fill();
        } else if needed.end < self.layout.chunk_count() {
            self.prefetch(needed.end);
        }
        if self.streaming && start == 0 {
            if let Some(last) = self.layout.chunk_count().checked_sub(1) {
                self.prefetch(last);
            }
        }
        {
            let mut recent = self.recent_ends.borrow_mut();
            if recent.len() == RECENT_READS {
                recent.pop_front();
            }
            recent.push_back(start.saturating_add(length as u64));
        }
        ReadLease {
            read_ahead: Rc::clone(self),
            read: Some(read),
            indices,
            addresses,
        }
    }

    /// Stop read-ahead, cancel its fetches and release the records it holds.
    pub(super) fn close(&self) {
        self.closed.set(true);
        let slots = std::mem::take(&mut *self.slots.borrow_mut());
        for slot in slots.into_values() {
            if let Slot::Fetching { abort, .. } = slot {
                abort.abort();
            }
        }
    }

    /// Records read-ahead has in flight and holds.
    #[cfg(feature = "test-utils")]
    pub(super) fn usage(&self) -> (usize, usize) {
        (self.fetching(), self.held())
    }

    fn next_id(&self) -> u64 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    fn window(&self) -> Range<usize> {
        self.anchor.get().map_or(0..0, |anchor| {
            self.layout
                .window(anchor, READ_AHEAD_BYTES, MAX_WINDOW_RECORDS)
        })
    }

    /// Whether `index` is in `window` or a read in progress needs it.
    fn keeps(&self, window: &Range<usize>, index: usize) -> bool {
        window.contains(&index)
            || self
                .reads
                .borrow()
                .values()
                .any(|read| read.contains(&index))
    }

    fn is_idle(&self, now: Instant) -> bool {
        self.reads.borrow().is_empty()
            && now.saturating_duration_since(self.last_read.get()) >= IDLE_READER
    }

    fn fetching(&self) -> usize {
        self.slots
            .borrow()
            .values()
            .filter(|slot| matches!(slot, Slot::Fetching { .. }))
            .count()
    }

    fn held(&self) -> usize {
        self.slots
            .borrow()
            .values()
            .filter(|slot| matches!(slot, Slot::Held(_)))
            .count()
    }

    /// Keep up to `READ_AHEAD_CONCURRENCY` fetches running in the window, nearest
    /// first. A record already fetched, held or failed is skipped, so each window
    /// position fetches a record at most once.
    fn fill(self: &Rc<Self>) {
        let window = self.window();
        for index in window.clone() {
            let active = self
                .slots
                .borrow()
                .range(window.clone())
                .filter(|(_, slot)| matches!(slot, Slot::Fetching { .. }))
                .count();
            if active >= READ_AHEAD_CONCURRENCY || !self.prefetch(index) {
                return;
            }
        }
    }

    /// Fetch one record in the background unless read-ahead already has it.
    /// Returns whether read-ahead fetches, holds or failed the record: false once
    /// it is closed or when no fetch can be admitted within its bounds.
    fn prefetch(self: &Rc<Self>, index: usize) -> bool {
        if self.closed.get() {
            return false;
        }
        if self.slots.borrow().contains_key(&index) {
            return true;
        }
        if !self.admit() {
            return false;
        }
        let id = self.next_id();
        let (abort, registration) = AbortHandle::new_pair();
        let address = self.layout.address(index);
        let this = Rc::clone(self);
        let fetch = async move {
            let result = Abortable::new(this.pool.fetch(address), registration).await;
            {
                let mut slots = this.slots.borrow_mut();
                // A cancelled fetch no longer owns the record's slot.
                if matches!(slots.get(&index), Some(Slot::Fetching { id: owner, .. }) if *owner == id)
                {
                    let slot = match result {
                        Ok(Ok(Some(content))) => Slot::Held(content),
                        _ => Slot::Failed,
                    };
                    slots.insert(index, slot);
                }
            }
            this.fill();
        }
        .boxed_local()
        .shared();
        self.slots.borrow_mut().insert(
            index,
            Slot::Fetching {
                id,
                fetch: fetch.clone(),
                abort,
            },
        );
        wasm_bindgen_futures::spawn_local(fetch);
        true
    }

    /// Make room for one more fetch: cancel this reader's stale fetches over the
    /// in-flight bound, then release held records over the client's budget. The
    /// window and the records of reads in progress are neither cancelled nor
    /// released, so admission is refused when they fill a bound.
    fn admit(&self) -> bool {
        self.cancel_stale();
        self.fetching() < MAX_IN_FLIGHT && self.pool.make_room(self)
    }

    /// Cancel fetches outside the window and reads in progress until another
    /// fits within the in-flight bound. The most recently started goes first, as
    /// an earlier fetch has made more progress.
    fn cancel_stale(&self) {
        let window = self.window();
        loop {
            let stale = {
                let slots = self.slots.borrow();
                let fetching = slots.iter().filter_map(|(&index, slot)| match slot {
                    Slot::Fetching { id, .. } => Some((index, *id)),
                    _ => None,
                });
                if fetching.clone().count() < MAX_IN_FLIGHT {
                    return;
                }
                fetching
                    .filter(|&(index, _)| !self.keeps(&window, index))
                    .max_by_key(|&(_, id)| id)
                    .map(|(index, _)| index)
            };
            let Some(Slot::Fetching { abort, .. }) =
                stale.and_then(|index| self.slots.borrow_mut().remove(&index))
            else {
                return;
            };
            abort.abort();
        }
    }

    /// Release up to `count` held records, behind the latest read first, and
    /// return how many were released. An idle reader releases any record;
    /// otherwise the window and the records of reads in progress are kept.
    fn release(&self, count: usize, idle: bool) -> usize {
        let window = if idle { 0..0 } else { self.window() };
        let held = self
            .slots
            .borrow()
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Held(_)))
            .map(|(&index, _)| index)
            .collect::<Vec<_>>();
        let released = releasable(
            self.position.get(),
            held.len().saturating_sub(count),
            &held,
            |index| self.keeps(&window, index),
        );
        let mut slots = self.slots.borrow_mut();
        for index in &released {
            slots.remove(index);
        }
        released.len()
    }
}

/// A read in progress. Read-ahead keeps the records it needs until it drops.
pub(super) struct ReadLease {
    read_ahead: Rc<ReadAhead>,
    read: Option<u64>,
    /// Index of each record the read needs, by address.
    indices: HashMap<[u8; 32], usize>,
    addresses: Vec<[u8; 32]>,
}

impl ReadLease {
    /// The record at `address` once read-ahead's fetch of it settles: `None`
    /// when read-ahead has no fetch of it, so the read fetches it itself. A
    /// fetch that failed is an error, the read's first attempt at the record.
    pub(super) async fn record(&self, address: [u8; 32]) -> Option<crate::data::Result<Bytes>> {
        let index = *self.indices.get(&address)?;
        let fetch = match self.read_ahead.slots.borrow().get(&index)? {
            Slot::Held(content) => return Some(Ok(content.clone())),
            Slot::Fetching { fetch, .. } => fetch.clone(),
            Slot::Failed => return None,
        };
        fetch.await;
        match self.read_ahead.slots.borrow().get(&index)? {
            Slot::Held(content) => Some(Ok(content.clone())),
            Slot::Failed => Some(Err(Error::NotFound(format!(
                "read-ahead could not fetch record {}",
                hex::encode(address)
            )))),
            Slot::Fetching { .. } => None,
        }
    }
}

impl Drop for ReadLease {
    fn drop(&mut self) {
        if let Some(read) = self.read {
            self.read_ahead.reads.borrow_mut().remove(&read);
            self.read_ahead.pool.awaited.remove(&self.addresses);
            self.read_ahead.last_read.set(Instant::now());
        }
    }
}
