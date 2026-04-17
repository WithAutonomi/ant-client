# Changelog

All notable changes to the `ant-core` library will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.0] — Unreleased

This release decouples `ant-core` from `ant-node` at runtime. The wire
protocol was extracted into the new [`ant-protocol`] crate; `ant-core`
now depends on `ant-protocol` and keeps `ant-node` only as an optional
dependency (for `LocalDevnet`) and a dev-dependency (for the test
harness that spins up real nodes).

[`ant-protocol`]: https://crates.io/crates/ant-protocol

### Breaking

- **`ant-node` is no longer a default runtime dependency.** The
  `LocalDevnet` wrapper and its types (`DevnetManifest` re-export) now
  require `features = ["devnet"]`. Downstream `Cargo.toml`:
  ```toml
  ant-core = { version = "0.2", features = ["devnet"] }
  ```
- `Client::with_evm_network` now takes `ant_protocol::evm::Network`
  instead of `evmlib::Network`. Both paths resolve to the same
  underlying type (`ant_protocol::evm` is a re-export of `evmlib`), so
  the fix for downstream callers is a one-line path change.
- `impl From<ant_node::Error> for Error` has been removed. Call sites
  that used `?` to propagate an `ant_node::Error` into an `ant-core`
  `Error` must now map explicitly:
  ```rust
  foo().await.map_err(|e| Error::Network(e.to_string()))?;
  ```
- `ant_core::data::LocalDevnet` is now behind the `devnet` feature.
  Code that previously imported it unconditionally must either enable
  the feature or use the `DevnetManifest` type alone (available
  unconditionally via `ant_core::data::DevnetManifest`).

### Changed

- `ant-core` no longer declares direct dependencies on `evmlib`,
  `saorsa-core`, or `saorsa-pqc`. Those transitive dependencies are
  consumed through `ant_protocol::{evm, transport, pqc}` so Cargo
  enforces a single version pin shared with `ant-node`.
- `DevnetManifest` and `DevnetEvmInfo` moved to
  `ant_protocol::devnet_manifest`. The `ant_core::data::DevnetManifest`
  path is unchanged; only the origin crate differs. The on-disk JSON
  format is byte-for-byte compatible.
- All internal `ant_node::{ant_protocol, client, payment}::*` imports
  rewritten to `ant_protocol::*` paths.

### Added

- `devnet` feature flag (off by default) that pulls in `ant-node` as an
  optional runtime dependency to enable `LocalDevnet`.
