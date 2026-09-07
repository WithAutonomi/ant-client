//! Bounded ingress for one outstanding encrypted response.

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

pub(super) struct ResponseInbox {
    sender: RefCell<mpsc::Sender<Vec<u8>>>,
    receiver: Mutex<mpsc::Receiver<Vec<u8>>>,
    remaining: Cell<Option<usize>>,
    queued: Cell<usize>,
    failure: RefCell<Option<String>>,
}

impl ResponseInbox {
    pub(super) fn new() -> Rc<Self> {
        let (sender, receiver) = mpsc::channel(MAX_QUEUED_MESSAGES);
        Rc::new(Self {
            sender: RefCell::new(sender),
            receiver: Mutex::new(receiver),
            remaining: Cell::new(None),
            queued: Cell::new(0),
            failure: RefCell::new(None),
        })
    }

    /// Enable ingress before sending a request, since the response can begin
    /// arriving while the outgoing DataChannel buffer is still draining.
    pub(super) fn expect_response(&self, max_payload_bytes: usize) -> Result<(), String> {
        self.check_failure()?;
        if self.remaining.get().is_some() || self.queued.get() != 0 {
            return Err("WebRTC response inbox is not idle".to_string());
        }
        let frame_bytes = max_payload_bytes
            .checked_add(4)
            .ok_or_else(|| "PQ frame limit overflow".to_string())?;
        self.remaining.set(Some(frame_bytes));
        Ok(())
    }

    /// Validate the browser-owned view before copying any bytes into WASM.
    /// The allowance covers the entire response, including messages that the
    /// consumer already removed, so draining the queue cannot reset the limit.
    pub(super) fn push(&self, data: JsValue) -> Result<(), String> {
        self.check_failure()?;
        let remaining = self
            .remaining
            .get()
            .ok_or_else(|| "node sent unsolicited WebRTC response data".to_string())?;
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
        let remaining = remaining
            .checked_sub(size)
            .ok_or_else(|| "WebRTC response exceeded its byte budget".to_string())?;
        // Check queue capacity before allocating too. Do not clone the sender:
        // each clone would reserve another slot in futures-channel's queue.
        if self.queued.get() >= MAX_QUEUED_MESSAGES {
            return Err("WebRTC response inbox has too many messages".to_string());
        }
        self.sender
            .borrow_mut()
            .try_send(view.to_vec())
            .map_err(|_| "WebRTC response inbox is full or closed".to_string())?;
        self.remaining.set(Some(remaining));
        self.queued.set(self.queued.get() + 1);
        Ok(())
    }

    pub(super) async fn next(&self) -> Result<Vec<u8>, String> {
        self.check_failure()?;
        let next = self.receiver.lock().await.next().await;
        self.check_failure()?;
        let message = next
            .ok_or_else(|| "response ended before its declared frame was complete".to_string())?;
        self.queued.set(self.queued.get() - 1);
        Ok(message)
    }

    pub(super) fn finish_response(&self) -> Result<(), String> {
        self.check_failure()?;
        self.remaining.set(None);
        if self.queued.get() != 0 {
            return Err("node sent data after its complete WebRTC response".to_string());
        }
        Ok(())
    }

    /// Wake a waiting reader and retain only the first terminal error.
    pub(super) fn fail(&self, error: String) {
        if self.failure.borrow().is_some() {
            return;
        }
        self.failure.replace(Some(error));
        self.remaining.set(None);
        self.sender.borrow_mut().close_channel();
        if let Some(mut receiver) = self.receiver.try_lock() {
            while receiver.try_recv().is_ok() {}
            self.queued.set(0);
        }
    }

    fn check_failure(&self) -> Result<(), String> {
        match self.failure.borrow().as_ref() {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
