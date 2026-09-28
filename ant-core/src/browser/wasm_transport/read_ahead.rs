//! Sequential read-ahead for the browser file reader. Media playback reads a file
//! front to back in small ranges. Fetching the next records in parallel overlaps
//! each record's discovery with playback, instead of stalling playback on it.
//! The reader holds the records it fetches within its own budget, so neither the
//! client's shared chunk cache nor another reader can evict them before use.
use crate::client_engine::files::RecordLayout;
use bytes::Bytes;
use futures::future::{join_all, AbortHandle, Abortable, FutureExt, LocalBoxFuture, Shared};
use self_encryption::{DataMap, MAX_CHUNK_SIZE};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ops::Range;
use std::rc::Rc;

/// Plaintext fetched ahead of a sequential read: about a minute of 5 Mbit/s video.
const READ_AHEAD_BYTES: usize = 32 * 1024 * 1024;
/// Records fetched in parallel within the read-ahead window.
const READ_AHEAD_CONCURRENCY: usize = 4;
/// Records in flight in total: the window's fetches plus one left behind by a
/// seek, such as the start of a file that a player returns to after reading the
/// index at its end. A new fetch cancels stale ones beyond this bound, so they
/// hold little of the client's read budget when a seek needs it.
const MAX_IN_FLIGHT: usize = READ_AHEAD_CONCURRENCY + 1;
const _: () = assert!(
    MAX_IN_FLIGHT <= super::MAX_READ_RESPONSE_MEMORY / super::READ_RESPONSE_RESERVATION,
    "every fetch in flight must be able to run a GET within the read budget"
);
/// Records a reader holds, about 100 MB, before releasing some. Covers the
/// window, the last record and recent records for short seeks back.
const MAX_HELD_RECORDS: usize = 24;
/// Most full-size records a window overlaps, when it starts on a record's last byte.
const MAX_WINDOW_RECORDS: usize = (READ_AHEAD_BYTES - 1) / MAX_CHUNK_SIZE + 2;
const _: () = assert!(
    MAX_HELD_RECORDS > MAX_WINDOW_RECORDS + 1,
    "held records must cover the window and the last record"
);
/// Recent read ends a new read may continue. Media elements interleave the
/// sequential stream with index reads, such as an MP4 `moov` at the end.
const RECENT_READS: usize = 4;

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

pub(super) struct ReadAhead {
    shared: Rc<crate::data::Client>,
    layout: RecordLayout,
    /// Every read starts a sequential stream, as media playback's reads do.
    streaming: bool,
    /// Ends of recent reads; a read starting at one continues it.
    recent_ends: RefCell<VecDeque<usize>>,
    /// Start of the latest sequential read; the read-ahead window starts here.
    anchor: Cell<Option<usize>>,
    /// Records the latest read needs. Like the window, they are never cancelled
    /// or released.
    reading: RefCell<Range<usize>>,
    slots: RefCell<BTreeMap<usize, Slot>>,
    next_id: Cell<u64>,
    closed: Cell<bool>,
}

impl ReadAhead {
    pub(super) fn new(
        shared: Rc<crate::data::Client>,
        root: &DataMap,
        streaming: bool,
    ) -> Result<Rc<Self>, String> {
        Ok(Rc::new(Self {
            shared,
            layout: RecordLayout::new(root)?,
            streaming,
            recent_ends: RefCell::new(VecDeque::with_capacity(RECENT_READS)),
            anchor: Cell::new(None),
            reading: RefCell::new(0..0),
            slots: RefCell::default(),
            next_id: Cell::new(0),
            closed: Cell::new(false),
        }))
    }

