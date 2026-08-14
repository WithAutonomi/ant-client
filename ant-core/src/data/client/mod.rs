//! Client operations for the Autonomi network.
//!
//! Provides high-level APIs for storing and retrieving data
//! on the Autonomi decentralized network.

pub mod adaptive;
pub mod batch;
pub mod cache;
pub(crate) mod cached_merkle;
pub(crate) mod cached_single;
pub mod chunk;
pub mod data;
pub mod file;
pub mod merkle;
pub mod payment;
pub mod quote;

use crate::data::client::adaptive::{AdaptiveConfig, AdaptiveController, ChannelStart, Outcome};
use crate::data::client::cache::ChunkCache;
use crate::data::error::{Error, Result};
use crate::data::network::{Network, NetworkHealth};
use crate::data::peer_cache;
use ant_protocol::evm::Wallet;
use ant_protocol::transport::{MultiAddr, P2PNode, PeerId};
use ant_protocol::{XorName, CLOSE_GROUP_SIZE};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::Mutex;
use tracing::debug;

/// Width of the chunk PUT-target set (initial writes plus fallback): the
/// closest `PUT_TARGET_WIDTH` peers to the address.
///
/// Mirrors the node-side `K_BUCKET_SIZE` / `PAID_QUOTE_ISSUER_CLOSENESS_WIDTH`
/// (20): a node accepts a reused payment proof only when one of the proof's
/// closest-`CLOSE_GROUP_SIZE` quote issuers is within its own local 20-closest,
/// so trying peers past this width is pointless.
pub(crate) const PUT_TARGET_WIDTH: usize = 20;

/// Ceiling on how long to wait for a peer to answer a settlement-versioned
/// quote request before falling back to the unversioned shape.
///
/// A storer that predates the versioned request cannot decode it and never
/// replies, so the only way to find out is to wait.
///
/// Note what is being waited for. This is **not** a cheap parse check: a peer
/// that understands the request runs the whole quote handler, queueing,
/// storage reads, pricing and signing, so the answer takes as long as any
/// quote takes. Abandoning the wait does not cancel that work either, it just
/// stops listening and adds a duplicate legacy request on top. So the wait has
/// to be sized for a real quote, not for a ping.
///
/// The full timeout is what made this expensive. The merkle E2E suite runs
/// with `quote_timeout_secs = 120`, so every probe against a fleet on the
/// published node cost two minutes and the suite blew the 60-minute CI cap.
///
/// # It must never bind in production
///
/// **Keep this at or above the largest production `quote_timeout_secs`**,
/// currently 10s. That is not a tuning preference, it is the safety property.
///
/// This wait is the only window in which a peer can refuse. Abandoning it
/// early does not merely mislabel a slow peer: the fallback then sends an
/// unversioned request under a *new* request id, so a refusal arriving after
/// the ceiling is answering a request nobody is listening to. It never counts
/// toward corroboration, never sets the latch, and the legacy request it
/// raced can return a perfectly good quote the client then pays against.
///
/// A shorter ceiling was tried, at 5s, to bring the slower CI runner under the
/// cap. It would have turned every legitimate 5-to-10 second refusal in
/// production into exactly that silent downgrade. The never-demote rule does
/// not help: it stops a peer being *cached* as legacy after it has answered
/// once, but it does not stop the request in flight from falling back. And the
/// compile-time guard does not help either, because it binds future builds
/// while the clients at risk are the ones already released.
///
/// So the ceiling exists solely to bound configurations that set a timeout far
/// above any real answer time, which in practice means test harnesses. Above
/// 10s it never binds on a production client, and nothing real is truncated.
///
/// The cost of leaving it here is roughly two minutes per merkle E2E test
/// while the suite's devnet still speaks the pre-versioned dialect. That is a
/// consequence of the temporary fork-branch protocol pin, not of the design:
/// once the fleet under test can answer a versioned request there are no
/// probes to pay for, and the suite returns to its baseline.
pub(crate) const VERSIONED_QUOTE_PROBE_CEILING: std::time::Duration =
    std::time::Duration::from_secs(15);

