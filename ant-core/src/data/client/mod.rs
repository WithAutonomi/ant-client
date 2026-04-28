//! Client operations for the Autonomi network.
//!
//! Provides high-level APIs for storing and retrieving data
//! on the Autonomi decentralized network.

pub mod adaptive;
pub mod batch;
pub mod cache;
pub mod chunk;
pub mod data;
pub mod file;
pub mod merkle;
pub mod payment;
pub(crate) mod peer_cache;
pub mod quote;

use crate::data::client::adaptive::{AdaptiveConfig, AdaptiveController, ChannelStart, Outcome};
use crate::data::client::cache::ChunkCache;
use crate::data::error::{Error, Result};
use crate::data::network::Network;
use ant_protocol::evm::Wallet;
use ant_protocol::transport::{MultiAddr, P2PNode, PeerId};
use ant_protocol::{XorName, CLOSE_GROUP_SIZE};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tracing::debug;

/// Classify a `data::error::Error` into a controller `Outcome`.
///
/// Capacity signals (Timeout / NetworkError) drive the controller
/// down; application errors do not. The mapping is conservative:
/// anything that COULD be transport-related is treated as a network
/// signal, because under-classifying a real network failure as
/// "application error" makes the controller blind to genuine stress.
///
/// Mapping policy:
/// - `Timeout` -> `Timeout` (per-op deadline elapsed)
/// - `Network`, `InsufficientPeers`, `Io` -> `NetworkError` (transport
///   layer reported failure)
/// - `Protocol`, `Storage` -> `NetworkError` (these wrap remote errors
///   that frequently include peer disconnects mid-stream — under
///   network stress these are how transport failures surface)
/// - `PartialUpload` -> `NetworkError` (literal capacity signal: some
///   chunks could not be stored)
/// - `AlreadyStored`, `Encryption`, `Crypto`, `Payment`,
///   `Serialization`, `InvalidData`, `SignatureVerification`,
///   `Config`, `InsufficientDiskSpace`, `CostEstimationInconclusive`
///   -> `ApplicationError` (would happen on a perfectly healthy link)
pub(crate) fn classify_error(err: &Error) -> Outcome {
    match err {
        Error::Timeout(_) => Outcome::Timeout,
        Error::Network(_)
        | Error::InsufficientPeers(_)
        | Error::Io(_)
        | Error::Protocol(_)
        | Error::Storage(_)
        | Error::PartialUpload { .. } => Outcome::NetworkError,
        Error::AlreadyStored
        | Error::Encryption(_)
        | Error::Crypto(_)
        | Error::Payment(_)
        | Error::Serialization(_)
        | Error::InvalidData(_)
        | Error::SignatureVerification(_)
        | Error::Config(_)
        | Error::InsufficientDiskSpace(_)
        | Error::CostEstimationInconclusive(_) => Outcome::ApplicationError,
    }
}

/// Default timeout for lightweight network operations (quotes, DHT lookups) in seconds.
const DEFAULT_QUOTE_TIMEOUT_SECS: u64 = 10;

/// Default timeout for chunk store operations in seconds.
///
/// Chunk PUTs transfer multi-MB payloads to multiple peers. On residential
/// connections with limited upload bandwidth, the default quote timeout (10 s)
/// is far too short — a 4 MB chunk at 1 Mbps takes ~32 s just for the data
/// transfer, before accounting for QUIC slow-start and NAT traversal overhead.
const DEFAULT_STORE_TIMEOUT_SECS: u64 = 10;

/// Default quote concurrency: high because quoting is pure network I/O
/// (DHT lookups + small request/response messages) with no CPU-bound work.
const DEFAULT_QUOTE_CONCURRENCY: usize = 32;

/// Default store concurrency: moderate because each chunk PUT sends ~4MB
/// to 7 close-group peers. At 8 concurrent stores, ~225MB of outbound
/// traffic can be in flight. Users on fast connections can increase this
/// with --store-concurrency; users on slow connections can decrease it.
const DEFAULT_STORE_CONCURRENCY: usize = 8;