    /// Before a read, fetch the records it needs through read-ahead, so no record
    /// is fetched twice. A sequential read also moves the read-ahead window to its
    /// start, so the records ahead are fetched alongside it. A read is sequential
    /// when it continues a recent read or the reader streams. Another read fetches
    /// only the record after it, in case it starts a new stream. A streaming read
    /// from the start of the file also fetches the last record, where containers
    /// such as MP4 and WebM often keep the index a player reads next.
    ///
    /// Returns the needed records read-ahead holds once their fetches settle. The
    /// read fetches the others itself.
    pub(super) async fn before_read(
        self: &Rc<Self>,
        start: usize,
        length: usize,
    ) -> HashMap<[u8; 32], Bytes> {
        let needed = self.layout.overlapping(start, length);
        // Each read retries failed records once, except those it fetches itself.
        self.slots
            .borrow_mut()
            .retain(|index, slot| !matches!(slot, Slot::Failed) || needed.contains(index));
        let sequential = self.streaming || self.recent_ends.borrow().contains(&start);
        if sequential {
            self.anchor.set(Some(start));
        }
        self.reading.replace(needed.clone());
        for index in needed.clone() {
            self.prefetch(index);
        }
        if sequential {
            self.fill();
        } else if needed.end < self.layout.len() {
            self.prefetch(needed.end);
        }
        if self.streaming && start == 0 {
            if let Some(last) = self.layout.len().checked_sub(1) {
                self.prefetch(last);
            }
        }
        {
            let mut recent = self.recent_ends.borrow_mut();
            if recent.len() == RECENT_READS {
                recent.pop_front();
            }
            recent.push_back(start.saturating_add(length));
        }
        let pending = {
            let slots = self.slots.borrow();
            needed
                .clone()
                .filter_map(|index| match slots.get(&index) {
                    Some(Slot::Fetching { fetch, .. }) => Some(fetch.clone()),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        join_all(pending).await;
        let slots = self.slots.borrow();
        needed
            .filter_map(|index| match slots.get(&index) {
                Some(Slot::Held(content)) => Some((self.layout.address(index), content.clone())),
                _ => None,
            })
            .collect()
    }

    /// After a read, release held records beyond the budget, behind the reader
    /// first. The window and the latest read's records are kept.
    pub(super) fn after_read(&self, start: usize) {
        let window = self.window();
        let reading = self.reading.borrow().clone();
        let mut slots = self.slots.borrow_mut();
        let held = slots
            .iter()
            .filter(|(_, slot)| matches!(slot, Slot::Held(_)))
            .map(|(&index, _)| index)
            .collect::<Vec<_>>();
        let kept = |index| window.contains(&index) || reading.contains(&index);
        for index in self.layout.releasable(start, MAX_HELD_RECORDS, &held, kept) {
            slots.remove(&index);
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

    fn window(&self) -> Range<usize> {
        self.anchor.get().map_or(0..0, |anchor| {
            self.layout.overlapping(anchor, READ_AHEAD_BYTES)
        })
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
            if self.closed.get() || active >= READ_AHEAD_CONCURRENCY {
                return;
            }
            self.prefetch(index);
        }
    }

    /// Fetch one record in the background, unless read-ahead already has it.
    fn prefetch(self: &Rc<Self>, index: usize) {
        if self.closed.get() || self.slots.borrow().contains_key(&index) {
            return;
        }
        self.cancel_stale();
        let id = self.next_id.replace(self.next_id.get() + 1);
        let (abort, registration) = AbortHandle::new_pair();
        let address = self.layout.address(index);
        let this = Rc::clone(self);
        let fetch = async move {
            let result =
                Abortable::new(this.shared.chunk_get_observed(&address), registration).await;
            {
                let mut slots = this.slots.borrow_mut();
                // A cancelled fetch no longer owns the record's slot.
                if matches!(slots.get(&index), Some(Slot::Fetching { id: owner, .. }) if *owner == id)
                {
                    let slot = match result {
                        Ok(Ok(Some(chunk))) => Slot::Held(chunk.content),
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
    }

    /// Before a new fetch, cancel fetches outside the window and the latest read
    /// until it fits within the in-flight bound. The most recently started goes
    /// first, as an earlier fetch has made more progress.
    fn cancel_stale(&self) {
        let window = self.window();
        let reading = self.reading.borrow().clone();
        let mut slots = self.slots.borrow_mut();
        loop {
            let fetching = slots.iter().filter_map(|(&index, slot)| match slot {
                Slot::Fetching { id, .. } => Some((index, *id)),
                _ => None,
            });
            if fetching.clone().count() < MAX_IN_FLIGHT {
                return;
            }
            let stale = fetching
                .filter(|(index, _)| !window.contains(index) && !reading.contains(index))
                .max_by_key(|&(_, id)| id)
                .map(|(index, _)| index);
            let Some(Slot::Fetching { abort, .. }) = stale.and_then(|index| slots.remove(&index))
            else {
                return;
            };
            abort.abort();
        }
    }
}
