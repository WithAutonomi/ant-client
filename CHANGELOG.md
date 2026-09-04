# Changelog

All notable changes to the `ant` binary will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `ant` is distributed on npm as `@withautonomi/ant`, so `npm install -g @withautonomi/ant`
  works in agent sandboxes and CI runners that allow package-manager traffic but block direct
  binary downloads (V2-1152). Packaging only: the published packages carry the same signed
  binaries as the GitHub release, verified against `SHA256SUMS.txt` and their ML-DSA-65
  signatures during packaging, and published from the release workflow with npm provenance. A
  per-platform companion package guarded by `os`/`cpu` means only one binary is downloaded. The
  `beta` dist-tag tracks beta releases; `install.sh`, `install.ps1` and the release assets are
  unchanged. See [`npm/README.md`](npm/README.md).
- `ant` falls back to a bootstrap peer list compiled into the binary when no
  `bootstrap_peers.toml` is present in the config directory. Priority is unchanged — explicit
  `-b` peers, then a devnet manifest, then the config file, then the embedded list — and an
  explicitly selected devnet manifest still errors rather than reaching for mainnet peers. This
  is what makes an npm install usable out of the box: npm 12 blocks package install scripts by
  default, so the step that copies that file into the config directory frequently does not run.
  `bootstrap_peers.toml` moved from `resources/` to `ant-core/resources/`, since `include_str!`
  cannot reach outside the crate and ant-core is published to crates.io; the release archives
  carry the same bytes as before.

### Changed
- `ant update` no longer replaces the binary when `ant` was installed by a package manager. It
  still reports whether a newer version is available, then prints the command that will install
  it (`npm update -g @withautonomi/ant`). Self-replacing an npm-managed binary left npm's
  metadata describing a file that was no longer there, so the next `npm update` silently rolled
  the user back.

## [0.3.6] - 2026-09-01

