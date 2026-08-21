# Changelog

All notable changes to the `ant` binary will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Changed (wallet path — batched merkle payments, V2-990)
- Wallet-path merkle uploads larger than one tree (`MAX_LEAVES` = 256 chunks ≈ 1 GiB)
  now settle in batched `payForMerkleTrees` transactions: sub-batches are paid in
  groups of `MERKLE_TREES_PER_PAYMENT` (4) trees per on-chain transaction instead of
  one transaction per tree. Partial-payment semantics are preserved at group
  granularity — a failed group still returns the proofs of previously-paid groups,
  and the failed group itself pays nothing (the batched entry point is atomic).
  Requires a payment vault deployment carrying the batched entry point (V2-992);
  the cap is re-exported as `ant_core::data::MERKLE_TREES_PER_PAYMENT` so
  consumers don't hardcode it.

### Changed (breaking — external-signer merkle API, ADR-0003)
- External-signer merkle uploads are no longer capped at one payment batch (`MAX_LEAVES` = 256 chunks ≈ 1 GiB): `file_prepare_upload*` now partitions the to-upload set into `MerkleTree`-sized sub-batches (`ExternalPaymentInfo::Merkle` carries `prepared_batches: Vec<PreparedMerkleBatch>`), the signer pays one transaction per batch, and the new `Client::finalize_upload_merkle_multi` takes one winner hash per batch. `finalize_upload_merkle` remains as the single-batch special case. A batch the signer never paid (`None` hash) no longer aborts the upload: paid batches store and the unpaid chunks surface via `Error::PartialUpload`.
- External-signer merkle prepared uploads no longer hold the encrypted file in memory: chunk bodies stay in the on-disk encryption spill (opaque `ExternalChunkStore` inside `ExternalPaymentInfo::Merkle`, replacing the resident `chunk_contents: Vec<Bytes>`), and finalize stores them via the wallet path's bounded spill fan-out — peak RAM ~256 MB regardless of file size, plus deferred-retry rounds the external path previously lacked.

### Added
- `Client::file_prepare_upload_with_mode`: external-signer prepare with an explicit `PaymentMode` override, mirroring the wallet path's `file_upload_with_mode`.
- `ClientConfig::merkle_external_batch_cap`: clamped test seam (`3..=MAX_LEAVES`) so E2E tests exercise real multi-batch external signing with kilobyte files.

### Fixed
- External-signer merkle finalize (`Client::finalize_upload_merkle`) now returns `Error::PartialUpload` when chunks remain short of quorum after all retries, matching the wave-batch finalize. Previously it returned `Ok` with `chunks_failed > 0`, which callers (desktop app, mobile FFI) took as success — reporting a paid but not fully retrievable file as complete (#166).

### Changed
- Default network binding changed from IPv4-only to IPv6 dual-stack. Hosts without a working IPv6 stack should pass `--ipv4-only` to avoid advertising unreachable v6 addresses to the DHT (which causes slow connects and junk address records).
- `ant file upload` now writes datamaps as `<filename>.<extension>.datamap` instead of stripping the extension. Uploading `photo.jpg` produces `photo.jpg.datamap` (was `photo.datamap`). Existing datamaps remain readable.
- `ant file upload` no longer silently overwrites an existing datamap. Repeated uploads of the same source path produce `name-2.datamap`, `name-3.datamap`, … capped at 100 attempts. Pass `--overwrite` to restore the previous behaviour.

### Added
- `ant file upload --overwrite`: replace any existing `<filename>.datamap` rather than writing a suffixed sibling.
- `ant file download --datamap` no longer requires `-o/--output` — defaults to the original filename derived from the datamap basename (`photo.jpg.datamap` → `photo.jpg`, written to the current directory). Pass `-o` to override.
- `ant file download --datamap` now reads both msgpack (canonical) and legacy JSON datamaps, so datamaps produced by older versions of the GUI download cleanly via the CLI.

### Internal
- New `ant_core::datamap_file` module owns the on-disk datamap format (msgpack canonical, JSON legacy auto-detect on read) and naming convention. `ant-cli` and consumers like `ant-gui` route through this single helper instead of reimplementing serialization.

## [0.1.1] - 2026-03-28

### Added
- Node management: `ant node add`, `ant node start`, `ant node stop`, `ant node status`, `ant node reset`
- Daemon management: `ant node daemon start`, `ant node daemon stop`, `ant node daemon status`
- Data operations: `ant file upload`, `ant file download`, `ant chunk put`, `ant chunk get`
- Wallet management: `ant wallet address`, `ant wallet balance`
- Automatic bootstrap peer loading from `bootstrap_peers.toml` config file
- `--json` global flag for structured output
- Cross-platform support (Linux, macOS, Windows)
