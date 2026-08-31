/// Cross-platform Autonomi client logic and browser WASM bindings.
pub mod browser;

#[cfg(any(feature = "native", feature = "browser-wasm"))]
mod client_engine;

#[cfg(feature = "native")]
pub mod config;
#[cfg(feature = "native")]
pub mod data;
#[cfg(feature = "native")]
pub mod datamap_file;
#[cfg(feature = "native")]
pub mod error;
#[cfg(feature = "native")]
pub mod node;
#[cfg(feature = "native")]
pub mod update;

#[cfg(all(target_arch = "wasm32", not(feature = "browser-wasm")))]
compile_error!("WASM builds of ant-core require the `browser-wasm` feature");
