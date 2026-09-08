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

`data::client::upload::Client::upload_records` coordinates native memory uploads,
native spilled files, and browser buffered/staged uploads. It selects Auto,
Single, or Merkle payment with native thresholds, partitions batches, pipelines
single-node waves, and applies the same store retry and recovery policies.
Platform adapters supply bytes, wallet settlement, checkpoint persistence, and
progress callbacks. Browser transport admission checks remain in its adapter.

Prepared Merkle checkpoints preserve the exact salted tree and payment request;
confirmed checkpoints retain native tagged proofs. Native file checkpoints are
atomically persisted under a file/payment-network scope and protected by a file
lock. Browser applications persist checkpoints with the awaited callback.
The SDK ethers and wagmi providers submit calldata encoded by evmlib and decode
settlement with its canonical event ABI. Custom providers can implement
`payMerkle`; applications using only single-node payments can select `single`.

Content addresses call `ant_protocol::compute_address`. Public DataMap records
use `client_engine::files::public_map_record` for both serialization and address
calculation. Whole-file BLAKE3 checksums retain their separate checksum meaning.

## Protocol definitions used by browser adapters

The WebRTC transport has no payment-policy module. Native and WASM clients
import the same definitions at their original application-protocol layer:

| Concern | Canonical definition |
| --- | --- |
| Close-group size and majority | `ant_protocol::{CLOSE_GROUP_SIZE, CLOSE_GROUP_MAJORITY}` |
| Lookup K, concurrency and iteration grace defaults | `saorsa_core::dht_lookup` (re-exported by `ant_protocol::transport`) |
| Pricing curve | `ant_protocol::payment::calculate_price` |
| Commitment type, limits, domains, signature bytes, verification and pin | `ant_protocol::payment::commitment` |
| Quote signing bytes and hash | `ant_protocol::evm::PaymentQuote` (from evmlib) |
| ML-DSA public-key size | `ant_protocol::pqc::api::MlDsaVariant::MlDsa65.public_key_size()` |

Browser quote envelopes only translate JSON/hex fields and check that duplicated
envelope fields match their native encoded payload. Browser test-node fixtures
also construct native quotes and commitments. Native commitment signing uses
the same canonical payload function as verification.

WebRTC frame limits, DataChannel chunk sizes and transfer deadlines remain in
transport: these describe the transport profile, not close-group or payment
policy. Browser memory and connection-pool limits remain adapter settings.