### Fixed
- A client whose routing table has been starved of peers now recovers at runtime
  instead of needing a restart. The fix is in `saorsa-core` 0.27.3 and reaches `ant`
  through this release's dependency uplift — see
  [WithAutonomi/saorsa-core#153](https://github.com/WithAutonomi/saorsa-core/pull/153) (V2-1036).

### Internal
- Release notes are now extracted per version rather than always taking the first
  CHANGELOG section, so each release ships its own notes (#183).

## [0.3.5] - 2026-08-31

### Added
- Resumable external-signer finalize for **both** payment paths, so a post-payment storage shortfall no longer strands the payment (#172). `Client::finalize_upload_resumable` (wave-batch) and `Client::finalize_upload_merkle_multi_resumable` (merkle) — each with a `_with_progress` variant — return a `FinalizeOutcome`: `Complete(FileUploadResult)`, or `Partial { result, resume }` carrying an opaque `FinalizeResume` handle (`Wave` / `Merkle`) that owns the already-paid material (the wave path's paid chunks, or the merkle path's on-disk spill + signed proofs). `Client::finalize_resume` (+ `_with_progress`) takes that handle and re-drives storage for only the still-unstored chunks against the **same** on-chain payment — no re-quoting, no second signature, no double payment — and is loopable until `Complete` (bound the loop: persistent store failures return `Partial` on every call, never `Err`). The merkle resumable finalize requires **every** sub-batch to be paid — a resume handle cannot acquire proofs for unpaid chunks, so a partial payment is rejected up front with a pointer at the non-resumable path. The existing consuming `finalize_upload` / `finalize_upload_merkle_multi` are unchanged (they still accept partial payment and surface a shortfall as `Error::PartialUpload`).
- `NetworkHealth`: one implementation of the write-readiness/peer-count computation for every embedded-client consumer (antd, ant-gui, ant-ffi, ant-tui), moved down from antd (#174). `Network::health` and `Client::network_health` return `NetworkHealth { write_ready, connected_peers, routing_table_size, rebootstrap_threshold }`, where `write_ready` is `max(routing_table_size, connected_peers) >= rebootstrap_threshold` — a client-mode routing table under-reports while stores still succeed, and the connected count alone misses the dead-network case. Both node reads are in-memory, so the snapshot is cheap enough to compute per call.

### Fixed
- `ant node start`/`ant node stop` with `--service-name` now resolve the node ID through the daemon API instead of reading `node_registry.json` directly, eliminating a race against concurrent registry mutations by the daemon.
- `ant node add --json` no longer interleaves binary-download progress with the JSON result; progress output now goes to stderr (was stdout) and is suppressed entirely in JSON mode.

### Changed
- `--evm-network` still defaults to `arbitrum-one`, **except** when a devnet manifest carrying an EVM block is loaded: that combination now errors and asks for an explicit choice (`local` to use the manifest, or a preset to override it). The old behavior silently overrode the manifest's EVM config, producing no-op mainnet-vault transactions on other chains that spent gas and set a useless ANT allowance before every chunk PUT failed payment verification. An explicit preset selected alongside a manifest EVM block now prints a warning that the manifest's EVM config is ignored. No change for mainnet users or read-only operations.

### Internal
- CLI-audit thinning (V2-189): `PortRange` parsing (`FromStr`), env `KEY=VALUE` parsing (`AddNodeOpts::parse_env_vars`), and bootstrap-peer resolution (`config::resolve_bootstrap_peers`) moved from ant-cli into ant-core; `node add`/`node reset` daemon calls now go through `ant_core::node::daemon::client` (new `add_node`/`reset`/`resolve_node_id_by_name` functions) instead of hand-rolled HTTP; the two CLI `ProgressReporter` impls collapsed into one.

## [0.3.4] - 2026-08-27

### Changed (breaking — ant-core update API)
- The `ant` client is beta-channel aware (#173): a `-beta.N` build tracks the beta channel by default — `ant update` takes later `-beta.N` builds and stable releases, never a release candidate — while stable builds keep tracking stable. `ANT_CHANNEL` / `ANT_VERSION` environment variables override channel and target-version resolution. Breaking on three `ant-core` public items (`resolve_binary`, `check_for_update`, `UpdateCheck`); the JSON output surface is additive.

## [0.3.3] - 2026-08-12

### Changed (breaking — external-signer merkle API, ADR-0003)
- External-signer merkle uploads are no longer capped at one payment batch (`MAX_LEAVES` = 256 chunks ≈ 1 GiB): `file_prepare_upload*` now partitions the to-upload set into `MerkleTree`-sized sub-batches (`ExternalPaymentInfo::Merkle` carries `prepared_batches: Vec<PreparedMerkleBatch>`), the signer pays one transaction per batch, and the new `Client::finalize_upload_merkle_multi` takes one winner hash per batch. `finalize_upload_merkle` remains as the single-batch special case. A batch the signer never paid (`None` hash) no longer aborts the upload: paid batches store and the unpaid chunks surface via `Error::PartialUpload`.
- External-signer merkle prepared uploads no longer hold the encrypted file in memory: chunk bodies stay in the on-disk encryption spill (opaque `ExternalChunkStore` inside `ExternalPaymentInfo::Merkle`, replacing the resident `chunk_contents: Vec<Bytes>`), and finalize stores them via the wallet path's bounded spill fan-out — peak RAM ~256 MB regardless of file size, plus deferred-retry rounds the external path previously lacked.

### Added
- `Client::file_prepare_upload_with_mode`: external-signer prepare with an explicit `PaymentMode` override, mirroring the wallet path's `file_upload_with_mode`.
- `ClientConfig::merkle_external_batch_cap`: clamped test seam (`3..=MAX_LEAVES`) so E2E tests exercise real multi-batch external signing with kilobyte files.

### Fixed
- External-signer merkle finalize (`Client::finalize_upload_merkle`) now returns `Error::PartialUpload` when chunks remain short of quorum after all retries, matching the wave-batch finalize. Previously it returned `Ok` with `chunks_failed > 0`, which callers (desktop app, mobile FFI) took as success — reporting a paid but not fully retrievable file as complete (#166).

## [0.3.2] - 2026-07-29

_No entries were recorded at release time. Backfill from the release-train manifests is tracked in Linear V2-1107._

## [0.3.1] - 2026-07-23

_No entries were recorded at release time. Backfill from the release-train manifests is tracked in Linear V2-1107._

## [0.3.0] - 2026-07-09

_No entries were recorded at release time. Backfill from the release-train manifests is tracked in Linear V2-1107._

## [0.2.2 – 0.2.11]

_Ten releases between 2026-04-30 and 2026-07-08; no entries were recorded for them. Backfill is tracked in Linear V2-1107._

## [0.2.1] - 2026-04-30

### Changed
- `ant file upload` now writes datamaps as `<filename>.<extension>.datamap` instead of stripping the extension. Uploading `photo.jpg` produces `photo.jpg.datamap` (was `photo.datamap`). Existing datamaps remain readable.
- `ant file upload` no longer silently overwrites an existing datamap. Repeated uploads of the same source path produce `name-2.datamap`, `name-3.datamap`, … capped at 100 attempts. Pass `--overwrite` to restore the previous behaviour.

### Added
- `ant file upload --overwrite`: replace any existing `<filename>.datamap` rather than writing a suffixed sibling.
- `ant file download --datamap` no longer requires `-o/--output` — defaults to the original filename derived from the datamap basename (`photo.jpg.datamap` → `photo.jpg`, written to the current directory). Pass `-o` to override.
- `ant file download --datamap` now reads both msgpack (canonical) and legacy JSON datamaps, so datamaps produced by older versions of the GUI download cleanly via the CLI.

### Internal
- New `ant_core::datamap_file` module owns the on-disk datamap format (msgpack canonical, JSON legacy auto-detect on read) and naming convention. `ant-cli` and consumers like `ant-gui` route through this single helper instead of reimplementing serialization.

## [0.2.0] - 2026-04-24

### Changed
- Default network binding changed from IPv4-only to IPv6 dual-stack. Hosts without a working IPv6 stack should pass `--ipv4-only` to avoid advertising unreachable v6 addresses to the DHT (which causes slow connects and junk address records).

## [0.1.1] - 2026-03-28

### Added
- Node management: `ant node add`, `ant node start`, `ant node stop`, `ant node status`, `ant node reset`
- Daemon management: `ant node daemon start`, `ant node daemon stop`, `ant node daemon status`
- Data operations: `ant file upload`, `ant file download`, `ant chunk put`, `ant chunk get`
- Wallet management: `ant wallet address`, `ant wallet balance`
- Automatic bootstrap peer loading from `bootstrap_peers.toml` config file
- `--json` global flag for structured output
- Cross-platform support (Linux, macOS, Windows)