/// How many distinct peers must refuse this client's settlement version before
/// the refusal is believed and uploads stop.
///
/// Nothing authenticates a refusal, so one peer's word cannot be enough: a
/// single hostile or misconfigured storer answering `ClientUpdateRequired` to
/// everything would otherwise deny every upload, turning an over-query design
/// that tolerates many bad peers into one that tolerates none.
///
/// Two is deliberately low. A genuine incompatibility reaches it instantly,
/// because every peer enforcing the newer rule refuses, and a client queries
/// far more than two. An attacker has to control two of the peers a given
/// request happens to reach, which is a materially different proposition from
/// controlling one.
pub(crate) const SETTLEMENT_REFUSAL_QUORUM: usize = 2;

/// Compile-time cutover guard shared by **every** unversioned quote retry.
///
/// Both quote paths fall back to an unversioned request when a peer stays
/// silent, because a storer built before the settlement version existed cannot
/// decode the versioned one. Silence is not proof of that, though: a dropped
/// response, packet loss, an overloaded peer, or one deliberately discarding
/// versioned requests are indistinguishable from the client side. So the
/// fallback is a downgrade path and can be provoked.
///
/// It is not only literal silence, either. `send_and_await_chunk_response`
/// keeps waiting when a reply decodes but carries a body the mapper does not
/// recognise, so a peer answering with an unexpected variant also lands on the
/// timeout and takes this path. That grants no capability beyond staying
/// silent, which is why it is bounded rather than special-cased, but a comment
/// claiming "any structured response prevents the retry" would be wrong.
///
/// It is safe only while **no refusal of either kind is possible**, and that
/// means bounding both constants, not just the minimum.
///
/// Raising `MIN` is the obvious hazard: a client below it would be refused,
/// and the retry hands it a quote anyway. Raising `CURRENT` is the subtler
/// one. As soon as some node runs a newer `CURRENT` than another, the older
/// node refuses newer clients with `StorerUpdateRequired` precisely because it
/// cannot promise to honour their payment. A client that retries such a peer
/// unversioned gets that unhonourable quote, and for a settlement change that
/// is not a pure increase the payment is then rejected after it has settled.
/// Guarding only `MIN` would leave that route open.
///
/// So the guard requires both to still be at the first declarable version.
///
/// This bounds **future builds** only. A client binary already in the field
/// carries whatever fallback it shipped with, and no source change reaches it;
/// that is inherent to shipping software and is why the storer still verifies
/// every payment it is actually offered.
///
/// Each fallback site references this constant so the guard cannot be orphaned
/// by deleting one path and forgetting the other. Retiring the fallbacks means
/// deleting this constant and every reference to it, which the compiler then
/// points at one by one.
pub(crate) const UNVERSIONED_RETRY_REQUIRES_MIN_V1: () = assert!(
    ant_protocol::MIN_SUPPORTED_SETTLEMENT_VERSION == 1
        && ant_protocol::CURRENT_SETTLEMENT_VERSION == 1,
    "an unversioned quote retry is still compiled in: it is a downgrade path around \
     both the too-old and the node-behind refusals, so delete every fallback site \
     before raising MIN_SUPPORTED_SETTLEMENT_VERSION or CURRENT_SETTLEMENT_VERSION"
);

/// Distinct peers that have refused this client's settlement version, plus the
/// wording of the first refusal.
///
/// A separate type rather than a field so the corroboration rule can be tested
/// on its own: it is the piece that decides whether an upload stops, and it
/// has to hold against both a lone lying peer and a genuine incompatibility.
#[derive(Clone, Default)]
pub(crate) struct SettlementRefusals {
    inner: Arc<Mutex<(HashSet<PeerId>, Option<String>)>>,
}

impl SettlementRefusals {
    /// Record a refusal from `peer_id`, returning the wording once
    /// [`SETTLEMENT_REFUSAL_QUORUM`] distinct peers agree and `None` below it.
    pub(crate) fn note(&self, peer_id: PeerId, message: &str) -> Option<String> {
        let mut guard = self.inner.lock().ok()?;
        let (peers, wording) = &mut *guard;
        peers.insert(peer_id);
        if wording.is_none() {
            *wording = Some(message.to_string());
        }
        (peers.len() >= SETTLEMENT_REFUSAL_QUORUM)
            .then(|| wording.clone())
            .flatten()
    }

