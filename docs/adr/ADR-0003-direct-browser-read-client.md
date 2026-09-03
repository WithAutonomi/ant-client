# ADR-0003: Direct browser immutable-data client over WebRTC Direct

- **Status:** Proposed
- **Date:** 2026-08-03
- **Last amended:** 2026-09-03
- **Decision owners:** <pending>
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ant-node ADR-0009; Saorsa WebRTC Direct transport; ant-client-browser-sdk

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
- authenticated protocol-v4 session establishment using ephemeral ML-KEM-768,
  ML-DSA-65 transcript authentication, peer-ID/public-key binding, independent
  direction keys, and ordered ChaCha20-Poly1305 records from `ant-protocol`;
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

`ant-core` may call narrow JavaScript callbacks to obtain file ranges, load or
discard externally staged encrypted records, report progress, and submit an
already verified payment plan. Those callbacks expose browser capabilities;
they do not reimplement Autonomi protocol behavior.

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
described by ant-node ADR-0009.

Browser protocol v4 requires a matching v4 node listener. Plaintext v3 and
encrypted v4 deliberately fail closed. This browser wire change does not alter
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
- Browser protocol v4 clients and node listeners must be deployed together.
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
