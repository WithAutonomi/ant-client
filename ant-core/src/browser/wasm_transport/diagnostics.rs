//! Opt-in live-network timing for test builds. No callbacks or peer/address
//! logging are included in production bindings.
use super::*;

thread_local! {
    static CALLBACK: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    static NEXT_ID: Cell<u32> = const { Cell::new(0) };
}

#[wasm_bindgen(js_name = setBrowserTrace)]
pub fn set_browser_trace(callback: Option<js_sys::Function>) {
    CALLBACK.with(|current| *current.borrow_mut() = callback);
}

pub(super) struct Trace {
    id: u32,
    operation: &'static str,
    target: String,
    peer: String,
    started: web_time::Instant,
    finished: bool,
}

impl Trace {
    pub(super) fn new(operation: &'static str, target: String, peer: String) -> Self {
        let trace = Self {
            id: NEXT_ID.with(|id| {
                id.set(id.get().wrapping_add(1));
                id.get()
            }),
            operation,
            target,
            peer,
            started: web_time::Instant::now(),
            finished: false,
        };
        trace.event("start", "");
        trace
    }

    pub(super) fn event(&self, phase: &str, detail: &str) {
        let callback = CALLBACK.with(|callback| callback.borrow().clone());
        if let Some(callback) = callback {
            let value = serde_json::json!({"id": self.id, "operation": self.operation,
                "target": self.target, "peer": self.peer, "phase": phase,
                "elapsed_ms": self.started.elapsed().as_secs_f64() * 1000.0, "detail": detail});
            let _ = callback.call1(&JsValue::NULL, &JsValue::from_str(&value.to_string()));
        }
    }

    pub(super) fn finish(&mut self, detail: &str) {
        self.finished = true;
        self.event("finish", detail);
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        if !self.finished {
            self.event("cancelled-or-error", "");
        }
    }
}