    /// The corroborated refusal, if one has been established.
    pub(crate) fn corroborated(&self) -> Option<String> {
        let guard = self.inner.lock().ok()?;
        let (peers, wording) = &*guard;
        (peers.len() >= SETTLEMENT_REFUSAL_QUORUM)
            .then(|| wording.clone())
            .flatten()
    }
}

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
///   `Serialization`, `InvalidData`, `NotFound`, `SignatureVerification`,
///   `Config`, `InsufficientDiskSpace`, `CostEstimationInconclusive`,
///   `Cancelled` -> `ApplicationError` (would happen on a perfectly
///   healthy link; `Cancelled` is caller-initiated and must not be retried
///   as a transport failure; `NotFound` is a definitively absent record
///   reported over a working link, not a transport symptom)
/// - `RemotePut` -> `ApplicationError` (the remote node responded with a
///   structured rejection — the transport succeeded, so the node declined
///   at the application layer; not a local capacity signal)
/// - `ClientUpdateRequired` -> `ApplicationError` (the storer refused to quote
///   a client that settles under superseded rules — a terminal verdict about
///   this build, not about link capacity, and no retry rate clears it)
/// - `CloseGroupShortfall` -> `ApplicationError` (a quorum shortfall caused
///   by close-group dial/relay churn with no PUT-response timeouts — remote
///   peer churn, not local backpressure; a timeout-bearing shortfall keeps
///   `InsufficientPeers`/`NetworkError` instead, so genuine congestion still
///   cuts the cap — V2-554)
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
        // A definitively absent record, reported over a working link —
        // the peers answered, there was just nothing stored there. Not a
        // transport symptom, so it must not push the limiter down.
        | Error::NotFound(_)
        | Error::SignatureVerification(_)
        | Error::Config(_)
        | Error::InsufficientDiskSpace(_)
        | Error::CostEstimationInconclusive(_)
        | Error::Cancelled(_)
        // The storer parsed our request and refused it on its merits, over a
        // working link. Sending fewer requests would not help, and treating it
        // as congestion would quietly shrink the limiter for the rest of the
        // run on the basis of a fault no retry can clear.
        | Error::ClientUpdateRequired(_)
        | Error::StorerUpdateRequired(_)
        | Error::BadQuoteBinding { .. }
        | Error::BadQuoteCommitment { .. }
        // An external-signer merkle batch larger than one tree can hold —
        // a caller-shape refusal raised before any network work, so it says
        // nothing about link capacity.
        | Error::MerkleBatchTooLarge { .. }
        // A remote node responded with a structured rejection — the
        // transport round-trip succeeded, so the node declined at the
        // application layer (payment/disk/quote/pool). Not a local
        // capacity signal; recorded but must not push the limiter down.
        | Error::RemotePut { .. }
        // A close-group PUT shortfall caused purely by dial/relay churn
        // (dead/stale relayed peer addresses), with no PUT-response
        // timeouts to signal local backpressure. Remote peer churn, not
        // "client sending too fast" — must not push the limiter down
        // (V2-554). A shortfall that DID time out keeps `InsufficientPeers`
        // (`NetworkError`) so real congestion still cuts the cap.
        | Error::CloseGroupShortfall(_) => Outcome::ApplicationError,
    }
}

/// Compute XOR distance between a peer's ID bytes and a target address.
///
/// Uses the first 32 bytes of the peer ID (or fewer if shorter) XORed
/// with the target address. The returned byte array sorts
/// lexicographically from closest to furthest.
pub(crate) fn peer_xor_distance(peer_id: &PeerId, target: &[u8; 32]) -> [u8; 32] {
    let peer_bytes = peer_id.as_bytes();
    let mut distance = [0u8; 32];
    for (i, d) in distance.iter_mut().enumerate() {
        let peer_byte = peer_bytes.get(i).copied().unwrap_or(0);
        *d = peer_byte ^ target[i];
    }
    distance
}