/// Configuration for the Autonomi client.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// Per-op timeout for lightweight network operations (quotes,
    /// DHT lookups), in seconds. The adaptive controller does NOT
    /// currently size timeouts; this remains a static knob.
    pub quote_timeout_secs: u64,
    /// Per-op timeout for chunk store (PUT) operations, in seconds.
    /// Should be larger than `quote_timeout_secs` because chunk PUTs
    /// transfer multi-MB payloads. The adaptive controller does NOT
    /// currently size timeouts; this remains a static knob.
    pub store_timeout_secs: u64,
    /// Number of closest peers to consider for routing.
    pub close_group_size: usize,
    /// **Deprecated.** Pre-adaptive ceiling for quote concurrency.
    ///
    /// The adaptive controller now sizes quote fan-out from observed
    /// signals. This field, when non-zero and smaller than the
    /// controller's per-channel default, clamps the **quote channel
    /// only** (it does NOT bleed into store or fetch). Removed in a
    /// future release.
    pub quote_concurrency: usize,
    /// **Deprecated.** Pre-adaptive ceiling for store concurrency.
    ///
    /// The adaptive controller now sizes store fan-out from observed
    /// signals. This field, when non-zero and smaller than the
    /// controller's per-channel default, clamps the **store channel
    /// only** (it does NOT bleed into quote or fetch). Removed in a
    /// future release.
    pub store_concurrency: usize,
    /// Adaptive controller configuration. Defaults are tuned to match
    /// or exceed the prior static behavior — disabling adaptation
    /// (`adaptive.enabled = false`) reverts to the controller's
    /// `initial` values without re-evaluation.
    pub adaptive: AdaptiveConfig,
    /// Allow loopback (`127.0.0.1`) connections in the saorsa-transport
    /// layer. Set to `true` only for devnet / local testing. Production
    /// peers on the public Autonomi network reject the QUIC handshake
    /// variant produced when this is `true`, so the default is `false`.
    ///
    /// This mirrors the `--allow-loopback` flag in `ant-cli`, which already
    /// defaults to `false` and threads through to the same
    /// `CoreNodeConfig::builder().local(...)` call.
    pub allow_loopback: bool,
    /// Bind a dual-stack IPv6 socket (`true`) or an IPv4-only socket
    /// (`false`). Defaults to `true`, matching the CLI default.
    ///
    /// Set to `false` only when running on hosts without a working IPv6
    /// stack, to avoid advertising unreachable v6 addresses to the DHT
    /// (which causes slow connects and junk DHT address records). This
    /// mirrors the `--ipv4-only` flag in `ant-cli`.
    pub ipv6: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            quote_timeout_secs: DEFAULT_QUOTE_TIMEOUT_SECS,
            store_timeout_secs: DEFAULT_STORE_TIMEOUT_SECS,
            close_group_size: CLOSE_GROUP_SIZE,
            quote_concurrency: DEFAULT_QUOTE_CONCURRENCY,
            store_concurrency: DEFAULT_STORE_CONCURRENCY,
            adaptive: AdaptiveConfig::default(),
            allow_loopback: false,
            ipv6: true,
        }
    }
}

