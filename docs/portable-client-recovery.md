# Shared client routing and upload recovery

Client portability does not enable nodes in WASM. `saorsa_core::client_routing`
contains the native client report-selection and witness-normalization functions.
The native DHT manager and browser client call those same functions. Node
listeners, routing-table maintenance, admission services, and background tasks
remain behind the native feature.

Browser FIND_NODE responses optionally carry the complete serialized native peer
record, preserving address tags and publish sequence. The browser converts that
record to `DHTNode` before selecting reports or assembling witness views. A peer
without a WebRTC endpoint still appears in witness votes; only the dialing
adapter filters it out as a connection candidate. Legacy responses without the
record continue to decode, with their reduced metadata.

`data::client::upload_state::UploadState` owns verified single-node payment plans
and confirmed proof bytes keyed by content address. Native payment finalization
and browser confirmation share its transition and proof builder. Native disk
receipts import their proofs into the same state for reuse. Quote expiry and
future-clock tolerance use the native constants on both platforms.

Browser upload checkpoints are emitted before requesting payment and again
before loading/storing bytes. A failed wallet callback can resume with the exact
retained quote hashes. After confirmed payment, a retry refreshes peers and quotes
but attaches the original paid proof to current PUT targets, without paying for
the replacement quotes. Missing per-quote transactions fail confirmation
atomically. Content is verified against its address and quoted length when
staged bytes are attached.

Checkpoints contain plans/proofs, not file bytes or wallet secrets. The WASM
adapter binds them to record addresses/sizes and payment-network identity. SDK
`upload({ checkpoint, onCheckpoint })` can restore a saved checkpoint with the
same input; `onCheckpoint` may asynchronously persist it and is awaited before
payment or PUTs proceed. The SDK also retains it for `resumeUpload` in the current
page. Applications own durable checkpoint/input storage, as native adapters own
disk receipts. Pending wallet submissions still require settlement observation
and the existing `onPaymentSubmitted` reconciliation path: an unpaid/prepared
checkpoint alone is not evidence that a transaction confirmed.

This change shares the existing single-node upload state and recovery policy.
It does not change browser file/range limits, expose native filesystem APIs, or
add Merkle payment selection to the browser facade. The native Merkle workflow
continues to use its existing shared payment implementation and native file
adapter.