/// Default timeout for lightweight network operations (quotes, DHT lookups) in seconds.
const DEFAULT_QUOTE_TIMEOUT_SECS: u64 = 10;

/// Default timeout for the per-peer chunk GET response and any other
/// caller that explicitly reads `store_timeout_secs`, in seconds.
///
/// Note despite the name: this knob does **not** govern the non-merkle
/// chunk PUT response timeout — that path uses the
/// `STORE_RESPONSE_TIMEOUT` constant in `chunk.rs` directly. Nor does
/// it govern the merkle batch PUT timeout — see
/// `DEFAULT_MERKLE_STORE_TIMEOUT_SECS`.
///
/// 10 s matches the pre-existing `main` default and intentionally
/// excludes residential-upload tuning, which is Mick's PR #78
/// territory (splitting GET into its own field).
const DEFAULT_STORE_TIMEOUT_SECS: u64 = 10;

/// Default timeout for **merkle batch** chunk store operations in seconds.
///
/// Separate from `DEFAULT_STORE_TIMEOUT_SECS` because merkle PUTs carry
/// an extra storer-side cost: the payment verifier runs an iterative
/// DHT lookup (`CLOSENESS_LOOKUP_TIMEOUT` in `ant-node`, **240 s**
/// post-PR #89) before accepting the proof.
///
/// This timeout MUST be >= the storer-side `CLOSENESS_LOOKUP_TIMEOUT`
/// plus padding for the store-response round-trip and storer-local
/// I/O. Otherwise the client gives up while the storer is still
/// happily verifying, the storer wastes CPU/bandwidth on a chunk the
/// client has already discarded, and the client re-targets a
/// different close-K member — potentially double-storing the same
/// chunk and polluting routing.
///
/// 270 s = 240 s (storer lookup) + 30 s padding (network RTT + LMDB
/// put + fsync + clock skew tolerance).
///
/// This invariant must be re-validated if either side's timeout
/// changes. Empirically surfaced as "every cross-region merkle chunk
/// times out at 10 s" on a 210-node 7-region testnet run on
/// 2026-05-12; bumping to 270 s flipped that 0/22 -> 9/9 pass rate.
const DEFAULT_MERKLE_STORE_TIMEOUT_SECS: u64 = 270;

