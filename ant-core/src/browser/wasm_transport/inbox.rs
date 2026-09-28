//! Bounded ingress credits for ordered encrypted response frames.

use super::WEBRTC_WRITE_CHUNK_BYTES;
use futures_channel::mpsc;
use futures_util::{lock::Mutex, StreamExt as _};
use js_sys::{ArrayBuffer, Uint8Array};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use wasm_bindgen::{JsCast as _, JsValue};

// The byte budget bounds payload allocations; this separate message cap also
// bounds queue overhead when a peer fragments a response into tiny messages.
const MAX_QUEUED_MESSAGES: usize = 512;

pub(super) struct ReceivedMessage {
    pub(super) bytes: Vec<u8>,
    pub(super) received_at: web_time::Instant,
}

pub(super) struct ResponseInbox {
    sender: RefCell<mpsc::Sender<ReceivedMessage>>,
    receiver: Mutex<mpsc::Receiver<ReceivedMessage>>,
    expected: Cell<usize>,
    queued_bytes: Cell<usize>,
    credits: Cell<usize>,
    multiplex: Cell<bool>,
    prefix: RefCell<Vec<u8>>,
    frame_remaining: Cell<Option<usize>>,
    max_payload: Cell<usize>,
    transfer: Cell<Option<(web_time::Instant, std::time::Duration)>>,
    queued: Cell<usize>,
    failure: RefCell<Option<String>>,
}

