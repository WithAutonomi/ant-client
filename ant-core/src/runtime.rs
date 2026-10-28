// Copyright 2024 MaidSafe.net limited.
//
// This Autonomi Software is licensed under the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT> or the Apache License, Version 2.0
// <LICENSE-APACHE or https://www.apache.org/licenses/LICENSE-2.0>, at your
// option. This file may not be copied, modified, or distributed except
// according to those terms.

//! Platform timers for shared client operations.
/// The clock these timers run on: tokio's off the browser (which tests can
/// pause), the page's in it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::time::Instant as Clock;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::time::{sleep, timeout};
#[cfg(target_arch = "wasm32")]
pub(crate) use web_time::Instant as Clock;

#[cfg(target_arch = "wasm32")]
use futures::channel::oneshot;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::{closure::Closure, JsCast, JsValue};
#[cfg(target_arch = "wasm32")]
use web_sys::{Document, Event, MessageChannel, MessageEvent, MessagePort, VisibilityState};

#[cfg(target_arch = "wasm32")]
pub(crate) async fn sleep(duration: std::time::Duration) {
    // Chunk long waits instead of truncating or wrapping the browser timer.
    let mut remaining = duration.as_nanos().div_ceil(1_000_000);
    while remaining > 0 {
        let millis = remaining.min(u128::from(u32::MAX)) as u32;
        gloo_timers::future::TimeoutFuture::new(millis).await;
        remaining -= u128::from(millis);
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug, thiserror::Error)]
#[error("deadline elapsed")]
pub(crate) struct Elapsed;

#[cfg(target_arch = "wasm32")]
pub(crate) async fn timeout<F: std::future::Future>(
    duration: std::time::Duration,
    future: F,
) -> Result<F::Output, Elapsed> {
    let timer = sleep(duration);
    futures_util::pin_mut!(future, timer);
    match futures_util::future::select(future, timer).await {
        futures_util::future::Either::Left((value, _)) => Ok(value),
        futures_util::future::Either::Right(_) => Err(Elapsed),
    }
}

/// Lets other tasks run before this one goes on.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::task::yield_now;

/// Lets other tasks run, and the browser handle its events, before this one
/// goes on: a browser task that never waits keeps both out. It waits for a
/// posted message, not a timer: browsers slow the timers of tabs in the
/// background to one a second or less, and do not slow posted messages.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn yield_now() {
    let Ok(channel) = MessageChannel::new() else {
        return;
    };
    let (sender, received) = oneshot::channel::<()>();
    let mut sender = Some(sender);
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |_: MessageEvent| {
        if let Some(sender) = sender.take() {
            let _ = sender.send(());
        }
    });
    let port = channel.port1();
    port.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    // Dropped before the closure, however this ends: the port lets go of it
    // first, so it is never called once freed.
    let _detach = Detach(port);
    if channel.port2().post_message(&JsValue::NULL).is_err() {
        return;
    }
    let _ = received.await;
}

/// Detaches a port's handler, and closes it, when dropped.
#[cfg(target_arch = "wasm32")]
struct Detach(MessagePort);

#[cfg(target_arch = "wasm32")]
impl Drop for Detach {
    fn drop(&mut self) {
        self.0.set_onmessage(None);
        self.0.close();
    }
}

/// Whether this runs in a browser page that is hidden (a tab in the
/// background), whose timers the browser slows to a second or more.
#[cfg(target_arch = "wasm32")]
pub(crate) fn page_hidden() -> bool {
    web_sys::window()
        .and_then(|window| window.document())
        .is_some_and(|document| document.visibility_state() == VisibilityState::Hidden)
}

/// Whether this runs in a hidden browser page: never, natively.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn page_hidden() -> bool {
    false
}

/// Ends once this browser page is hidden (at once if it is now). Watches the
/// page's `visibilitychange` event, not a timer, which a hidden page slows.
/// Never ends outside a browser page.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn until_page_hidden() {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return futures::future::pending().await;
    };
    if document.visibility_state() == VisibilityState::Hidden {
        return;
    }
    let (sender, hidden) = oneshot::channel::<()>();
    let mut sender = Some(sender);
    let watched = document.clone();
    let on_change = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
        if watched.visibility_state() == VisibilityState::Hidden {
            if let Some(sender) = sender.take() {
                let _ = sender.send(());
            }
        }
    });
    if document
        .add_event_listener_with_callback("visibilitychange", on_change.as_ref().unchecked_ref())
        .is_err()
    {
        return futures::future::pending().await;
    }
    // Unlistened before the closure is freed, however this ends.
    let _unlisten = Unlisten {
        document,
        callback: on_change,
    };
    let _ = hidden.await;
}

/// Removes a page's `visibilitychange` listener when dropped.
#[cfg(target_arch = "wasm32")]
struct Unlisten {
    document: Document,
    callback: Closure<dyn FnMut(Event)>,
}

#[cfg(target_arch = "wasm32")]
impl Drop for Unlisten {
    fn drop(&mut self) {
        let _ = self.document.remove_event_listener_with_callback(
            "visibilitychange",
            self.callback.as_ref().unchecked_ref(),
        );
    }
}

/// Never ends: there is no page to hide natively.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) async fn until_page_hidden() {
    futures::future::pending().await
}

/// Read the platform clock while preserving the protocol's serialized SystemTime type.
pub(crate) fn system_time() -> std::time::SystemTime {
    let elapsed = web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .unwrap_or_default();
    std::time::SystemTime::UNIX_EPOCH
        .checked_add(elapsed)
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}