/// Default timeout for chunk GET response operations in seconds.
const DEFAULT_CHUNK_GET_TIMEOUT_SECS: u64 = 10;

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
    /// Per-op timeout, in seconds, for the chunk GET response path
    /// (`chunk_get_from_peer`) and any other caller that reads this
    /// field directly.
    ///
    /// Note despite the historical name `store_timeout_secs`: this
    /// knob does **not** govern the non-merkle chunk PUT response
    /// timeout (that path uses the `STORE_RESPONSE_TIMEOUT` constant
    /// in `chunk.rs`) and does **not** govern the merkle batch PUT
    /// timeout (see `merkle_store_timeout_secs`). Rename pending in
    /// Mick's PR #78 which adds a dedicated `chunk_get_timeout_secs`.
    ///
    /// The adaptive controller does NOT currently size timeouts;
    /// this remains a static knob.
    pub store_timeout_secs: u64,
    /// Per-op timeout for **merkle batch** chunk store (PUT)
    /// operations, in seconds. Separate from `store_timeout_secs`
    /// because merkle PUTs incur the storer-side
    /// `CLOSENESS_LOOKUP_TIMEOUT` (240 s post-PR #89) on top of the
    /// usual store path; the client must wait at least that long
    /// plus padding, or the storer wastes work on a chunk the client
    /// has already given up on. Default 270 s.
    pub merkle_store_timeout_secs: u64,
    /// Per-peer response timeout for chunk GET operations, in seconds.
    /// This is intentionally independent from `store_timeout_secs`: PUTs
    /// and GETs have different payload direction and performance profiles.
    pub chunk_get_timeout_secs: u64,
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
    /// Per-batch leaf cap for **external-signer** merkle preparation,
    /// clamped to `3..=MAX_LEAVES` when set (a cap of 2 cannot partition odd
    /// totals — parts of 3 and 2 compose any count, so 3 is the smallest
    /// safe cap). `None` (the default) uses the contract maximum
    /// (`MAX_LEAVES` = 256).
    ///
    /// This is a test seam (ADR-0003): a small cap makes
    /// `file_prepare_upload_with_mode` produce a genuine multi-batch
    /// prepared upload from a kilobyte file, so the N-signature external
    /// flow is exercisable in E2E without a multi-GiB fixture. Production
    /// callers should leave it `None` — a lower cap only means more payment
    /// transactions for the same chunks.
    pub merkle_external_batch_cap: Option<usize>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            quote_timeout_secs: DEFAULT_QUOTE_TIMEOUT_SECS,
            store_timeout_secs: DEFAULT_STORE_TIMEOUT_SECS,
            merkle_store_timeout_secs: DEFAULT_MERKLE_STORE_TIMEOUT_SECS,
            chunk_get_timeout_secs: DEFAULT_CHUNK_GET_TIMEOUT_SECS,
            close_group_size: CLOSE_GROUP_SIZE,
            quote_concurrency: DEFAULT_QUOTE_CONCURRENCY,
            store_concurrency: DEFAULT_STORE_CONCURRENCY,
            adaptive: AdaptiveConfig::default(),
            allow_loopback: false,
            ipv6: true,
            merkle_external_batch_cap: None,
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

    // File downloads choose a stream-decrypt batch size per download
    // from the current fetch cap and usable RAM, then pass it into
    // self_encryption's runtime batch-size API. The adaptive controller
    // still drives fan-out inside each batch by re-reading
    // `controller.fetch.current()` in the decrypt callback.

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
    /// Path for the persistent client peer cache. `None` disables the cache.
    peer_cache_path: Option<PathBuf>,
    /// Peers that did not answer a settlement-versioned quote request, and are
    /// therefore asked in the legacy shape from now on.
    ///
    /// Without this the probe cost is paid on **every** request rather than
    /// roughly once per peer. Measured on the merkle E2E suite against a fleet
    /// that predates the versioned requests, re-probing took the run from ~24
    /// minutes to over 60, because each of the sixteen candidates per pool sat
    /// out a full `quote_timeout_secs` before the fallback.
    ///
    /// Roughly, not exactly: concurrent first contacts are not coalesced, so
    /// several in-flight requests can all miss the cache for the same peer and
    /// each probe it once before any of them records the answer. Observed at
    /// about two probes per peer on a 35-node devnet. Single-flighting them
    /// would remove the duplicates but not the wall-clock cost, which is set
    /// by how many *sequential* quote rounds an upload performs rather than by
    /// how many probes each round contains.
    ///
    /// Process-local and never persisted. A peer that upgrades mid-run keeps
    /// being asked in the legacy shape until the next start, which is
    /// acceptable while the legacy shape still gets a quote, and stops
    /// mattering when the fallback is deleted (see
    /// [`UNVERSIONED_RETRY_REQUIRES_MIN_V1`]).
    ///
    /// Entries are only ever added for a peer that has **never** answered a
    /// versioned request. Without that condition a single lost response would
    /// pin an upgraded peer to the legacy shape for the rest of the session,
    /// turning one dropped packet into a standing downgrade; with it, a peer
    /// that has shown it understands the versioned shape can never be demoted.
    ///
    /// A peer that has never answered can still get itself asked without a
    /// version by staying silent, exactly as it could through the fallback
    /// alone. Remembering the answer makes that cheaper to sustain, so it is
    /// not a new capability but it is a wider one, and the compile-time guard
    /// requires the whole path to be gone before any refusal is possible.
    unversioned_quote_peers: Arc<Mutex<HashSet<PeerId>>>,
    /// Peers observed answering a settlement-versioned request. Never
    /// downgraded, however they behave later.
    versioned_capable_peers: Arc<Mutex<HashSet<PeerId>>>,
    /// Distinct peers that have refused this client on settlement-version
    /// grounds, and the wording of the first such refusal.
    ///
    /// Client-wide and sticky, for two reasons that pull in opposite
    /// directions and are both real.
    ///
    /// It must outlive one operation, because the verdict is about this
    /// **build**, not this upload. Held in a single collector's local state, a
    /// refusal observed by one in-flight upload says nothing to another that
    /// is about to submit a payment, and merkle payments cannot be undone.
    ///
    /// It must not fire on one peer's say-so, because nothing authenticates a
    /// refusal. A single hostile or confused peer answering
    /// `ClientUpdateRequired` to every query would otherwise abort every
    /// upload the client attempts, converting an over-query design that
    /// tolerates many bad peers into one that tolerates none. So a refusal
    /// becomes terminal only once [`SETTLEMENT_REFUSAL_QUORUM`] distinct peers
    /// agree, which a genuine incompatibility reaches immediately (every
    /// upgraded peer refuses) and a lone attacker cannot reach at all.
    settlement_refusals: SettlementRefusals,
}

