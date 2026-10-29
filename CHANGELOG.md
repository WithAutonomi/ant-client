# Changelog

All notable changes to the `ant` binary will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `data::Error::InsufficientFunds { have, need }` (amounts in atto): a wallet whose payment-token
  balance is below the payment total now fails with this instead of a generic
  `Error::Payment(String)`, so consumers such as the mobile SDK can offer "top up your wallet"
  without matching on message text (#152). It covers every wallet-backed payment path: the
  journaled native path that file and data uploads use, the wave-batch and Merkle batch paths, and
  single-chunk payment. Nothing has been paid when it is returned, because the wallet checks its
  balance before sending. A shortfall after earlier batches of the same upload were paid still
  surfaces as `PartialUpload`, so the spend and the stored chunks are still reported, and it is now
  kept typed in the new `PartialUpload::cause` field (also set when storing the paid batches
  failed too). Code that builds `PartialUpload` or matches it without `..` must add the field. The
  CLI's message changes from `payment error: Insufficient tokens to pay for quotes. Have: …` to
  `insufficient funds: wallet has … atto, payment needs … atto`. A shortfall of gas (ETH) is not
  detected up front and is unchanged.

## [0.3.12] - 2026-10-22

### Changed (breaking — `ant-core` browser API)
- In the browser, `createPointer`, `updatePointer` and `transferPointer` take their callbacks as
  an options object, `{ onPrepared, onSubmission, onPaid }`; a bare `onPaid` function as the last
  argument is no longer accepted. In `ant-core`, `ManifestInput::create` returns a typed
  `ManifestInputError` instead of a `String` (#230).

### Added
- Uploads report the file's SHA-256 (V2-1564). `ant file upload` prints it (and `--json` adds
  `sha256`); it is computed in the same read that feeds the encryptor, so it costs no extra pass.
  Manifest entries (ADR-0006) gain an optional `sha256`: uploads record it in the upload history
  and in `ant manifest create` manifests, `ant manifest show` prints it, and
  `ant manifest download` checks every downloaded file against it, failing the entry (and leaving
  nothing at the target) on a mismatch. Entries without it encode exactly as before, and older
  readers ignore it. In `ant-core`: `FileUploadResult::sha256`, `PreparedUpload::sha256`,
  `DataUploadResult::sha256`, `ManifestEntry::sha256` and `ManifestError::Sha256Mismatch`.
- Browser manifest export and pointer payment recovery (ADR-0007, proposed; V2-1478, V2-1354).
  `createManifest` and `createFileLink` build the same `.ant` manifests and `ant://` links as the
  CLI, which reads them unchanged. A pointer write can checkpoint its signed state and verified
  plan through `onPrepared` before anything is paid, and `onSubmission` reports each transaction
  the wallet broadcasts. If the page is lost between payment and receipt, `recoverPointerPayment`
  rebuilds the paid proof from that checkpoint and the transaction hash without paying again, and
  `storePaidPointer` stores it (#230).

### Changed
- Reads spread their first request for a record across the whole close group, instead of every
  client asking the nearest holder first, so many clients reading the same data no longer pile
  onto one node (ADR-0023 in `ant-node`; V2-1508, #225). Pointer reads already asked the whole
  group and are unaffected.
- In the browser, a node that declines a connection because it is full is skipped for 15 s
  rather than the 30-minute dial-failure cooldown, and a read whose holders are all declining
  waits them out (up to three times) and then fails with "the nodes that may hold chunk … are
  busy; try again shortly" instead of reporting the record not found. Native reads are
  unchanged. `BrowserNetwork` gains `refused_peers()`, with a default that reports none (#225).

### Security
- `rustls` 0.23.46, for RUSTSEC-2026-0346, -0347, -0348 and -0349 (V2-1595, #232). Of the four,
  only RUSTSEC-2026-0349 applies to `ant`'s configuration: an on-path attacker could make later
  resumed connections to a server name use classical-only key exchange.

## [0.3.11] - 2026-10-16

### Added
- Pointer ownership transfer (ADR-0018 in `ant-node`). `ant pointer transfer --key <FILE>
  <RECIPIENT>` signs the pointer's final state, at counter `u64::MAX`, pointing at the new
  owner's pointer: readers of the address are redirected there by every node that holds it, the
  address stays the same, and no node that holds it gives it up, because a final state is
  replaced by nothing. It is refused before paying if any peer of the close group holds a final
  state for the pointer, if not every peer answers, or if the recipient does not exist or leads
  back. `ant pointer finality <ADDRESS>` asks the whole close group whether a pointer is `open`,
  `settling`, `unconfirmed` (final on a majority, but not every peer answered), `final` or
  `forked`, which is what a recipient checks before relying on a transfer, and `ant pointer controller <ADDRESS>` follows transfers to the pointer that now
  decides. In `ant-core`: `Client::pointer_transfer`, `pointer_sign_transfer`,
  `pointer_finality` and `pointer_controller`, and the new `Error::PointerFinal` and
  `Error::PointerForked`. In the browser: `transferPointer` and `pointerFinality`.
- File manifests, `ant://` links and an upload history (ADR-0006, V2-1478). A manifest is a `.ant`
  file listing a set of files together with what is needed to download them (a public address or
  an embedded DataMap per entry). It is never stored on the network, so it can be kept or shared
  as a file or as an `ant://manifest/...` link. `ant manifest create` uploads files or
  directories and writes the manifest (`--public` also stores each DataMap chunk, `--public-file`
  and `--embed-public` add files that are already public by address, `--link` also prints the
  link). `ant manifest list` shows every recorded upload; `show` and `link` take a `.ant` file, a
  link or an upload id; `export` writes a manifest out as a `.ant` file, and `--compact` swaps
  embedded DataMaps for public addresses (publishing DataMaps that are still private is paid and
  asks first); `download` extracts the files, all of them or those named with `--select`. Paths
  are checked for portability and containment before anything is written. `ant file upload` now
  records every upload in the history (the record id is printed, and `manifest_id` is added to
  `--json`), and `ant file download` accepts an `ant://<hex>` link and, with `-o` omitted, names
  the output after the address (it was an error). In `ant-core`: the `ant_core::data::manifest`
  module (types, encode/decode, links, builder, extraction, history, compaction), and
  `Client::file_upload_with_visibility_and_progress` is now public. In the browser:
  `parseManifestLink` and `decodeManifest`, whose entries feed `downloadPrivateFile` and
  `openPrivateFile`. (#219)
- In the browser: `BrowserFileReader.pipeTo(writable, { start?, end?, signal?, onProgress? })`
  for public and private files streams a file of any size to a `WritableStream` (for example one
  in the origin private file system) with backpressure and cancellation, holding at most 32 MiB
  of plaintext at a time. `downloadPublicFile` and `downloadPrivateFile` take a trailing
  `{ maxMemoryBytes?, signal? }` argument: a download over the memory budget fails before any
  transfer and points to `pipeTo`, and `signal` cancels it. Files over 1 GB are no longer
  rejected, file offsets are 64-bit throughout browser reads (they were 32-bit on wasm32), and
  manifest validation accepts up to 262,144 chunks. Chunk positions and decryption now come from
  `self_encryption::DataMapIndex` (`self_encryption` 0.36.2). (V2-1398, #214)

### Changed (breaking — browser WASM API)
- `PublicFileDescriptor.size` is `u64` rather than `usize`, and the WASM exports that carry sizes
  and offsets change with it. Existing JavaScript call forms keep working, but sizes and offsets
  are now validated as non-negative safe integers: `readRange` rejects fractional, negative,
  missing or imprecise arguments that were previously coerced (an omitted length read nothing,
  and `1.5` read from byte 1). A DataMap whose range reads are rejected, such as one with
  non-contiguous indices, now fails at `openPublicFile`/`openPrivateFile` instead of opening with
  read-ahead off, and `BrowserFileReader.close()` releases that reader's records without
  clearing the client's shared chunk cache. Native `data_download_range` is unchanged. (V2-1398,
  #214)

### Changed
- A pointer read that meets a final state returns the final state a majority of the close group
  holds, rather than the merge winner, and asks the whole group until one is found. Two
  different final states are a fork only the owner can make, by racing them to different nodes;
  with no majority on either, the read fails with `Error::PointerForked` instead of guessing.
  Updating a final pointer is refused with `Error::PointerFinal`, before anything is paid.
- Concurrent reads of the same chunk share one fetch per `Client`, native and WASM alike.
  `chunk_get`, `chunk_get_from_closest_peers` and the observed download path send one GET per
  address while a fetch is in flight; later callers wait for its result, and nothing is kept
  after it ends, so caching stays the chunk cache's job. A failed shared fetch fails every waiter
  with the same error and each caller's own retry policy still applies; if the caller running the
  fetch is dropped, a waiter runs it instead. Downloads with diagnostics enabled neither share
  nor lend a fetch, so their records describe only their own GETs. This removes duplicate GETs
  where reads overlap, such as parallel ranged reads of one object (part boundaries, and every
  part resolving the same DataMap chunks): on a 25-node devnet, a cold 64 MiB download through
  antd's S3 gateway went from 82-99 to 46-49 GETs received by the nodes, with the same bytes.
  No signature changes; the browser's read-ahead readers share fetches too. (V2-1433, #218)

### Security
- `quinn-proto` is bumped from 0.11.15 to 0.11.19 in `Cargo.lock`, clearing RUSTSEC-2026-0338,
  0339, 0340, 0341 and 0342. Nothing in this workspace builds `quinn` or `quinn-proto` (they
  appear in the lockfile only as `reqwest`'s optional dependency), so the shipped binaries were
  not exposed and are unchanged; the bump clears the Security Audit job. (V2-1527, #227)

### Internal
- CI: release-promotion PRs (`chore(release):` titles from `rc-*` branches) are exempt from the
  `linear-link` and `pr-template` checks. (V2-1092, #181)

## [0.3.10] - 2026-10-08

### Added
- Licence texts and third-party notices now ship with the CLI. The release archives and the npm
  platform packages gain `LICENSE-MIT`, `LICENSE-APACHE`, `THIRD-PARTY-NOTICES.txt` (generated
  from `Cargo.lock` for each target, with the licence, copyright and notice files of every
  statically linked crate, OpenSSL and other bundled C code) and `RUST-STD-COPYRIGHT.html`; the
  npm meta package gains the two licence files. The `ant` binary is unchanged, and `ant update`,
  `install.sh` and `install.ps1` pick entries from the archive by name, so the extra files do not
  affect them. (V2-1392, #213)
- In the browser: `BrowserFileReader` reads ahead during sequential reads, so media keeps
  streaming while records are still being discovered. A sequential read fetches up to four
  records of the next 32 MiB in parallel (a streaming reader also fetches the last record, where
  a video's index usually is), within bounds that do not trust DataMap metadata: at most 24
  records held or in flight across all readers of a client, and speculative GETs never take more
  than half of the read budget. `openPublicFile` and `openPrivateFile` take an optional third
  argument `{ streaming: true }`; existing callers are unaffected. Read-ahead records are kept
  out of the shared chunk cache. (V2-1355, #209)

### Changed
- Native and WASM reads: once a record's early-GET allowance is used up, a hint with fewer than
  `CLOSE_GROUP_SIZE` attempted peers nearer the target stays eligible, so a slow lookup that
  finally names a record's actual holders is acted on at once instead of after discovery ends.
  Speculative GETs are capped at twice the allowance, and the two-GET limit, hedge delay and
  absence rules are unchanged. (V2-1355, #209)
- In the browser: DHT lookups query six peers per round rather than three
  (`BROWSER_LOOKUP_ALPHA`). In headless Chromium against mainnet, the mean cold read of a 612 MB
  video's records dropped from 11.5 s to 6.1 s and quoting an 8 MB upload from 155 s to 120 s, at
  the cost of about 40% more connections. Native lookups, the grace period and the dial timeout
  are unchanged. (V2-1357, #211)

### Fixed
- A pointer read could return the state a successful write had replaced: a write lands on five
  of the seven peers, and three peers replaying the older state could answer first. A read now
  keeps asking while a state that would replace its answer has been named by too few peers, until
  that state is backed by two peers or the group is exhausted. A newer state only one peer names
  still never wins, so no single peer decides what a pointer says. (V2-1363, #208)
- Pointer write and read quorums are counted over the configured close group rather than over
  however many peers a lookup returned, so a lookup that comes back short is a shortfall instead
  of a completed write or read. One peer answering that the pointer "had moved on" no longer ends
  a write that other peers stored, and a native write no longer drops the proof it paid for: a
  read confirms the move first, any other refusal is final only when no peer stored the record
  and a second peer refused as well, and a node's own storage error (a full disk, for example) is
  not a refusal. Anything short of that is retried with the proof already paid for. (#208)
- In the browser: `createPointer` and `updatePointer` no longer call the wallet for a write that
  cannot land. While nodes are being upgraded, some of a close group do not advertise pointers
  and the browser never sends them one; before quoting, the client now asks each member whether
  it would take a pointer write and refuses once a write quorum cannot be reached. A node that
  cannot be asked within 20 seconds is not counted against the write. A paid state is also
  stored when the page's `onPaid` callback never settles (after 30 seconds), and
  `ant pointer resolve` prints the kind when the target is not a chunk. In `ant-core`: the `BrowserNetwork`
  trait gains the provided method `accepts_pointer_writes`, which answers `true` by default, so
  existing implementations compile and behave as before. (#208)
- In the browser: connections the browser reports `failed` or `closed` are never reused, and a
  failed connection now fails all its lanes at once, so waiting requests are retried elsewhere
  instead of waiting out their 10 s response deadline. Safari (WebKit) leaves a lost
  connection's DataChannels reading `"open"`, which made downloads and media playback stall for
  minutes. (V2-1355, #215)
- In the browser: ICE gathering is taken one connection at a time, page-wide. WebKit allows 256
  WebRTC UDP sockets per page and silently closed established connections past that, and
  connections gathering at once opened 12-30 sockets each instead of 2. (V2-1355, #216)

### Internal
- CI: the browser integration job builds the pinned `ant-node` without `-D warnings`, moves the
  pin past its Rust 1.99 fix (the job had failed on every PR since stable moved to 1.99), refuses
  to start unless the sibling checkout is the pinned revision, and fails a run in which any test
  was skipped. (V2-1399, #217)

## [0.3.9] - 2026-10-01

### Added
- Pointers: a mutable reference to a chunk or to another pointer, signed by an owner key that can
  never change (ADR-0016 in `ant-node`). Every state is paid for; reads are free. The owner key
  is a 32-byte seed kept in a file only its owner can read (`0600` on Unix), and losing it means
  the pointer can never be updated again. New CLI commands: `ant pointer keygen --output <FILE>`
  (never overwrites an existing file), `ant pointer address --key <FILE>` (needs no network),
  `ant pointer create --key <FILE> <TARGET> [--kind chunk|pointer]` (refused before paying if the
  pointer already exists), `ant pointer update`, `ant pointer get <ADDRESS>` and
  `ant pointer resolve <ADDRESS>`, which follows a chain of pointers to the target at its end. In `ant-core`:
  `Client::pointer_create`, `pointer_update`, `pointer_put`, `pointer_get` and `pointer_resolve`,
  `pointer_sign_create` and `pointer_sign_update` for external payers, `prepare_pointer_payment`
  and `pointer_put_paid` for a proof paid outside this client, `ChunkPaymentPlan::proof` and
  `MAX_POINTER_RESOLVE_DEPTH`. An update signs one past the counter the network serves, each
  state is paid under its own identifier, and a read needs two peers to agree. In the browser:
  `getPointer`, `resolvePointer`, `createPointer`, `updatePointer` and `storePaidPointer` on
  `BrowserNetworkClient`, and `pointerAddress(seed)`; the browser only asks nodes whose HELLO
  advertises `pointer_protocol`. Additive: a client that never calls the pointer API sends
  nothing new. (V2-1277, #200)
- In the browser: the seven mainnet WebRTC Direct bootstrap addresses (certificate-pinned) are
  bundled in the `webrtc` list of `bootstrap_peers.toml` and exposed through WASM, so a browser
  client can connect to mainnet without supplying seeds; explicit seeds and profiles are still
  honoured. The native CLI reads the `quic` list and is unaffected. (V2-1305, #206)

### Changed (breaking — browser WASM API)
- `BrowserNodeClient` and `BrowserNodeSession` are no longer exported from the production WASM
  build: `BrowserNetworkClient` is the single browser networking client. Use it for
  authentication, discovery and transfers; a one-seed client supports authenticated HELLO checks.
  `BrowserNetworkClient.connect(expectedPayment)` now authenticates through the pool that later
  transfers use: at most four seed attempts run concurrently, the first authenticated seed that
  passes the capability and expected-payment checks makes the client ready, and discovery starts
  with the ready seeds and takes later candidates as they arrive. Rejected seeds cannot re-enter
  the pool through read or discovery fallback, and later callers can retry transient failures
  with bounded backoff. Native Rust APIs and the high-level SDK overloads keep their
  signatures. (V2-1305, #206)

### Fixed
- In the browser: a closed RPC session or failed response inbox no longer counts as a live
  connection, even while the DataChannel still reports open. The dead session is retired before
  admission is retried, so recovery waits for a fresh connection instead of uploaders spinning on
  a closed session for 400 seconds (the reproduction now completes in about 340 ms). (V2-1305,
  #206)
- Faster cold reads, native and WASM: the shared read scheduler allows a second early GET after a
  one-second stall while keeping the two-GET bound, deduplication, fallback allowance and fatal
  integrity errors. Browser cold-start measurements on mainnet (three alternated pairs) put the
  median connection time at 2.11 s instead of 5.27 s and the first chunk, including connection,
  at 18.6 s instead of 28.5 s. In the browser, queued lanes recheck the shared failed-dial cache
  instead of repeating a failed connection attempt, and early hints require authenticated
  connections. (V2-1305, #206)

### Internal
- CI: repo checks run on pull requests only; the duplicate `push` runs on `main` and `rc-*` are
  dropped from `ci.yml` and `adr-governance.yml`. (V2-1353, #207)

## [0.3.8] - 2026-09-24

### Added
- `ant` is distributed on npm as `@withautonomi/ant`, so `npm install -g @withautonomi/ant`
  works in agent sandboxes and CI runners that allow package-manager traffic but block direct
  binary downloads (V2-1152). Packaging only: the published packages carry the same signed
  binaries as the GitHub release, verified against `SHA256SUMS.txt` and their ML-DSA-65
  signatures during packaging, and published from the release workflow with npm provenance. A
  per-platform companion package guarded by `os`/`cpu` means only one binary is downloaded. The
  `beta` dist-tag tracks beta releases and `rc` tracks release candidates, so a plain
  `npm install -g @withautonomi/ant` only ever serves a stable build; `install.sh`, `install.ps1`
  and the release assets are unchanged. See [`npm/README.md`](npm/README.md).
- `ant` falls back to the bootstrap seeds already bundled into the binary
  (`network_defaults::bundled_bootstrap_seeds`) when no `bootstrap_peers.toml` is present in the
  config directory, instead of failing with `NoBootstrapPeers`. Priority is otherwise unchanged
  — explicit `-b` peers, then a devnet manifest, then the config file, then the bundled seeds —
  and an explicitly selected devnet manifest still errors rather than reaching for mainnet peers.
  This is what makes an npm install usable out of the box: npm 12 blocks package install scripts
  by default, so the step that copies that file into the config directory frequently does not
  run.
- `ant-core` builds for the browser as WASM and shares the native client's own routing, quote and
  witness policy, content addressing, self-encryption, DataMap encoding, payment preparation and
  per-record storage policy, rather than keeping a second implementation (ADR-0004, V2-803).
  `ant-core` gains a `native` feature (on by default, so ordinary consumers keep the existing
  API; filesystem, daemon and node code sit behind it) and a `browser-wasm` feature, built with
  `--no-default-features --features browser-wasm`. The browser client speaks browser protocol v5
  over WebRTC Direct with the shared post-quantum transport session, resolves public downloads
  and bounded range reads of ordinary native-compatible DataMaps, and runs the same batched and
  Merkle payment coordinator as native uploads, with prepared and paid checkpoints, bounded
  transfers and recovery. Browser recovery tells a verified failed payment from an unknown
  outcome, keeps durable evidence, never treats an unknown outcome as permission to pay again,
  and blocks new wallet submissions after the client is closed; confirmed Merkle batches stay
  storable when a later payment or reconciliation fails, with the partial progress and spend
  kept. See [`README.md`](README.md) and
  [`docs/portable-client-recovery.md`](docs/portable-client-recovery.md). (#186)
- In the browser: `BrowserNetworkClient.uploadRecords(batch, …)` quotes, pays for and stores one
  batch of caller-staged, content-addressed records, so a file larger than the origin's
  IndexedDB quota can be uploaded in storage-bounded windows, each with its own payment and
  checkpoint scope (the coordinator picks single-node or Merkle payment per batch under the
  native rules). `downloadPrivateFile` and `openPrivateFile` read a file from the canonical
  MessagePack DataMap its holder kept, the same bytes as a native `.datamap` file, resolving
  nested DataMap records from the network. Every upload result now reports `paymentMode`
  (`single` or `merkle`), like native `payment_mode_used`. (V2-1285, #203)

### Changed (breaking — browser client)
- Browser protocol v5 requires matching nodes, and the browser-WASM bindings change, so consumers
  such as the browser SDK must refresh their generated bindings together with the client. Native
  QUIC behaviour and the native entry points are unchanged. (V2-803, #186)

### Changed
- `bootstrap_peers.toml` is now a shared resource (`ant-core/resources/bootstrap_peers.toml`,
  ADR-0005) with a `quic` list of native multiaddresses and a `webrtc` list of complete WebRTC
  Direct multiaddresses; release packaging still installs it under the same filename, and legacy
  `peers = [...]` files stay readable. `-b/--bootstrap` accepts QUIC multiaddresses such as
  `/ip4/1.2.3.4/udp/10000/quic`, including an optional `/p2p/<peer-id>` pin that is kept all the
  way to the transport; the old `ip:port` form is still accepted. In `ant-core`: the new
  `network_defaults` module (`bundled_bootstrap_seeds`, `parse_bootstrap_seeds`,
  `parse_quic_seed`) and `config::load_bootstrap_multiaddrs` / `resolve_bootstrap_multiaddrs`
  alongside the existing `SocketAddr` functions. (V2-803, #186)
- `ant update` no longer replaces the binary when `ant` was installed by a package manager. It
  still reports whether a newer version is available, then prints the command that will install
  it (`npm update -g @withautonomi/ant`). Self-replacing an npm-managed binary left npm's
  metadata describing a file that was no longer there, so the next `npm update` silently rolled
  the user back.

### Fixed
- Uploads no longer fail outright when the public RPC briefly answers the fee estimate with a 429
  or `-32000 context deadline exceeded` (`Could not get current gas price: ...`). The fee read in
  the payment journal's `prepare_payment` now retries, as the released client already did through
  `send_transaction_with_retries`; native and WASM uploads are both covered. Picked up by pinning
  `evmlib` 8ad5ec0 and `ant-protocol` 17949f8. (V2-1288, #204)
- Native uploads persist the signed payment transaction before it is broadcast and reconcile it
  on retry: the journal is retained while the outcome is unfinalised or cannot be observed, a
  verified finalised replacement clears the attempt for a fresh payment, and a same-intent
  replacement recovers its settlement receipt. A payment whose outcome is unknown is not paid
  again, and a journal signed in the current call and rejected with `nonce too low` (a stale read
  from a lagging RPC replica) is discarded and prepared once more, since nothing was sent.
  (V2-803, #186)

### Internal
- Dependency pins: `ant-protocol`, `saorsa-core` and `saorsa-transport` advance to the V2-834
  merges (pins only, no source change; the three must name the same revisions so the graph keeps
  a single copy of each). (V2-834, #202)

## [0.3.7] - 2026-09-16

### Added
- Quote requests declare the settlement version the client pays under (`QuoteRequestV2` and
  `MerkleCandidateQuoteRequestV2`, on both the single-node and the merkle path), so a client
  settling under superseded rules is refused before it pays. A merkle batch pays on-chain before
  any storer sees a PUT and merkle receipts are not refundable, so without this the refusal came
  after the money was gone. `Error::ClientUpdateRequired` is terminal: the upload aborts before
  payment and shows the storer's own wording, which tells the user to run `ant update` and that
  nothing was charged. `Error::StorerUpdateRequired` is not: the storer is the old side, so the
  upload quietly uses other peers, and fails for lack of quotes (still before payment) if too few
  remain. A corroborated refusal is latched for the run, so the wave-batch payment path
  (`prepare_chunk_payment`, `batch_pay`) and an external signer are also stopped from paying by
  another route, and a refusal in flight is no longer cancelled when a sibling candidate pool
  fails first. When this landed the minimum and current settlement versions were equal, so
  nothing refused the build yet; the gate takes effect on the first settlement bump that makes
  the minimum load-bearing. (V2-975, #171)
- `ant file download --download-diagnostics <PATH>` writes an opt-in JSONL sidecar (schema v4) of
  the download for troubleshooting: request IDs, the local client peer ID, expected and
  authenticated peers, transport source and route, timings, DHT context, retries, outcomes and
  concurrency snapshots. The records go through a bounded, non-blocking channel; with the flag
  absent no file, channel or writer is created and ordinary downloads are unchanged. In
  `ant-core`: `spawn_download_diagnostics_writer`, `DownloadDiagnosticsRecord`,
  `DownloadDiagnosticsOutcome` and `DownloadDiagnosticsSender`. (V2-903, #193)

### Fixed
- A command-line file upload that stops because storers refused a merkle payment now says so.
  The failure used to read `N chunk(s) short of quorum after 4 attempts`; it now carries the
  storer's refusal, and an externally unpaid sub-batch is reported as having no proof rather than
  as a refusal. This covers `ant file upload` only: the in-memory `data_put` path still returns a
  bare `Missing merkle proof for chunk <hex>`. (V2-1175, #195)

### Internal
- The test harness builds against `ant-node`'s file-per-chunk `ChunkStore` instead of
  `LmdbStorage`, which had stopped all seven e2e test binaries compiling once ant-core pointed at
  an ant-node carrying that change; the dependency stays on one line so the release tooling can
  retarget it. (V2-1033, #196, #197)
- CI: the PR template check requires one of Linear's closing magic words (`Closes`, `Fixes`, ...)
  rather than accepting a bare issue key or URL. (V2-1161, #191)

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