/// Build the adaptive controller for a `Client`. Loads any persisted
/// snapshot, clamps cold-start values into the deprecated-flag bounds
/// **per channel** (so a pin on `--store-concurrency` does NOT bleed
/// into the fetch / quote channels), and returns the persistence path
/// so callers can save back at shutdown.
fn build_controller(config: &ClientConfig) -> (AdaptiveController, Option<PathBuf>) {
    let mut adaptive_cfg = config.adaptive.clone();

    // Per-channel ceilings: each legacy field is interpreted as a cap
    // for ONLY its matching channel. The fetch channel has no
    // pre-existing legacy field; it always uses the controller's
    // default ceiling.
    //
    // The legacy fields are non-zero by ClientConfig::default(), but
    // we honor them as bounds only when they would actually CONSTRAIN
    // the controller — i.e. when smaller than the per-channel default
    // max. A default ClientConfig must not silently lower the
    // controller's ceilings.
    // A value equal to the historic legacy default is treated as
    // "not pinned by the user" — without this, every default
    // ClientConfig would silently lower the controller's per-channel
    // ceilings to the prior static values (32/8) and the controller
    // could never grow above them.
    let user_quote_max = config.quote_concurrency;
    let user_store_max = config.store_concurrency;
    let quote_pinned = user_quote_max > 0 && user_quote_max != DEFAULT_QUOTE_CONCURRENCY;
    let store_pinned = user_store_max > 0 && user_store_max != DEFAULT_STORE_CONCURRENCY;
    if quote_pinned && user_quote_max < adaptive_cfg.max.quote {
        adaptive_cfg.max.quote = user_quote_max;
    }
    if store_pinned && user_store_max < adaptive_cfg.max.store {
        adaptive_cfg.max.store = user_store_max;
    }

    // Cold-start values: matched to the prior static defaults. If the
    // legacy field caps the channel below the cold-start, lower the
    // start to match — never start above the channel's max.
    let mut start = ChannelStart::default();
    start.quote = start.quote.min(adaptive_cfg.max.quote);
    start.store = start.store.min(adaptive_cfg.max.store);
    start.fetch = start.fetch.min(adaptive_cfg.max.fetch);

    let adaptive_enabled = adaptive_cfg.enabled;
    let controller = AdaptiveController::new(start, adaptive_cfg);
    // Skip disk warm-start entirely when adaptation is disabled —
    // fixed-concurrency mode means the user wants exactly the cold
    // start, no surprises from prior runs. (warm_start is also a
    // no-op when disabled, but skipping the load avoids file I/O
    // and the path-resolution side effects.)
    let persist_path = if adaptive_enabled {
        let p = adaptive::default_persist_path();
        if let Some(ref path) = p {
            if let Some(snap) = adaptive::load_snapshot(path) {
                debug!(path = %path.display(), "adaptive: warm-start from disk");
                controller.warm_start(snap);
            }
        }
        p
    } else {
        // Even with adaptation off, persist_path is computed so
        // explicit save_adaptive_snapshot() calls still work — but
        // the controller currently never moves, so saving the cold
        // start is harmless.
        adaptive::default_persist_path()
    };

    // Note: self_encryption's `STREAM_DECRYPT_BATCH_SIZE` is a
    // `LazyLock<usize>` populated from the env var at first access
    // and frozen for the process lifetime. Setting the env var from
    // Rust would require `std::env::set_var`, which is `unsafe`
    // since Rust 1.80 (it races against concurrent reads in any
    // other thread); per project policy, `unsafe` is banned.
    //
    // The adaptive controller still drives fan-out *inside* each
    // batch — we re-read `controller.fetch.current()` in the
    // `streaming_decrypt` callback. The upstream batch size only
    // controls how many chunks `self_encryption` asks us for at a
    // time (default 10). For larger batch sizes export
    // `STREAM_DECRYPT_BATCH_SIZE` before launching the process.

    (controller, persist_path)
}

/// Client for the Autonomi decentralized network.
///
/// Provides high-level APIs for storing and retrieving chunks
/// and files on the network.
pub struct Client {
    config: ClientConfig,
    network: Network,
    wallet: Option<Arc<Wallet>>,
    evm_network: Option<ant_protocol::evm::Network>,
    chunk_cache: ChunkCache,
    next_request_id: AtomicU64,
    /// Adaptive concurrency controller: replaces the static
    /// quote/store concurrency knobs. See `adaptive` module.
    controller: AdaptiveController,
    /// Path the controller persists its snapshot to. `None` disables
    /// persistence (useful for tests / non-disk environments).
    persist_path: Option<PathBuf>,
}

