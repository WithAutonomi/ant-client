# Generated WASM smoke tests

These tests exercise the JavaScript bindings emitted from `ant-core` without
providing a browser application or SDK. Build the package and run the tests
from the `ant-core` directory:

```bash
wasm-pack build --target web --out-dir wasm-tests/pkg --release . \
  --no-default-features --features browser-wasm,test-utils
node --import ./wasm-tests/setup-wasm.mjs \
  --test ./wasm-tests/*.test.mjs
```

The generated `pkg/` directory is ignored. Browser-native integration,
examples, and end-to-end browser tests belong to `ant-client-browser-sdk`.

The test build enables `test-utils` for a deterministic mock node. Transport
regressions use a mock `RTCPeerConnection` while running the real PQ handshake,
record encryption, lookup, quote verification, and upload implementation.
Production packages should enable only `browser-wasm`.

Upload fixtures follow native policy: seven initial peers, authenticated
witness views, a supported paid median, and four successful stores. Tests
cover inconsistent views, partial existing-holder votes, one payable quote,
even quote counts, wider PUT fallback, and reuse of the paid proof. Structured
remote errors are tested separately from an actual PUT-response deadline.

Record-batch tests upload one file as consecutive caller-staged batches with
separate payments, check that progress keeps file-level record positions, and
confirm each result reports the payment mode the shared coordinator used.
Private-file tests keep the DataMap record local, then download and range-read
through the caller-held DataMap, including one with nested DataMap records.

Download regressions cover known holders omitted by failed discovery, complete
discovery failure, bounded fallback, and unchanged BLAKE3 verification. The mock
retains successful PUT payloads across connection replacements so a full upload,
download and random-access read can run through the real WASM path, including
when the storage holders stop answering FIND_NODE.

The shared read-engine regressions in `src/client_engine/{read,files}.rs` run
with `cargo test -p ant-core --lib client_engine`. They cover native retry timing,
peer deduplication and fallback bounds, typed fetch errors, multi-level DataMaps
on a current-thread runtime, verified records, and range boundaries. Generated
WASM additionally downloads a native shrunk DataMap through the mock network and
checks media ranges across chunk boundaries and EOF.

Upload discovery regressions verify native's twenty-to-seven fallback, reject
persistent five-peer results without payment or PUT, and ensure the browser
adapter does not bypass genuine failed-connection suppression on fallback.
Sequential-upload tests reproduce a first success followed by four FIND_NODE
failures and a 3/7 result; the same client must recover through normal discovery.
A separate test covers grace-cancelled requests, while actual connection failures
remain cached across attempts.

The browser network facade now constructs the ordinary `data::Client`. The Rust
test node decodes native `ant_protocol::ChunkMessage` requests inside encrypted
WebRTC frames, signs canonical evmlib quotes, and decodes the native payment
proof on PUT. The fixtures also exercise saorsa-core identity generation,
export/import, signing, tamper rejection, and browser deadlines without Tokio.
Native GET's immediate integrity-failure behavior applies to the browser too.

Forwarded address-record tests carry real owner signatures in encrypted binary
lookup responses. They check byte-for-byte proof preservation, reject tampered
and expired proofs, and confirm that unsigned outer address metadata cannot
replace the owner's signed addresses. Legacy mock responses exercise the
compatible unsequenced discovery-hint path throughout the existing suite.

Address V2 is signed by definition. Browser tests verify that an authenticated
`addr-v2` capability makes every returned peer proof mandatory; an omitted proof
is rejected. Servers without that capability exercise the V1-era discovery-hint
fallback. The capability token is imported from portable saorsa-core by both
node and client adapters.

Review regressions also exercise durable payment intent/submission/receipt
journals, malformed wallet results without duplicate payment, staged-data
preflight, canonical DataMap metadata, live-route caching with seed fallback,
monotonic frame deadlines, operation-specific response timeouts, and pool
capacity wakeups after cancellation. See `../browser-tests/README.md` for the
Chromium integration against real nodes and Anvil.

`payment-recovery.test.mjs` covers storing earlier paid Merkle batches after a
later payment/recovery failure, explicit reconciliation of rejected and reverted
payments, preservation of paid proofs, failed verification/persistence, and client
closure during asynchronous prepayment checkpoints for both payment modes.

`rpc-deadlines.test.mjs` covers independent queue/send/response budgets, a final
buffer drain with early responses, one HELLO for concurrent cold operations,
reauthentication and capability checks after timeout, bounded admission without
closing active work, pool closure during setup, and strict queued session handles.

`lookup-reuse.test.mjs` covers draining admitted lookups after grace cancellation,
retaining actual dial failures, cancelling queued work, closing background work,
and bounded, deduplicated preconnections from verified owner-address hints. The
shared read-engine tests also cover racing one connected known holder against
ordinary discovery: a miss cannot establish absence, corrupt content remains
fatal, and a slow cached candidate cannot block a discovered holder. These reads
use the same policy on native and WASM. The mock copies outbound bytes before
re-entering WASM, matching the DataChannel ownership boundary across memory growth.

Immutable reads consume bounded candidate updates while discovery is pending.
Early misses do not establish absence, peers are deduplicated within a round,
and a slow early GET can race a different final holder. Native seeds this shared
policy from its routing table; WebRTC also publishes validated lookup replies
as they arrive. Write discovery and witness admission remain unchanged.
Both targets now permit one additional early candidate after a one-second stall,
while preserving the two-GET maximum even when discovery completes. Shared tests
cover the delay, slot handoff, deduplication, and fatal integrity errors. The lane
tests check that a failed cold dial is not repeated by the other queued lane.
Early candidates use authenticated connections, with WebRTC preconnections
publishing hints once ready. Download regressions cover cold closer addresses
blocking neither slot ahead of a ready holder, and retaining the twenty-peer
fallback allowance after more than seven early misses.

`parallel-lanes.test.mjs` checks the two-channel transport profile: discovery and
quotes use the control channel; GET/PUT use an independent authenticated data
channel on the same ICE association. Slow transfers, cancellation and malformed
frames cannot consume the other channel's response or invalidate its PQ session.
Pool limits count peers with up to two live channels each; eviction checks active
leases on both lanes. Nodes must allow the standard two channels per connection.

Multiplexing regressions advertise `rpc-multiplex-4` and delay replies before
AEAD sealing, so out-of-order application completion still preserves encrypted
wire sequence. They check a four-request maximum, slow/cancelled RPC isolation,
no repeated authentication, cancellation before admission, legacy-node fallback,
and read reservations retained through abandoned replies and returned decoding.
