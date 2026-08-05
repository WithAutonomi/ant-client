# ADR-0003: Direct browser immutable-data client over WebTransport

- **Status:** Proposed
- **Date:** 2026-08-03
- **Last amended:** 2026-08-05
- **Decision owners:** <pending>
- **Reviewers:** <pending>
- **Supersedes:** none
- **Superseded by:** none
- **Related:** ant-node ADR-0009; W3C WebTransport

## Context

The Autonomi web client must perform closest-node lookup, immutable-data
download, quote verification, payment, and upload itself. Sending those
operations through an HTTP application gateway would make the gateway an
availability, privacy, and bandwidth chokepoint.
Browsers cannot use the native Saorsa QUIC protocol, but they can establish
WebTransport sessions with browser-compatible node listeners.

This repository owns the client and UI side of that split. Nodes own transport
termination, local DHT answers, storage reads and paid writes, endpoint
records, and testnet bootstrap-manifest production under ant-node ADR-0009.

## Decision Drivers

- File bytes must travel directly from a storage node to the browser.
- The browser must own iterative XOR lookup rather than ask a gateway to do it.
- Self-signed node certificates must be authenticated through hashes embedded
  in self-contained node multiaddresses, without a separate client argument.
- Downloaded immutable content must be verified before it is exposed to users.
- The wallet secret must be provided at runtime, used only by the local EVM
  signer, and never sent to a node or persisted in bootstrap metadata.
- Local testnets need a reproducible bootstrap and default-file workflow.
- Compatibility-sensitive file processing should be shared with the Rust
  client instead of being independently reimplemented in JavaScript.

## Considered Options

1. **Use the daemon REST API as a data gateway.** Rejected for lookup and file
   bytes because it would not exercise a full browser client.
2. **Compile the complete native Rust client to WebAssembly.** Deferred because
   the native transport, EVM provider, Tokio, and filesystem dependency graph
   is not currently browser-compatible.
3. **Use a Rust/WASM data core with thin JavaScript browser adapters
   (chosen).** Compile the portable immutable-data part of `ant-core` to WASM
   while retaining JavaScript only where browser APIs or currently
   native-only dependencies require it.

## Decision

`ant-core` has two build surfaces. Its default `native` feature preserves the
existing native library. A `wasm32-unknown-unknown` build with
`--no-default-features --features browser-wasm` excludes node management,
native transport, Tokio, filesystem, and native EVM-provider dependencies and
exports browser-safe immutable-data operations through `wasm-bindgen`.

The Rust/WASM core will:

- self-encrypt complete public files with the same `self_encryption 0.36`
  implementation used by the Rust client;
- encode and decode the native MessagePack `DataMap` representation;
- calculate and verify BLAKE3 content addresses;
- verify every encrypted record against its DataMap destination address; and
- authenticate, decompress, and reconstruct complete public files.

The `web/` package will remain responsible for browser-specific orchestration:

- load a versioned browser bootstrap manifest containing WebTransport
  multiaddresses and published immutable-file metadata;
- accept a single multiaddress per seed, extract its one or two
  `/certhash` SHA-256 multihashes internally, and pass them to WebTransport;
- require `/p2p/<peer-id>` in every address, verify its `HELLO` peer ID, and
  reject discovered endpoint/peer mismatches;
- perform iterative `FIND_NODE` queries using 256-bit XOR ordering, `K = 20`,
  and `ALPHA = 3`;
- query closest direct endpoints with `GET_CHUNK`, retrying `not_found` and
  unavailable nodes without routing bytes through the manifest service;
- call the Rust/WASM core to self-encrypt selected public files and generate
  their public MessagePack DataMaps;
- request ordinary node storage quotes, verify their ML-DSA peer/content
  binding, forced price, and signed storage commitment before payment;
- construct an EVM wallet only from the runtime secret field, approve the
  public vault when required, and make one batched `payForQuotes` transaction;
- upload each content-addressed encrypted record with the signed quote and
  transaction hash through paid `PUT_CHUNK`; the wallet key never crosses the
  WebTransport session;
- fetch the public MessagePack DataMap and every resolved encrypted data chunk,
  then pass those records to the Rust/WASM reconstruction API;
- verify the final file size and use the Rust/WASM BLAKE3 verifier before
  allowing a save;
- expose a small test site that loads the local testnet manifest, displays the
  startup-published file, uploads paid files, and downloads through the browser
  save flow.

