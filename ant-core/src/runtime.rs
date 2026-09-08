// Copyright 2024 MaidSafe.net limited.
//
// This Autonomi Software is licensed under the MIT license <LICENSE-MIT or
// https://opensource.org/licenses/MIT> or the Apache License, Version 2.0
// <LICENSE-APACHE or https://www.apache.org/licenses/LICENSE-2.0>, at your
// option. This file may not be copied, modified, or distributed except
// according to those terms.

//! Platform timers for shared client operations.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use tokio::time::{sleep, timeout};

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
