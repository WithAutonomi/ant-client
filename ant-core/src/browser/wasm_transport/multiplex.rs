//! One authenticated session, bounded RPCs, and a single ordered response reader.
//!
//! Cancellation drops the caller, not an admitted exchange. Its slot and read
//! reservation remain owned until the authenticated reply is drained or the
//! ordinary transport deadline closes the session.
use super::*;
use crate::client_engine::read_budget::ReadPermit;
use tokio::sync::Semaphore;

pub(super) const CAPABILITY: &str = "rpc-multiplex-4";
const MAX_REQUESTS: usize = 4;
type Reply = Result<(BrowserResponseFrame, Duration), RpcError>;

pub(super) struct RpcSession {
    channel: RtcDataChannel,
    inbox: Rc<ResponseInbox>,
    pq: Rc<RefCell<Option<PqSession>>>,
    slots: std::sync::Arc<Semaphore>,
    send: Mutex<()>,
    pending: RefCell<HashMap<u64, oneshot::Sender<Reply>>>,
    changed: watch::Sender<()>,
    multiplex: Cell<bool>,
    closed: Cell<bool>,
}

impl RpcSession {
    pub(super) fn new(
        channel: RtcDataChannel,
        inbox: Rc<ResponseInbox>,
        pq: Rc<RefCell<Option<PqSession>>>,
    ) -> Rc<Self> {
        let (changed, _) = watch::channel(());
        let session = Rc::new(Self {
            channel,
            inbox,
            pq,
            slots: std::sync::Arc::new(Semaphore::new(1)),
            send: Mutex::new(()),
            pending: RefCell::new(HashMap::new()),
            changed,
            multiplex: Cell::new(false),
            closed: Cell::new(false),
        });
        let reader = Rc::clone(&session);
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(error) = reader.read_responses().await {
                reader.close(error.to_string());
            }
        });
        session
    }

    pub(super) fn enable_multiplex(&self) {
        if !self.multiplex.replace(true) {
            self.inbox.enable_multiplex();
            self.slots.add_permits(MAX_REQUESTS - 1);
        }
    }

    pub(super) fn close(&self, error: String) {
        if self.closed.replace(true) {
            return;
        }
        self.slots.close();
        self.inbox.fail(error.clone());
        self.channel.close();
        for (_, sender) in self.pending.take() {
            let _ = sender.send(Err(error.clone().into()));
        }
        self.changed.send_replace(());
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.get()
    }

    pub(super) async fn admit(
        &self,
        timeout: Duration,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, RpcError> {
        crate::runtime::timeout(timeout, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| RpcError::Timeout("WebRTC admission timed out waiting for peer".into()))?
            .map_err(|_| "WebRTC session closed".to_string().into())
    }

    pub(super) async fn request(
        self: &Rc<Self>,
        id: u64,
        plaintext: Vec<u8>,
        timeout: Duration,
        read_permit: Option<ReadPermit>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) -> Result<(BrowserResponseFrame, Duration, Option<ReadPermit>), RpcError> {
        let permit = match permit {
            Some(permit) => permit,
            None => self.admit(RPC_ADMISSION_TIMEOUT).await?,
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
        }
    }
}