The local browser manifest is bootstrap metadata, not a gateway. Production
clients will replace its unsigned endpoint list with the ML-DSA-signed records
defined by ant-node ADR-0009.

The JavaScript API and demo never accept a certificate hash separately from an
endpoint. Their sole dialing input is a canonical address of the form
`/.../quic-v1/webtransport/certhash/<multihash>/p2p/<peer-id>`. Repeated
`/certhash` components permit current/next certificate overlap. Keeping the
transport location, accepted TLS key, and expected peer identity in one value
prevents callers from accidentally combining fields belonging to different
nodes.

Rust nodes construct and validate this syntax through
`saorsa_transport::TransportAddr::WebTransport` wrapped by
`saorsa_core::MultiAddr`; it is not a browser-specific string type. The
JavaScript parser is the browser implementation of that same canonical wire
format and is covered by matching current/next-pin fixtures.

Quote and storage-commitment verification remains JavaScript for now.
`ant-protocol 2.3.1` unconditionally reaches the native Saorsa transport and
EVM dependency graph, including Tokio networking and `mio`, and therefore
cannot be linked into a browser WASM target. A future transport-free
`ant-protocol` feature should expose the pure multiaddress, quote, commitment,
and ML-DSA verification types without enabling native networking. At that
point those compatibility-sensitive operations should also move behind the
Rust/WASM boundary.

For compatibility with the local launcher, the bootstrap manifest still
carries a resolved JSON view of the public root DataMap alongside its ordinary
on-network DataMap address. The download path does not use that copy to select
records: it fetches the public DataMap from a node and uses the Rust/WASM
decoder to derive the encrypted-record addresses. Production discovery must
still replace the unsigned manifest with validation of the signed/on-network
metadata chain.

## Consequences

### Positive

- Lookup, payment, and data transfer remain decentralized at the application
  layer.
- A local five-node testnet can validate multiple direct node connections,
  multiaddress-embedded certificate pins, lookup convergence, fallback, and
  content verification.
- Self-encryption, DataMap serialization, reconstruction, and content
  addressing have one Rust implementation across native and browser clients.
- The browser application keeps direct control of WebTransport, wallet, and
  DOM APIs without pulling native runtime dependencies into WASM.

### Negative / Trade-offs

- The current client reconstructs files in memory and the local launcher caps
  public files at 64 MiB; upload encryption and reconstruction are not yet
  streaming.
- JavaScript lookup and quote-verification behavior must remain aligned with
  native Kademlia and protocol rules until transport-free Rust APIs exist.
- The initial WASM module is approximately 1.4 MiB uncompressed and browser
  file processing is still in memory.
- Certificate and endpoint verification adds bootstrap-record lifecycle work.

### Neutral / Operational

- The manifest HTTP service carries only small bootstrap metadata.
- WebTransport still requires a secure browser context; localhost qualifies
  for development.
- Node and client repositories must run compatible browser protocol versions.

## Validation

- Rust unit tests cover an exact native `self_encryption 0.36` wire vector,
  public DataMap generation, round-trip reconstruction, and tamper rejection.
- JavaScript unit tests cover fixed-width identifiers, XOR ordering,
  bidirectional binary framing, manifest/payment validation, quote signatures,
  the native Keccak-256 EVM quote-hash vector, and the browser orchestration
  around the Rust/WASM boundary.
- CI compiles `ant-core` for `wasm32-unknown-unknown` with native features
  disabled, builds the generated `wasm-pack` package, runs browser-client
  tests against it, and produces the Vite bundle.
- The browser production bundle builds without Node-specific runtime APIs.
- The generated WASM package encrypts and reconstructs the same fixed vector
  as the native Rust test, covering the native KDF, authenticated decryption,
  Brotli reconstruction, MessagePack DataMap, and BLAKE3 addresses.
- A live node integration test starts five WebTransport-enabled nodes,
  publishes a public DataMap and encrypted chunks, connects with the advertised
  self-contained multiaddress, retrieves every record, and reconstructs the
  exact file, pays a real signed quote, accepts a paid binary PUT through the
  ordinary node verifier, and reads the stored record back.
- Before acceptance, run interactive tests on current Chrome, Firefox, and
  Safari and add shared lookup convergence vectors with the native client.
- Revisit this decision when streaming file reconstruction or production signed
  endpoint discovery is implemented.

## Notes for AI-assisted work

AI tools may help draft this ADR, but **must not mark it Accepted without human
review**. Accepted ADRs are immutable: create a new superseding ADR rather than
editing an Accepted ADR.
