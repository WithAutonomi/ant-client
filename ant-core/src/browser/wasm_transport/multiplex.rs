//! One authenticated session, bounded RPCs, and a single ordered response reader.
//!
//! Cancellation drops the caller, not an admitted exchange. Its slot and read
//! reservation remain owned until the authenticated reply is drained or the
//! ordinary transport deadline closes the session.
//!
//! A speculative GET, one no read waits for yet such as read-ahead, is admitted
//! only while speculative GETs hold less than a share of a session's slots, and
//! never takes a slot another request is queued for. A GET a read waits for, such
//! as a seek's, therefore never waits behind read-ahead that comes after it.
//!
//! Like the read budget's permits, a GET is classed once: when it queues as a
//! read's, or when it is admitted. A GET whose read stops waiting for it, such as
//! one cancelled by a seek, keeps its slot as a read's until its exchange
//! completes, so read-ahead may hold more than the share for that long.
use super::*;
use crate::client_engine::read_budget::{ReadPermit, SPECULATIVE_SHARE_DIVISOR};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

pub(super) const CAPABILITY: &str = "rpc-multiplex-4";
/// Requests one multiplexed session carries at once.
const MAX_REQUESTS: usize = 4;
type Reply = Result<(BrowserResponseFrame, Duration), RpcError>;

/// How a GET competes for a session's slots. It is speculative while no read
/// waits for its record, and is asked again whenever `changed` changes.
pub(super) struct ReadPriority<'a> {
    pub(super) speculative: &'a dyn Fn() -> bool,
    pub(super) changed: watch::Receiver<()>,
}

/// A session slot an admitted request holds until its exchange completes.
pub(super) struct SessionSlot {
    permit: Option<OwnedSemaphorePermit>,
    session: Rc<RpcSession>,
    speculative: bool,
}

impl Drop for SessionSlot {
    fn drop(&mut self) {
        // Release the slot before waking those waiting for one.
        drop(self.permit.take());
        if self.speculative {
            self.session
                .speculative
                .set(self.session.speculative.get() - 1);
        }
        self.session.released.send_replace(());
    }
}

/// Wakes those waiting on `released` when dropped. An exclusive request's
/// wait for every slot that times out or is cancelled hands back the permits it
/// had gathered, and no `SessionSlot` release reports them.
struct WakeOnDrop<'a>(&'a watch::Sender<()>);

impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.send_replace(());
    }
}

/// Resolves on the next change. A closed channel never changes again.
async fn next_change(receiver: &mut watch::Receiver<()>) {
    if receiver.changed().await.is_err() {
        futures::future::pending::<()>().await;
    }
}

pub(super) struct RpcSession {
    channel: RtcDataChannel,
    inbox: Rc<ResponseInbox>,
    pq: Rc<RefCell<Option<PqSession>>>,
    slots: std::sync::Arc<Semaphore>,
    /// Slots held by speculative GETs.
    speculative: Cell<usize>,
    /// Changes when a slot is released or added, or the session closes.
    released: watch::Sender<()>,
    send: Mutex<()>,
    pending: RefCell<HashMap<u64, oneshot::Sender<Reply>>>,
    changed: watch::Sender<()>,
    multiplex: Cell<bool>,
    closed: Cell<bool>,
    pool: RefCell<Option<Weak<PoolAvailability>>>,
}