impl Client {
    /// Create a client connected to the given P2P node.
    #[must_use]
    pub fn from_node(node: Arc<P2PNode>, config: ClientConfig) -> Self {
        let network = Network::from_node(node);
        let (controller, persist_path) = build_controller(&config);
        Self {
            config,
            network,
            wallet: None,
            evm_network: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
            controller,
            persist_path,
        }
    }

    /// Create a client connected to bootstrap peers.
    ///
    /// Threads `config.allow_loopback` and `config.ipv6` through to
    /// `Network::new`, which controls the saorsa-transport `local` and
    /// `ipv6` flags on the underlying `CoreNodeConfig`. See
    /// `ClientConfig::allow_loopback` and `ClientConfig::ipv6` for details.
    ///
    /// # Errors
    ///
    /// Returns an error if the P2P node cannot be created or bootstrapping fails.
    pub async fn connect(
        bootstrap_peers: &[std::net::SocketAddr],
        config: ClientConfig,
    ) -> Result<Self> {
        debug!(
            "Connecting to Autonomi network with {} bootstrap peers (allow_loopback={}, ipv6={})",
            bootstrap_peers.len(),
            config.allow_loopback,
            config.ipv6,
        );
        let network = Network::new(bootstrap_peers, config.allow_loopback, config.ipv6).await?;
        let (controller, persist_path) = build_controller(&config);
        Ok(Self {
            config,
            network,
            wallet: None,
            evm_network: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
            controller,
            persist_path,
        })
    }

    /// Set the wallet for payment operations.
    ///
    /// Also populates the EVM network from the wallet so that
    /// token approvals work without a separate `with_evm_network` call.
    #[must_use]
    pub fn with_wallet(mut self, wallet: Wallet) -> Self {
        self.evm_network = Some(wallet.network().clone());
        self.wallet = Some(Arc::new(wallet));
        self
    }

    /// Set the EVM network without requiring a wallet.
    ///
    /// This enables token approval and contract interactions
    /// for external-signer flows where the private key lives outside Rust.
    #[must_use]
    pub fn with_evm_network(mut self, network: ant_protocol::evm::Network) -> Self {
        self.evm_network = Some(network);
        self
    }

    /// Get the EVM network, falling back to the wallet's network if available.
    ///
    /// # Errors
    ///
    /// Returns an error if neither `with_evm_network` nor `with_wallet` was called.
    pub(crate) fn require_evm_network(&self) -> Result<&ant_protocol::evm::Network> {
        if let Some(ref net) = self.evm_network {
            return Ok(net);
        }
        if let Some(ref wallet) = self.wallet {
            return Ok(wallet.network());
        }
        Err(Error::Payment(
            "EVM network not configured — call with_evm_network() or with_wallet() first"
                .to_string(),
        ))
    }

    /// Get the client configuration.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Get a mutable reference to the client configuration.
    pub fn config_mut(&mut self) -> &mut ClientConfig {
        &mut self.config
    }

    /// Get a reference to the network layer.
    #[must_use]
    pub fn network(&self) -> &Network {
        &self.network
    }

    /// Get the wallet, if configured.
    #[must_use]
    pub fn wallet(&self) -> Option<&Arc<Wallet>> {
        self.wallet.as_ref()
    }

    /// Get a reference to the chunk cache.
    #[must_use]
    pub fn chunk_cache(&self) -> &ChunkCache {
        &self.chunk_cache
    }

    /// Adaptive concurrency controller. Hot loops read
    /// `controller().<channel>.current()` to size their fan-out and
    /// call `.observe(...)` on each completion.
    #[must_use]
    pub fn controller(&self) -> &AdaptiveController {
        &self.controller
    }