impl ResponseInbox {
    pub(super) fn new() -> Rc<Self> {
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_MESSAGES * 4);
        Rc::new(Self {
            sender: RefCell::new(sender),
            receiver: Mutex::new(receiver),
            expected: Cell::new(0),
            queued_bytes: Cell::new(0),
            credits: Cell::new(0),
            multiplex: Cell::new(false),
            prefix: RefCell::new(Vec::with_capacity(4)),
            frame_remaining: Cell::new(None),
            max_payload: Cell::new(0),
            transfer: Cell::new(None),
            queued: Cell::new(0),
            failure: RefCell::new(None),
        })
    }

    pub(super) fn set_transfer(&self, started: web_time::Instant, budget: std::time::Duration) {
        self.transfer.set(Some((started, budget)));
    }

    pub(super) fn transfer_remaining(&self) -> Option<std::time::Duration> {
        self.transfer
            .get()
            .map(|(started, budget)| budget.saturating_sub(started.elapsed()))
            .filter(|remaining| !remaining.is_zero())
    }

    pub(super) fn enable_multiplex(&self) {
        self.multiplex.set(true);
    }

    /// Enable ingress before sending a request, since the response can begin
    /// arriving while the outgoing DataChannel buffer is still draining.
    pub(super) fn expect_response(&self, max_payload_bytes: usize) -> Result<(), String> {
        self.check_failure()?;
        if !self.multiplex.get() && (self.expected.get() != 0 || self.queued.get() != 0) {
            return Err("WebRTC response inbox is not idle".into());
        }
        if self.expected.get() >= 4 {
            return Err("too many outstanding WebRTC responses".into());
        }
        self.expected.set(self.expected.get() + 1);
        self.credits.set(self.credits.get() + 1);
        self.max_payload.set(max_payload_bytes);

        Ok(())
    }

    /// Validate the browser-owned view before copying any bytes into WASM.
    /// Frame lengths consume ingress credits independently of queue draining;
    /// payload and message counts are bounded before allocating WASM buffers.
    pub(super) fn push(&self, data: JsValue) -> Result<(), String> {
        self.check_failure()?;
        let view = if data.is_instance_of::<ArrayBuffer>() {
            Uint8Array::new(&data)
        } else if data.is_instance_of::<Uint8Array>() {
            data.unchecked_into::<Uint8Array>()
        } else {
            return Err("node sent a non-binary DataChannel message".to_string());
        };
        let size = view.length() as usize;
        if size == 0 || size > WEBRTC_WRITE_CHUNK_BYTES {
            return Err(format!("invalid WebRTC response message size {size}"));
        }
        let queued_bytes = self
            .queued_bytes
            .get()
            .checked_add(size)
            .filter(|bytes| *bytes <= (self.max_payload.get() + 4) * 4)
            .ok_or_else(|| "WebRTC response exceeded its byte budget".to_string())?;
        // Check queue capacity before allocating too. Do not clone the sender:
        // each clone would reserve another slot in futures-channel's queue.
        // Parse only the four-byte length before copying. Credits end at each
        // declared frame boundary, so a spare large-frame allowance cannot be
        // used to smuggle unsolicited extra replies into the queue.
        if self.credits.get() == 0 {
            return Err("node sent unsolicited WebRTC response data".into());
        }
        let mut offset = 0;
        if self.frame_remaining.get().is_none() {
            let mut prefix = self.prefix.borrow_mut();
            while prefix.len() < 4 && offset < size {
                prefix.push(view.get_index(offset as u32));
                offset += 1;
            }
            if prefix.len() == 4 {
                let length = u32::from_be_bytes(prefix.as_slice().try_into().unwrap()) as usize;
                if length == 0 || length > self.max_payload.get() {
                    return Err("WebRTC response exceeded its byte budget".into());
                }
                self.frame_remaining.set(Some(length));
                prefix.clear();
            }
        }
        if let Some(left) = self.frame_remaining.get() {
            let left = left
                .checked_sub(size - offset)
                .ok_or_else(|| "PQ frame contains bytes after its declared payload".to_string())?;
            self.frame_remaining.set((left != 0).then_some(left));
            if left == 0 {
                self.credits.set(self.credits.get() - 1);
            }
        }
        let queue_limit = MAX_QUEUED_MESSAGES * if self.multiplex.get() { 4 } else { 1 };
        if self.queued.get() >= queue_limit {
            return Err("WebRTC response inbox has too many messages".to_string());
        }
        self.sender
            .borrow_mut()
            .try_send(ReceivedMessage {
                bytes: view.to_vec(),
                received_at: web_time::Instant::now(),
            })
            .map_err(|_| "WebRTC response inbox is full or closed".to_string())?;
        self.queued_bytes.set(queued_bytes);
        self.queued.set(self.queued.get() + 1);
        Ok(())
    }

    pub(super) async fn next(&self) -> Result<ReceivedMessage, String> {
        self.check_failure()?;
        let next = self.receiver.lock().await.next().await;
        self.check_failure()?;
        let message = next
            .ok_or_else(|| "response ended before its declared frame was complete".to_string())?;
        self.queued.set(self.queued.get() - 1);
        self.queued_bytes
            .set(self.queued_bytes.get() - message.bytes.len());
        Ok(message)
    }

    pub(super) fn finish_response(&self) -> Result<(), String> {
        self.check_failure()?;
        self.transfer.set(None);
        self.expected.set(
            self.expected
                .get()
                .checked_sub(1)
                .ok_or_else(|| "unsolicited WebRTC response".to_string())?,
        );
        if !self.multiplex.get() && self.queued.get() != 0 {
            return Err("node sent data after its complete WebRTC response".to_string());
        }
        Ok(())
    }

    /// Wake a waiting reader and retain only the first terminal error.
    pub(super) fn fail(&self, error: String) {
        if self.failure.borrow().is_none() {
            self.failure.replace(Some(error));
        }
        self.expected.set(0);
        self.credits.set(0);
        self.sender.borrow_mut().close_channel();
        if let Some(mut receiver) = self.receiver.try_lock() {
            while receiver.try_recv().is_ok() {}
            self.queued.set(0);
            self.queued_bytes.set(0);
        }
    }

    /// Whether the inbox has failed, without draining it as `check_failure` does.
    pub(super) fn is_failed(&self) -> bool {
        self.failure.borrow().is_some()
    }

    pub(super) fn check_failure(&self) -> Result<(), String> {
        let failure = self.failure.borrow().clone();
        match failure {
            Some(error) => {
                // A reader may have held the queue lock when failure arrived.
                // Drain it after wakeup before releasing the response budget.
                self.fail(error.clone());
                Err(error)
            }
            None => Ok(()),
        }
    }
}