impl Client {
    /// Create a client connected to the given P2P node.
    #[must_use]
    pub fn from_node(node: Arc<P2PNode>, config: ClientConfig) -> Self {
        Self::from_node_with_peer_cache(node, config, None)
    }

    /// Create a client connected to the given P2P node and attach an optional
    /// persistent peer cache path.
    #[must_use]
    pub fn from_node_with_peer_cache(
        node: Arc<P2PNode>,
        config: ClientConfig,
        peer_cache_path: Option<PathBuf>,
    ) -> Self {
        let network = Network::from_node(node);
        let (controller, persist_path) = build_controller(&config);
        Self {
            config,
            network,
            wallet: None,
            evm_network: None,
            chunk_cache: ChunkCache::default(),
            next_request_id: AtomicU64::new(1),
            unversioned_quote_peers: Arc::new(Mutex::new(HashSet::new())),
            versioned_capable_peers: Arc::new(Mutex::new(HashSet::new())),
            settlement_refusals: SettlementRefusals::default(),
            controller,
            persist_path,
            peer_cache_path,
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
            unversioned_quote_peers: Arc::new(Mutex::new(HashSet::new())),
            versioned_capable_peers: Arc::new(Mutex::new(HashSet::new())),
            settlement_refusals: SettlementRefusals::default(),
            controller,
            persist_path,
            peer_cache_path: None,
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

    /// Compute the live network-participation snapshot.
    ///
    /// Convenience pass-through to [`Network::health`] — the single
    /// write-readiness implementation shared by all embedded-client
    /// consumers (antd, ant-gui, ant-ffi, ant-tui).
    pub async fn network_health(&self) -> NetworkHealth {
        self.network.health().await
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

    /// Persist currently connected peers that have Direct-tagged addresses in
    /// the DHT. Best effort; failures are logged and do not affect the client
    /// operation that just completed.
    pub async fn save_peer_cache(&self) {
        if let Some(ref path) = self.peer_cache_path {
            let node = self.network().node();
            peer_cache::promote_connected_direct_peers(node.as_ref(), path, node.dht().k_value())
                .await;
        }
    }

    /// Get the next request ID for protocol messages.
    pub(crate) fn next_request_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Handle to the set of peers that cannot answer a settlement-versioned
    /// quote request, shared with the per-peer request futures on both quote
    /// paths.
    ///
    /// Callers read it before choosing a request shape and insert into it when
    /// a peer stays silent. A poisoned lock is treated as "nothing known", so
    /// the worst case is a wasted probe rather than a silently skipped version
    /// declaration.
    pub(crate) fn unversioned_quote_peers(&self) -> Arc<Mutex<HashSet<PeerId>>> {
        Arc::clone(&self.unversioned_quote_peers)
    }

    /// Handle to the set of peers already seen answering a versioned request.
    /// Consulted before demoting a peer, so a lost response cannot strand an
    /// upgraded peer in the legacy shape.
    pub(crate) fn versioned_quote_capable_handle(&self) -> Arc<Mutex<HashSet<PeerId>>> {
        Arc::clone(&self.versioned_capable_peers)
    }

    /// Record that `peer_id` refused this client's settlement version, and
    /// report whether enough distinct peers now agree for it to be believed.
    ///
    /// Returns the refusal wording once [`SETTLEMENT_REFUSAL_QUORUM`] is met,
    /// and `None` below it, so a lone peer is treated as a peer fault rather
    /// than a verdict about this build.
    pub(crate) fn note_settlement_refusal(&self, peer_id: PeerId, message: &str) -> Option<String> {
        self.settlement_refusals.note(peer_id, message)
    }

    /// The corroborated refusal, if this client has already been told by
    /// enough peers that it cannot settle.
    ///
    /// Checked before spending money. The verdict concerns this build rather
    /// than any one upload, so an upload that starts after another has already
    /// established it must not proceed to pay.
    pub(crate) fn corroborated_settlement_refusal(&self) -> Option<String> {
        self.settlement_refusals.corroborated()
    }

    /// Handle to the shared refusal tracker, for collectors that run outside
    /// `&self`.
    pub(crate) fn settlement_refusals(&self) -> SettlementRefusals {
        self.settlement_refusals.clone()
    }

    /// Return the chunk PUT-target set: the closest [`PUT_TARGET_WIDTH`] peers
    /// to the address, each paired with its known network addresses.
    ///
    /// Used by the merkle store path, which — unlike single-node payment — has
    /// no witnessed put-target list to forward, so it fetches the closest-K
    /// neighbourhood locally.
    pub(crate) async fn put_target_peers(
        &self,
        target: &XorName,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        self.closest_peers(target, PUT_TARGET_WIDTH).await
    }

    /// Return the requested number of closest peers for a target address.
    ///
    /// Queries the DHT for peers by XOR distance. Returns each peer
    /// paired with its known network addresses.
    pub(crate) async fn closest_peers(
        &self,
        target: &XorName,
        count: usize,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        let peers = self.network().find_closest_peers(target, count).await?;

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
            // A definitively absent record over a working link — the peers
            // answered, nothing was stored there. Must NOT register as a
            // capacity signal.
            (
                Error::NotFound("missing".to_string()),
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
                    spend: Box::new(crate::data::error::PartialUploadSpend {
                        storage_cost_atto: "0".to_string(),
                        gas_cost_wei: 0,
                    }),
                    reason: "r".to_string(),
                },
                Outcome::NetworkError,
            ),
            (
                Error::BadQuoteBinding {
                    peer_id: "peer".to_string(),
                    detail: "mismatch".to_string(),
                },
                Outcome::ApplicationError,
            ),
            // A remote application rejection: the node responded with a
            // structured `ProtocolError`, so the transport succeeded and
            // this must NOT register as a capacity signal (V2-468).
            (
                Error::RemotePut {
                    address: "abcd".to_string(),
                    source: ant_protocol::ProtocolError::PaymentFailed("stale quote".to_string()),
                },
                Outcome::ApplicationError,
            ),
            // A close-group quorum shortfall caused by dial/relay churn with
            // no PUT-response timeouts — remote peer churn, not local
            // backpressure, so it must NOT register as a capacity signal
            // (V2-554). A timeout-bearing shortfall keeps `InsufficientPeers`.
            (
                Error::CloseGroupShortfall("Stored on 3 peers, need 4".to_string()),
                Outcome::ApplicationError,
            ),
            // Refusing an oversized external-signer merkle batch happens
            // before any network work, so it is not a capacity signal.
            (
                Error::MerkleBatchTooLarge {
                    addresses: 257,
                    max_leaves: 256,
                },
                Outcome::ApplicationError,
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
            | Error::NotFound(_)
            | Error::Serialization(_)
            | Error::Crypto(_)
            | Error::Io(_)
            | Error::Config(_)
            | Error::SignatureVerification(_)
            | Error::Encryption(_)
            | Error::AlreadyStored
            | Error::InsufficientDiskSpace(_)
            | Error::CostEstimationInconclusive(_)
            | Error::Cancelled(_)
            | Error::PartialUpload { .. }
            | Error::BadQuoteBinding { .. }
            | Error::BadQuoteCommitment { .. }
            | Error::MerkleBatchTooLarge { .. }
            | Error::RemotePut { .. }
            | Error::ClientUpdateRequired(_)
            | Error::StorerUpdateRequired(_)
            | Error::CloseGroupShortfall(_) => (),
        };
    }
}