    /// Persist the current adaptive snapshot to disk so the next
    /// `Client::connect` warm-starts at the learned values instead of
    /// cold defaults. Best effort — failures log and are discarded.
    /// Idempotent. Safe to call from a Drop impl or an explicit
    /// shutdown hook.
    pub fn save_adaptive_snapshot(&self) {
        if let Some(ref path) = self.persist_path {
            adaptive::save_snapshot(path, self.controller.snapshot());
        }
    }

    /// Get the next request ID for protocol messages.
    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Return all peers in the close group for a target address.
    ///
    /// Queries the DHT for the closest peers by XOR distance.
    /// Returns each peer paired with its known network addresses.
    pub(crate) async fn close_group_peers(
        &self,
        target: &XorName,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        let peers = self
            .network()
            .find_closest_peers(target, self.config().close_group_size)
            .await?;

        if peers.is_empty() {
            return Err(Error::InsufficientPeers(
                "DHT returned no peers for target address".to_string(),
            ));
        }
        Ok(peers)
    }
}

/// Persist the adaptive snapshot when the `Client` is dropped, so any
/// caller — CLI, daemon, library user, integration test — gets
/// warm-start carry-over for free without remembering to call
/// `save_adaptive_snapshot()` explicitly. Best effort, sync `std::fs`,
/// no panic risk on a poisoned mutex (the inner helper handles it).
///
/// We deliberately write SYNCHRONOUSLY (not via `spawn_blocking`)
/// because Drop runs during process shutdown / runtime teardown,
/// when fire-and-forget background tasks can be dropped before they
/// complete and the snapshot is silently lost. A small synchronous
/// stall on a tokio worker (typically <1ms for a local-disk JSON
/// write of ~50 bytes) is the right tradeoff for guaranteed
/// persistence — BOUNDED by `DROP_SAVE_TIMEOUT` so a stalled
/// network-mounted data dir cannot block process shutdown.
const DROP_SAVE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

