/// Cross-platform Autonomi client logic and browser WASM bindings.
pub mod browser;

mod payment_policy;
mod quote_policy;
mod quote_validation;
mod record;
mod transfer_policy;

mod client_engine;

#[cfg(feature = "native")]
pub mod config;
pub mod data;
#[cfg(feature = "native")]
pub mod datamap_file;
#[cfg(feature = "native")]
pub mod error;
#[cfg(feature = "native")]
pub mod node;
mod runtime;
#[cfg(feature = "native")]
pub mod update;

#[cfg(all(target_arch = "wasm32", not(feature = "browser-wasm")))]
compile_error!("WASM builds of ant-core require the `browser-wasm` feature");