impl RpcSession {
    pub(super) fn new(
        channel: RtcDataChannel,
        inbox: Rc<ResponseInbox>,
        pq: Rc<RefCell<Option<PqSession>>>,
    ) -> Rc<Self> {
        let (changed, _) = watch::channel(());
        let (released, _) = watch::channel(());
        let session = Rc::new(Self {
            channel,
            inbox,
            pq,
            slots: std::sync::Arc::new(Semaphore::new(1)),
            speculative: Cell::new(0),
            released,
            send: Mutex::new(()),
            pending: RefCell::new(HashMap::new()),
            changed,
            multiplex: Cell::new(false),
            closed: Cell::new(false),
            pool: RefCell::new(None),
        });
        let reader = Rc::clone(&session);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = reader.read_responses().await {
                reader.close(error.to_string());
            }
        });
        session
    }

    pub(super) fn set_pool(&self, pool: Option<&Rc<PoolAvailability>>) {
        self.pool.replace(pool.map(Rc::downgrade));
    }

    pub(super) fn is_busy(&self) -> bool {
        !self.pending.borrow().is_empty()
    }

    fn notify_pool(&self) {
        if let Some(pool) = self.pool.borrow().as_ref().and_then(Weak::upgrade) {
            pool.notify_waiters();
        }
    }

    pub(super) fn enable_multiplex(&self) {
        if !self.multiplex.replace(true) {
            self.inbox.enable_multiplex();
            self.slots.add_permits(MAX_REQUESTS - 1);
            self.released.send_replace(());
        }
    }

    pub(super) fn close(&self, error: String) {
        if self.closed.replace(true) {
            return;
        }
        self.slots.close();
        self.released.send_replace(());
        self.inbox.fail(error.clone());
        self.channel.close();
        for (_, sender) in self.pending.take() {
            let _ = sender.send(Err(error.clone().into()));
        }
        self.changed.send_replace(());
        self.notify_pool();
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    /// Requests the session carries at once.
    fn capacity(&self) -> usize {
        if self.multiplex.get() {
            MAX_REQUESTS
        } else {
            1
        }
    }

    fn slot(self: &Rc<Self>, permit: OwnedSemaphorePermit, speculative: bool) -> SessionSlot {
        self.speculative
            .set(self.speculative.get() + usize::from(speculative));
        SessionSlot {
            permit: Some(permit),
            session: Rc::clone(self),
            speculative,
        }
    }

    pub(super) async fn admit(self: &Rc<Self>, timeout: Duration) -> Result<SessionSlot, RpcError> {
        crate::runtime::timeout(timeout, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| RpcError::Timeout("WebRTC admission timed out waiting for peer".into()))?
            .map(|permit| self.slot(permit, false))
            .map_err(|_| "WebRTC session closed".to_string().into())
    }

    /// Admit a GET. One a read waits for queues for a slot like any request.
    /// A speculative one takes a free slot only within the session's
    /// speculative share and only while no request is queued for it, and is
    /// asked again whenever a slot is released or `read.changed` changes, so
    /// a GET a read comes to wait for is admitted as one.
    pub(super) async fn admit_read(
        self: &Rc<Self>,
        timeout: Duration,
        read: &ReadPriority<'_>,
    ) -> Result<SessionSlot, RpcError> {
        let mut released = self.released.subscribe();
        let mut changed = read.changed.clone();
        let admission = async {
            loop {
                // A speculative GET over its share never tries the slots,
                // which is how a closed session would otherwise fail it.
                if self.is_closed() {
                    return Err("WebRTC session closed".to_string().into());
                }
                if !(read.speculative)() {
                    return self
                        .slots
                        .clone()
                        .acquire_owned()
                        .await
                        .map(|permit| self.slot(permit, false))
                        .map_err(|_| "WebRTC session closed".to_string().into());
                }
                // At least one slot: a session without multiplexing has one,
                // which read-ahead may then hold.
                let share = (self.capacity() / SPECULATIVE_SHARE_DIVISOR).max(1);
                if self.speculative.get() < share {
                    match self.slots.clone().try_acquire_owned() {
                        Ok(permit) => return Ok(self.slot(permit, true)),
                        Err(TryAcquireError::Closed) => {
                            return Err("WebRTC session closed".to_string().into())
                        }
                        Err(TryAcquireError::NoPermits) => {}
                    }
                }
                select(
                    Box::pin(next_change(&mut released)),
                    Box::pin(next_change(&mut changed)),
                )
                .await;
            }
        };
        crate::runtime::timeout(timeout, admission)
            .await
            .map_err(|_| RpcError::Timeout("WebRTC admission timed out waiting for peer".into()))?
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn request(
        self: &Rc<Self>,
        id: u64,
        plaintext: Vec<u8>,
        timeout: Duration,
        read_permit: Option<ReadPermit>,
        permit: Option<SessionSlot>,
        exclusive: bool,
    ) -> Result<(BrowserResponseFrame, Duration, Option<ReadPermit>), RpcError> {
        // Preserve per-lane PUT serialization: several complete incoming PUTs
        // would consume the node's byte budget before any one can finish. Give
        // up ordinary admission before acquiring all slots to avoid upgrades
        // deadlocking when several callers already hold one slot each.
        let permit = if exclusive {
            drop(permit);
            let count = self.capacity() as u32;
            // However the wait ends, a speculative GET waiting on `released`
            // learns of any permits it hands back.
            let _wake = WakeOnDrop(&self.released);
            let permit = crate::runtime::timeout(
                RPC_ADMISSION_TIMEOUT,
                self.slots.clone().acquire_many_owned(count),
            )
            .await
            .map_err(|_| RpcError::Timeout("WebRTC write admission timed out".into()))?
            .map_err(|_| "WebRTC session closed".to_string())?;
            self.slot(permit, false)
        } else {
            match permit {
                Some(permit) => permit,
                None => self.admit(RPC_ADMISSION_TIMEOUT).await?,
            }
        };
        let (mut sender, receiver) = oneshot::channel();
        let session = Rc::clone(self);
        wasm_bindgen_futures::spawn_local(async move {
            let _permit = permit;
            // Unsent queued work is cancellable. Once any frame bytes may have
            // been sent, own the exchange through completion independent of caller.
            let lock = match select(
                Box::pin(crate::runtime::timeout(
                    RPC_ADMISSION_TIMEOUT,
                    session.send.lock(),
                )),
                Box::pin(sender.cancellation()),
            )
            .await
            {
                Either::Left((Ok(lock), _)) => lock,
                Either::Left((Err(_), _)) => {
                    let _ = sender.send(Err(RpcError::Timeout(
                        "WebRTC admission timed out waiting for sender".into(),
                    )));
                    return;
                }
                Either::Right(_) => return,
            };
            if sender.is_canceled() || session.closed.get() {
                return;
            }
            let result = session.exchange(id, plaintext, timeout, lock).await;
            let _ = sender
                .send(result.map(|(response, processing)| (response, processing, read_permit)));
        });
        receiver
            .await
            .map_err(|_| "WebRTC session closed".to_string())?
    }

    async fn exchange(
        &self,
        id: u64,
        plaintext: Vec<u8>,
        timeout: Duration,
        send_lock: MutexGuard<'_, ()>,
    ) -> Reply {
        let (sender, mut receiver) = oneshot::channel();
        self.pending.borrow_mut().insert(id, sender);
        let frame = (|| {
            self.inbox
                .expect_response(MAX_BROWSER_RESPONSE_BYTES + PQ_ENCRYPTED_OVERHEAD_BYTES)?;
            let encrypted = self
                .pq
                .borrow_mut()
                .as_mut()
                .ok_or("WebRTC PQ session unavailable")?
                .seal(&plaintext)
                .map_err(|e| e.to_string())?;
            encode_pq_frame(&encrypted).map_err(|e| e.to_string())
        })();
        drop(plaintext);
        let frame = match frame {
            Ok(frame) => frame,
            Err(error) => {
                self.close(error.clone());
                return Err(error.into());
            }
        };
        self.changed.send_replace(());
        if let Err(error) =
            send_data_channel_frame(&self.channel, &frame, transfer_timeout_ms(frame.len())).await
        {
            let error = self
                .inbox
                .check_failure()
                .err()
                .map(RpcError::Transport)
                .unwrap_or(error);
            self.close(error.to_string());
            return Err(error);
        }
        drop(frame);
        drop(send_lock);
        // Each RPC gets its own allowance after its outgoing frame drains.
        let ceiling = TransferDeadline::new(
            timeout + transfer_timeout(MAX_BROWSER_RESPONSE_BYTES) * MAX_REQUESTS as u32,
        );
        let mut wait = timeout;
        loop {
            match crate::runtime::timeout(wait, &mut receiver).await {
                Ok(Ok(result)) => return result,
                Ok(Err(_)) => return Err("WebRTC response dispatcher closed".to_string().into()),
                Err(_) => {
                    // A started response uses its size-derived transfer budget.
                    // Its request ID is encrypted until the whole frame arrives.
                    if let Some(remaining) = self.inbox.transfer_remaining() {
                        if !ceiling.remaining().is_zero() {
                            wait = remaining.min(ceiling.remaining());
                            continue;
                        }
                    }
                    let error = RpcError::Timeout("WebRTC response timed out".into());
                    self.close(error.to_string());
                    return Err(error);
                }
            }
        }
    }

    async fn read_responses(&self) -> Result<(), RpcError> {
        let mut changed = self.changed.subscribe();
        loop {
            while self.pending.borrow().is_empty() && !self.closed.get() {
                changed
                    .changed()
                    .await
                    .map_err(|_| "WebRTC session closed".to_string())?;
            }
            if self.closed.get() {
                return Ok(());
            }
            // Per-RPC timers handle first-response waiting. The reader still
            // enforces the ordinary size-derived deadline once a frame starts.
            let encrypted = read_pq_payload_typed(
                Rc::clone(&self.inbox),
                MAX_BROWSER_RESPONSE_BYTES + PQ_ENCRYPTED_OVERHEAD_BYTES,
                RPC_ADMISSION_TIMEOUT.as_millis() as u32,
            )
            .await?;
            let started = web_time::Instant::now();
            let plaintext = self
                .pq
                .borrow_mut()
                .as_mut()
                .ok_or("WebRTC PQ session unavailable".to_string())?
                .open(&encrypted)
                .map_err(|e| e.to_string())?;
            let response = parse_response_frame(&plaintext).map_err(|e| e.to_string())?;
            let id = response.header.request_id;
            let sender = self
                .pending
                .borrow_mut()
                .remove(&id)
                .ok_or_else(|| format!("unsolicited or duplicate response ID {id}"))?;
            let _ = sender.send(Ok((response, started.elapsed())));
            self.notify_pool();
        }
    }
}