impl Drop for Client {
    fn drop(&mut self) {
        let Some(path) = self.persist_path.clone() else {
            return;
        };
        let snap = self.controller.snapshot();
        adaptive::save_snapshot_with_timeout(path, snap, DROP_SAVE_TIMEOUT);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Cover EVERY variant of `data::error::Error`. Build an instance of
    /// each, classify it, and assert the resulting `Outcome` matches the
    /// only sensible mapping. If a future commit adds a new error variant
    /// without updating `classify_error`, this test fails to ensure the
    /// adaptive controller always sees correct capacity signals.
    ///
    /// Mapping policy (mirrors `classify_error` doc):
    /// - `Timeout` -> `Outcome::Timeout`
    /// - `Network`, `InsufficientPeers`, `Io`, `Protocol`, `Storage`,
    ///   `PartialUpload` -> `Outcome::NetworkError` (transport-related
    ///   or literal capacity failure)
    /// - everything else -> `Outcome::ApplicationError` (would happen
    ///   on a perfectly healthy network)
    #[test]
    fn classify_error_covers_all_variants() {
        let cases: Vec<(Error, Outcome)> = vec![
            (Error::Timeout("t".to_string()), Outcome::Timeout),
            (Error::Network("n".to_string()), Outcome::NetworkError),
            (
                Error::InsufficientPeers("p".to_string()),
                Outcome::NetworkError,
            ),
            (Error::Storage("s".to_string()), Outcome::NetworkError),
            (Error::Payment("p".to_string()), Outcome::ApplicationError),
            (Error::Protocol("p".to_string()), Outcome::NetworkError),
            (
                Error::InvalidData("d".to_string()),
                Outcome::ApplicationError,
            ),
            (
                Error::Serialization("s".to_string()),
                Outcome::ApplicationError,
            ),
            (Error::Crypto("c".to_string()), Outcome::ApplicationError),
            (
                Error::Io(std::io::Error::other("io")),
                Outcome::NetworkError,
            ),
            (Error::Config("c".to_string()), Outcome::ApplicationError),
            (
                Error::SignatureVerification("s".to_string()),
                Outcome::ApplicationError,
            ),
            (
                Error::Encryption("e".to_string()),
                Outcome::ApplicationError,
            ),
            (Error::AlreadyStored, Outcome::ApplicationError),
            (
                Error::InsufficientDiskSpace("d".to_string()),
                Outcome::ApplicationError,
            ),
            (
                Error::CostEstimationInconclusive("c".to_string()),
                Outcome::ApplicationError,
            ),
            (
                Error::PartialUpload {
                    stored: vec![],
                    stored_count: 0,
                    failed: vec![],
                    failed_count: 0,
                    total_chunks: 0,
                    reason: "r".to_string(),
                },
                Outcome::NetworkError,
            ),
        ];
        for (err, expected) in &cases {
            let got = classify_error(err);
            assert_eq!(
                got, *expected,
                "classify_error({err:?}) = {got:?}, expected {expected:?}",
            );
        }
    }

    /// C4 fix guard: pinning the legacy `quote_concurrency` /
    /// `store_concurrency` ClientConfig fields must clamp ONLY the
    /// matching channel's max in the resulting controller. The fetch
    /// (download) channel must keep its full default ceiling.
    #[test]
    fn legacy_concurrency_pin_does_not_bleed_across_channels() {
        let cfg = ClientConfig {
            quote_concurrency: 4,
            store_concurrency: 2,
            ..ClientConfig::default()
        };
        let (controller, _) = build_controller(&cfg);
        // The store/quote caps must be clamped to the user's pin.
        assert_eq!(controller.config.max.quote, 4, "quote pin not respected");
        assert_eq!(controller.config.max.store, 2, "store pin not respected");
        // The fetch cap must NOT have been lowered — that's the
        // regression C4 was about.
        let default_fetch_max = adaptive::ChannelMax::default().fetch;
        assert_eq!(
            controller.config.max.fetch, default_fetch_max,
            "fetch cap was lowered by store/quote pin (C4 regression)"
        );
        // Cold-start values must respect the lowered ceilings.
        assert!(
            controller.quote.current() <= 4,
            "quote start exceeds its cap"
        );
        assert!(
            controller.store.current() <= 2,
            "store start exceeds its cap"
        );
    }

    /// Default ClientConfig must NOT silently lower the controller's
    /// per-channel ceilings — the adaptive defaults give every channel
    /// real headroom to grow. This guards against future commits
    /// re-introducing a global clamp.
    #[test]
    fn default_client_config_does_not_clamp_controller_max() {
        let cfg = ClientConfig::default();
        let (controller, _) = build_controller(&cfg);
        let defaults = adaptive::ChannelMax::default();
        // The legacy fields default to 32/8 (the prior static knobs),
        // both of which are <= the per-channel adaptive defaults
        // (128/64). build_controller must keep the larger, not clobber
        // with the legacy values.
        assert_eq!(controller.config.max.quote, defaults.quote);
        assert_eq!(controller.config.max.store, defaults.store);
        assert_eq!(controller.config.max.fetch, defaults.fetch);
        // Compile-time-ish guard: if a new variant is added to Error,
        // this match forces an update here.
        let _ = |e: &Error| match e {
            Error::Timeout(_)
            | Error::Network(_)
            | Error::InsufficientPeers(_)
            | Error::Storage(_)
            | Error::Payment(_)
            | Error::Protocol(_)
            | Error::InvalidData(_)
            | Error::Serialization(_)
            | Error::Crypto(_)
            | Error::Io(_)
            | Error::Config(_)
            | Error::SignatureVerification(_)
            | Error::Encryption(_)
            | Error::AlreadyStored
            | Error::InsufficientDiskSpace(_)
            | Error::CostEstimationInconclusive(_)
            | Error::PartialUpload { .. } => (),
        };
    }
}
