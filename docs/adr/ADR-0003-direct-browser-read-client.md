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

## Considered Options

1. **Use the daemon REST API as a data gateway.** Rejected for lookup and file
   bytes because it would not exercise a full browser client.
2. **Compile the complete native Rust client to WebAssembly.** Deferred because
   the native transport, EVM, and filesystem dependency graph is not currently
   browser-compatible.
3. **Implement a narrow JavaScript WebTransport immutable-data client
   (chosen).** It maps directly to the versioned browser node protocol and
   keeps the application boundary small enough to audit.

## Decision

The `web/` package will implement the direct browser client:

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
- self-encrypt selected public files with the native `self_encryption 0.36`
  format and generate the public MessagePack DataMap;
- request ordinary node storage quotes, verify their ML-DSA peer/content
  binding, forced price, and signed storage commitment before payment;
- construct an EVM wallet only from the runtime secret field, approve the
  public vault when required, and make one batched `payForQuotes` transaction;
- upload each content-addressed encrypted record with the signed quote and
  transaction hash through paid `PUT_CHUNK`; the wallet key never crosses the
  WebTransport session;
- fetch the public MessagePack DataMap and every resolved encrypted data chunk;
- reconstruct the file with the native `self_encryption 0.36` BLAKE3 KDF,
  ChaCha20-Poly1305 authentication, and Brotli decompression;
- verify encrypted-record addresses, per-chunk plaintext hashes and sizes, and
  the final whole-file BLAKE3 hash before allowing a save;
- expose a small test site that loads the local testnet manifest, displays the
  startup-published file, uploads paid files, and downloads either through the
  browser save flow.

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

For the local vertical slice, the bootstrap manifest carries a resolved JSON
view of the public root DataMap alongside its ordinary on-network DataMap
address. The browser still fetches and verifies that public DataMap record and
all file bytes directly from nodes. Production discovery must replace this
unsigned resolved view with parsing and validation of the signed/on-network
metadata chain.

## Consequences

### Positive

- Lookup, payment, and data transfer remain decentralized at the application
  layer.
- A local five-node testnet can validate multiple direct node connections,
  multiaddress-embedded certificate pins, lookup convergence, fallback, and
  content verification.
- The browser protocol is independent of native Rust serialization details.

### Negative / Trade-offs

- The current client reconstructs files in memory and the local launcher caps
  public files at 64 MiB; upload encryption and reconstruction are not yet
  streaming.
- JavaScript lookup behavior must remain aligned with native Kademlia rules.
- Certificate and endpoint verification adds bootstrap-record lifecycle work.

### Neutral / Operational

- The manifest HTTP service carries only small bootstrap metadata.
- WebTransport still requires a secure browser context; localhost qualifies
  for development.
- Node and client repositories must run compatible browser protocol versions.

## Validation

- Unit tests cover fixed-width identifiers, XOR ordering, bidirectional binary
  framing, manifest/payment validation, quote signatures and the native
  Keccak-256 EVM quote-hash vector, native-format
  encryption/DataMap generation, and BLAKE3 mismatch rejection.
- The browser production bundle builds without Node-specific runtime APIs.
- A fixed vector generated by native `self_encryption 0.36` verifies browser
  KDF, authenticated decryption, Brotli reconstruction, and tamper rejection.
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
