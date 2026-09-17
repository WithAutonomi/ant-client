# ADR-0004: Direct browser immutable-data client over WebRTC Direct

- **Status:** Proposed
- **Date:** 2026-08-03
- **Last amended:** 2026-09-16
- **Decision owners:** <pending>
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** [ant-node ADR-0013](https://github.com/WithAutonomi/ant-node/blob/web-support/docs/adr/ADR-0015-direct-browser-clients-over-webrtc-direct.md); Saorsa WebRTC Direct transport; ant-client-browser-sdk

## Context

Browser applications must perform closest-node lookup, immutable-data transfer,
quote verification, payment planning, and storage without routing file bytes
through an HTTP application gateway. A gateway would become an availability,
privacy, and bandwidth chokepoint.

Browsers cannot use the native Saorsa QUIC transport, but they can connect to
Saorsa WebRTC Direct listeners through ICE-lite, certificate-pinned DTLS, SCTP,
and reliable ordered DataChannels. The reusable Autonomi behavior belongs in
`ant-core`; browser application integration belongs in the separate
`ant-client-browser-sdk` project.

## Decision Drivers

- File bytes must travel directly between storage nodes and the browser.
- The client must perform iterative XOR lookup instead of delegating it to a
  gateway.
- Compatibility-sensitive networking, self-encryption, quote verification,
  payment planning, and storage policy must be implemented in shared Rust.
- Existing native `ant-core` and `ant-cli` callers must remain source-compatible.
- Browser UI, wallet, file, worker, IndexedDB, and service-worker choices must
  not become part of the low-level Rust crate.
- Generated WASM bindings must be tested independently of any particular SDK or
  demo application.

## Considered Options

1. **Use the daemon REST API as a data gateway.** Rejected because lookup and
   file bytes would no longer be direct.
2. **Compile the complete native client unchanged to WebAssembly.** Rejected for
   now because its Tokio, filesystem, native QUIC, daemon, and native EVM
   dependencies are not browser-compatible.
3. **Keep an application and JavaScript protocol implementation in this
   repository.** Rejected because the newer browser SDK owns that layer and a
   second application implementation would drift.
4. **Expose a Rust/WASM client core and keep browser integration in the SDK
   (chosen).**

## Decision

`ant-core` retains its default `native` feature. Building for
`wasm32-unknown-unknown` with `--no-default-features --features browser-wasm`
selects a browser-safe dependency graph and exports the low-level browser API
through `wasm-bindgen`. Native callers and `ant-cli` continue to use the
existing native facade and QUIC transport.

### Responsibilities of ant-core

The Rust/WASM implementation owns:

- canonical WebRTC Direct multiaddress parsing, including the literal IP, UDP
  port, DTLS certificate fingerprint, and expected ANT peer ID;
- browser `RTCPeerConnection` and ordered `RTCDataChannel` management through
  `web-sys`, including framing, fragmentation, backpressure, deadlines, and
  bounded connection reuse;
- authenticated protocol-v5 session establishment using ephemeral ML-KEM-768,
  ML-DSA-65 transcript authentication, peer-ID/public-key binding, independent
  direction keys, and ordered ChaCha20-Poly1305 records from `saorsa_transport::webrtc`
  using `saorsa-pqc`;
- iterative closest-node lookup through Saorsa's shared
  transport-independent lookup runner;
- authenticated discovery of additional WebRTC Direct node addresses;
- public DataMap resolution, nested DataMap handling, chunk retrieval,
  reconstruction, range decryption, and BLAKE3 verification;
- incremental self-encryption and content-addressed record generation using the
  same `self_encryption` implementation and MessagePack DataMap representation
  as the native client;
- signed storage-quote and commitment verification, price calculation, and
  payment-plan construction using portable `ant-protocol` types;
- paid record upload with the shared adaptive scheduler, bounded in-flight
  bytes, close-group quorum, fallback targets, and whole-record retries; and
- the bounded `BrowserFileReader` used by range-oriented consumers.

Native client behavior is the reference for shared client policy:

- `quote_validation.rs` owns the resolve-before-pay checks for peer binding,
  signatures, commitment shape, forced pricing, sidecar limits, and commitment
  resolution. Both targets use ant-protocol verifiers, commitments, and U256
  amounts; browser adapters decode their JSON envelope into these same types. Baseline quotes have no pin to
  resolve, so an unsolicited sidecar does not affect their admission.
- `quote_policy.rs` owns witnessed peer eligibility, the quote collection
  window, supported median subsets, existing-holder voting, and PUT ordering.
  Both paths require seven initial peers, use the closest seven peers' views
  to establish witness support, and prefer supporting witnesses for storage.
  The base witness quorum is five, reduced by missing views as in native.
  The initial PUT neighbourhood widens to twenty peers when available, with
  a seven-peer fallback. Four successful stores remain the delivery quorum.
- `payment_policy.rs` preserves stable tie ordering, selects the upper median,
  and pays that issuer three times its price. The native cost estimate uses
  the same median rule. Existing-holder quotes are excluded from payment.
- `transfer_policy.rs` preserves native failure classification. Structured
  remote PUT rejections and dial churn do not reduce adaptive concurrency;
  a storage shortfall containing a PUT-response timeout does. Browser RPC
  errors retain their codes and failure origin instead of classifying words
  in a human-readable error message.

The adapters own transport discovery, authenticated transcript collection,
wire decoding, transaction submission, and proof encoding. Browser capability
and payment-network checks additionally ensure eligible WebRTC targets exist
before invoking the wallet. A peer's ability to supply a valid payable quote
and its ability to store another issuer's proof are separate checks.

`ant-core` may call narrow JavaScript callbacks to obtain file ranges, load or
discard externally staged encrypted records, report progress, and submit an
already verified payment plan. Those callbacks expose browser capabilities;
they do not reimplement Autonomi protocol behavior.

### Browser RPC ownership and deadlines

A pooled operation owns the peer mutex from connection/PQ/HELLO authentication
through capability and payment-network validation and the complete application
exchange. The private locked client is the only entry point to wire requests.
Queued operations recheck authentication under that lock and may reconnect after
a failed predecessor. An explicit `BrowserNodeSession` instead checks its captured
generation under the lock and remains invalid after closure.

Pool and peer admission share one monotonic 400-second ceiling. This allows one
maximum request/response transfer (180 seconds each) plus bounded setup; it is a
queue limit, not a replacement for the operation's response allowance. Existing
parent cancellation still wins. Connection setup, including SDP and PQ, has a
30-second ceiling and HELLO has its own 10-second ceiling. Application request
transfer retains the shared size-based deadline, including the final DataChannel
buffer drain. Only then does the caller's response allowance begin. Response
fragment arrival timestamps preserve the existing size-based frame deadline even
when ingress was buffered during the outgoing drain.

Queue expiry does not close an active association. Abandoning an encrypted
exchange retires its generation because encryption has already advanced the PQ
sequence, even if backpressure prevented transmission. A closed pool cannot
publish an in-flight connection. Admission, transfer and response timeout messages
survive the browser data adapter and keep their timeout classification.

This repairs the September 8 outer-response-timer regression and September 14
stale-authentication regression without changing wire messages, native transport,
payment proofs, or quorum. Browser lookup's five-second batch grace policy dates
to August 31 and is unchanged by this repair. Bounded global dial/byte scheduling
and discovery-policy changes require separate evidence and review.

### Responsibilities of ant-client-browser-sdk

The separate SDK owns:

- packaging and initializing the generated WASM module;
- the public TypeScript API and stable application-facing errors/events;
- `File` and `Blob` handling, Web Workers, and IndexedDB upload staging;
- wallet-independent payment-provider interfaces and Ethers or Wagmi/Viem
  adapters;
- browser save flows;
- the same-origin service-worker bridge for media-element byte ranges;
- runnable examples and demo user interfaces; and
- TypeScript, bundler, and real-browser end-to-end tests.

This repository does not ship a browser demo or duplicate those adapters. It
keeps only a small generated-WASM smoke-test harness under
`ant-core/wasm-tests/`. The harness validates that `wasm-pack` output loads in
JavaScript and preserves key native/WASM compatibility vectors.

### Bootstrap and protocol compatibility

A bootstrap endpoint is one canonical address of the form
`/ip4|ip6/<literal>/udp/<port>/webrtc-direct/certhash/<multihash>/p2p/<peer-id>`.
The endpoint binds transport location, accepted DTLS certificate, and expected
ANT identity. DNS endpoints, port zero, malformed hashes, and ambiguous address
components are rejected.

A local testnet manifest is optional bootstrap metadata, not a data gateway or
download authorization source. A client can start from one complete endpoint,
authenticate it, obtain public payment configuration, discover peers, and
resolve any public DataMap address from the network. Production bootstrap
distribution and certificate-rotation recovery remain operational concerns
described by ant-node ADR-0013.

Browser clients and node listeners must agree on the shared
`BROWSER_PROTOCOL_VERSION`, `BROWSER_PROTOCOL_NAME`, and DataChannel identifier
exported by `saorsa_transport::webrtc`. Mismatched versions fail closed. This browser wire change does not alter
native QUIC, stored chunk/DataMap formats, quote commitments, payment proofs, or
public file addresses.

## Consequences

### Positive

- Native and browser clients share self-encryption, DataMaps, lookup behavior,
  quote verification, payment planning, transfer scheduling, and retry policy.
- Browser applications do not need to reproduce Autonomi networking or
  cryptography in JavaScript.
- The `ant-client` PR remains focused on the reusable Rust library rather than
  embedding a competing application and SDK.
- SDK UI and wallet integrations can evolve independently of the native CLI.
- Generated bindings are still exercised directly, catching failures that a
  Rust-only WASM target check would miss.

### Negative / Trade-offs

- `ant-core` and `ant-client-browser-sdk` releases must remain compatible, and
  the SDK must regenerate its bundled WASM when the low-level API changes.
- Browser-only failures involving workers, IndexedDB, wallets, service workers,
  and media elements are detected in the SDK rather than this repository.
- Complete-file downloads remain memory-bound; range reading is the bounded
  path for large media and range-oriented formats.
- WebRTC exposes transport metadata, lengths, and timing. The application-layer
  post-quantum session protects RPC and chunk plaintext against later
  compromise of only the classical DTLS key exchange; it does not make ICE,
  DTLS, SCTP, or the browser WebRTC implementation post-quantum secure.
- Nodes must preserve their DTLS certificate because changing it invalidates
  certificate-pinned endpoints.

### Operational

- WebRTC and service-worker consumers require a secure browser context;
  localhost qualifies for development.
- Deploy browser clients and node listeners using the same shared wire contract.
- The SDK owns real-browser compatibility testing for current Chrome, Firefox,
  and Safari.

## Validation

This repository validates:

- Rust unit tests for browser manifests, framing, payment verification,
  self-encryption, DataMap handling, lookup, and transfer policy;
- `cargo check` and `cargo clippy` for `wasm32-unknown-unknown` with only the
  `browser-wasm` feature;
- a release `wasm-pack` build;
- JavaScript loading of the generated module;
- fixed native/WASM vectors for WebRTC addresses, response framing, EVM quote
  hashing, self-encryption, nested DataMaps, streaming encryption,
  reconstruction, and tamper rejection; and
- coordinated node integration tests for encrypted session establishment,
  lookup, public download, signed quotes, paid upload, and record read-back.

The browser SDK separately validates its TypeScript API, wallet adapters,
worker and IndexedDB staging, save behavior, service-worker range bridge,
examples, and live browser flows.

## Notes for AI-assisted work

AI tools may help draft this ADR, but **must not mark it Accepted without human
review**. Accepted ADRs are immutable: create a new superseding ADR rather than
editing an Accepted ADR.

## Shared crate graph amendment (2026-09-08)

Browser WASM now imports ant-protocol, saorsa-core, saorsa-pqc, and evmlib.
The ordinary `data::Client` owns discovery policy, quote admission, U256 median
payment plans, proof construction, PUT retries/quorum, GET integrity checking,
chunk caching, and in-memory file reads on both targets. Native filesystem
streaming, persistent resume receipts, node management, sockets and background
runtime tasks remain feature-gated.

`BrowserNetwork` supplies authenticated discovery and request I/O as local
futures. The WebRTC implementation sends encoded `ChunkMessage` requests in the
`chunk_protocol` binary frame; ant-node routes these to its existing handler.
HELLO capability checks and browser payment-network selection run before the
wallet callback. Application record limits remain 4 MiB; encoded native messages
allow 5 MiB for proof overhead. This capability is additive to browser v5 and
requires a node advertising `chunk_protocol` for the shared Client facade.

The browser protocol is unreleased and retains its v5 identifiers. Frames are
one JSON object followed immediately by raw binary content, with no inner
length prefix; the shared codec uses the parsed JSON byte offset to locate the
body. It rejects frames exceeding
5 MiB + 64 KiB before parsing and caps JSON parsing at a fixed 64 KiB shared with
the node. This accommodates paid upload quotes with full signed commitments.
The node cannot lower this header limit. The encrypted-record length prefix
still provides bounded DataChannel reassembly. This revises the unreleased
format in place without a protocol-version bump.

The browser facade retains JS wallet callbacks and staged content loaders.
`ChunkPaymentPlan` separates payment metadata from bytes, so staging can load
records within a bounded store window. `with_content` checks length and hash
before the shared proof/store path runs. Browser reads use a bounded instance
of the native chunk cache. Browser clocks use web-time and JS deadlines; payment
wire timestamps retain std::time::SystemTime serialization.

Native and WASM unit/integration checks cover these boundaries. Generated-WASM
transport tests mock the WebRTC host while exercising the shared Client and
real PQ session, signature, proof, framing, and encryption code. The node devnet
test sends the same native quote/PUT/GET messages through real WebRTC endpoints
and pays against a local Anvil chain.

### Browser API and recovery boundaries

`BrowserNodeClient.connect()` completes the PQ handshake and HELLO and returns
`BrowserNodeSession`. Application RPC methods belong to that session. Closing
it invalidates the handle; reconnect explicitly to obtain another session.

Public file identity is its canonical DataMap address. Browser descriptor fields
are display hints or derived metadata, not another source of content identity.
Staged uploads resolve their map and preflight the staged records before payment.
The whole-file digest is computed after reading plaintext; it is not required to
retrieve a public file. Native Rust public-file APIs and the stored format are
unchanged by this browser API contract.

Uploads journal prepared attempts before wallet invocation, submission evidence
as it arrives, and the raw receipt before validation. A failed observation is
not permission to submit again. The optional wallet `recover` callback observes
the original payment; it must never broadcast another transaction. Browser
callers supply a checkpoint persistence callback before any paid submission.

Definitively unsuccessful payments can be resolved separately with
`BrowserNetworkClient.reconcileFailedUploadPayment(checkpoint, verifyFailure,
onCheckpoint)`. The caller waits for the original upload and wallet request to
finish, then passes the latest checkpoint. The trusted verifier receives
`(attempt, scope)` and checks the complete attempt against its original wallet
and payment network. It must never submit a transaction. Its result is either:

- `{ status: "notSubmitted", evidence: {...} }`, establishing that the wallet
  never submitted the payment and cannot still submit it. Rust also requires
  that the journal contain no submission or receipt evidence.
- `{ status: "reverted", transactionHashes: [...], evidence: {...} }`, after
  establishing final on-chain failure of every transaction. The hashes must
  cover all journaled transactions; a partial failure cannot release the attempt.

The nonempty evidence object records the verifier's wallet or chain observations.
Rust validates the journal/result consistency; wallet and chain verification
belongs to this trusted callback, as confirmation does for payment callbacks.
Timeouts, missing receipts, and unresolved outcomes must reject verification.
The method archives the failed attempt and its evidence while preserving prepared
plans and confirmed proofs. It awaits `onCheckpoint` before returning the updated
checkpoint. The caller then explicitly resumes the ordinary upload with that
checkpoint. A persistence or verification failure does not authorize payment.

Confirmed Merkle batches remain eligible for storage if a later payment or
recovery fails. Partial results retain both the original error and current-call
spend/progress. Closing the browser client before wallet invocation clears and
persists that definitely unsubmitted intent; already submitted payments retain
their journal for observation.

The callback lookup facade adapts caller-supplied queries for embedding and
algorithm tests. The production network adapter additionally owns authenticated
sessions, ownership records, endpoint health, and live routing cache admission.
Both use the same Saorsa lookup engine; production failure cases are tested
through `BrowserNetworkClient` rather than inferred from facade tests.

Browser wire adaptation reuses verified address publications through a bounded
256-entry LRU cache per WASM thread/instance. Keys are the complete encoded
publication, including its signature, identity key and sequence; only successful
shared-core verification enters the cache. Repeated lookup responses and local
wire/typed conversions can reuse the immutable verified object. The cache is
not an owner-view store: every use still checks the advertised peer identity,
derives fresh local reliability metadata, and follows the shared monotonic
replacement and witness policy. Failed verification never evicts a valid entry.


### Transfer progress semantics (2026-09-17 correction)

The upload adapter exposes optional observation hooks for existing-storage
checks, records already present, validated payment quote pools, and successful
record writes. Existing adapters remain source-compatible through default no-op
hooks. The aggregate `stored` callback still includes already-present records
for native accounting; the browser exposes it as confirmed availability, not
new writes. Browser new-store counters use only successful PUT callbacks.
Merkle preflight checks do not imply that candidate payment quotes are ready.
Record quote completion is emitted only after the actual batch has been prepared.
These hooks do not authorize payments, change quorum, or relax verification.
