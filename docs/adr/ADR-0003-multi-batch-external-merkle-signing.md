# ADR-0003: Multi-Batch External Merkle Signing with Spill-Backed Prepared Uploads

- **Status:** Proposed
- **Date:** 2026-08-11
- **Decision owners:** Nic-dorman
- **Reviewers:** ant-client maintainers
- **Supersedes:** none
- **Superseded by:** none
- **Related:** V2-946 (V2-947/V2-948/V2-949 consumers), issues #166/#140, PR #167, ADR-0004 (ant-node, commitment-bound quotes)

## Context

The external-signer upload flow (prepare → out-of-band on-chain payment →
finalize) is how every keyless consumer uploads: the desktop app's
WalletConnect flow and both mobile SDKs. It has two structural limits the
wallet path does not share:

1. **A ~1 GiB hard cap.** The external merkle protocol is *one prepared
   batch → one signature → one winner hash*. `prepare_merkle_batch_external`
   refuses more than `MAX_LEAVES` (2^8 = 256, the payment contract's tree
   depth cap) addresses with `MerkleBatchTooLarge`, because a single
   signature cannot express a split. At ~4 MiB per chunk that caps fresh
   uploads at ≈ 1 GiB. The wallet path has no such cap:
   `pay_for_merkle_multi_batch` partitions with `merkle_batch_partitions`
   and signs one transaction per sub-batch.

2. **File-sized RAM residency across the signing window.** Prepare encrypts
   through the on-disk `ChunkSpill` (bounded memory), then reads every chunk
   back into a resident `Vec<Bytes>` carried inside
   `ExternalPaymentInfo::Merkle`, because finalize needs the bodies after
   the external signing round-trip. The in-code comment is explicit: *"NOT
   memory-bounded for large files."* The wallet path instead stores straight
   from the spill (`upload_merkle_from_spill`, ≤64 bodies in flight,
   ~256 MB peak, 4 GB test-proven). Mobile consumers are the worst exposed:
   the prepared upload sits resident while the user app-switches to their
   wallet, which is exactly when iOS reclaims memory from backgrounded apps.

Neither limit is documented user-facing, and neither is inherent to the
payment contract — the contract is already paid per-tree, N times, by the
wallet path.

## Decision Drivers

- External-signer consumers (desktop, mobile) are the products being taken
  to GA; >1 GiB media files are ordinary user content.
- The wallet path already contains proven machinery for both halves of the
  fix: sub-batch partitioning/payment folding, and spill-streamed storing
  with deferred retries and accurate `PartialUpload` accounting.
- A contract-level batched entry point (`payForMerkleTrees`) would be a
  T3 payments/economics change on its own timeline; the client-side fix
  must not wait for it.
- The external merkle store path has zero automated test coverage (V2-945);
  whatever we build must be exercisable by the existing 35-node Merkle E2E
  CI job with small files.

## Considered Options

1. **Document the limit and stop there.** Zero code risk; leaves GA products
   capped at 1 GiB with file-sized RAM spikes, and pushes splitting onto
   every consumer app (which cannot express it — one DataMap spans the whole
   file).
2. **Shrink chunks at compile time** (`MAX_CHUNK_SIZE` is an `option_env!`).
   Raises the byte cap without touching the protocol, but it is a
   whole-binary constant: it diverges client chunking from the network's,
   multiplies chunk count (and payment cost) for everyone, and does nothing
   about RAM residency.
3. **Contract batching first** (`payForMerkleTrees(batches[])`, one tx).
   Best endgame UX, but T3 (payments/economics: ADR, adversarial review,
   release train) and still needs all the client-side multi-batch plumbing
   this ADR describes. Deferred as an optimization (V2-949).
4. **Client-side multi-batch external signing + spill-backed prepared
   uploads** — mirror the wallet path across the API boundary: prepare
   returns N sub-batches, the signer pays each, finalize takes N winner
   hashes and stores from the spill. **Chosen.**

## Decision

We will reshape the external-signer merkle flow to carry N sub-batches and
keep chunk bodies on disk:

1. **Prepare** partitions the to-upload set with the existing
   `merkle_batch_partitions` rules (≤ `MAX_LEAVES` leaves per batch,
   singleton-remainder rebalanced) and builds one `PreparedMerkleBatch` per
   partition. `ExternalPaymentInfo::Merkle` carries
   `prepared_batches: Vec<PreparedMerkleBatch>`.
2. **Chunk bodies stay in the `ChunkSpill`.** The file-path prepared upload
   carries the spill (an opaque `ExternalChunkStore`) instead of a resident
   `Vec<Bytes>`; the spill's existing lockfile/Drop lifecycle rides the
   prepared-upload session (consumers already park `PreparedUpload` in
   TTL/session maps). The in-memory `data_prepare_upload` path keeps a
   resident store variant — its input is already in memory by definition.
3. **Finalize accepts one winner hash per batch**
   (`finalize_upload_merkle_multi`, `Vec<Option<[u8; 32]>>` aligned to
   batch order). Per-batch proofs are folded exactly like
   `pay_for_merkle_multi_batch` folds them, then chunks are stored via the
   wallet path's spill store engine (`upload_merkle_from_spill`): bounded
   fan-out, deferred retry rounds, quorum shortfalls and
   missing-proof chunks surfaced through the `PartialUpload` contract
   established by PR #167. A batch whose payment the signer abandoned
   (`None` hash) simply contributes no proofs: its chunks land in the
   `PartialUpload` failed set while every paid batch still stores —
   the same forward-progress semantics as a wallet-path sub-batch payment
   failure. The existing single-hash `finalize_upload_merkle` remains as
   the one-batch special case and errors if the upload was prepared as
   multiple batches.
4. **Test seams:** the per-batch leaf cap becomes clamped client
   configuration (`3..=MAX_LEAVES`, default `MAX_LEAVES` — 3 is the floor
   because a cap of 2 cannot partition odd totals into payable ≥2-leaf
   trees) so E2E tests can
   exercise real multi-batch signing with kilobyte files, and
   `file_prepare_upload_with_mode` exposes the payment-mode override the
   wallet path already has, so the external merkle path is testable below
   the 64-chunk auto threshold (V2-945).

This is a breaking change to `ExternalPaymentInfo` and the finalize surface,
shipped in an ant-core 0.6.0 API-break window with coordinated FFI (V2-947)
and desktop (V2-948) updates.

## Consequences

### Positive

- External-signer uploads reach wallet-path size parity: N × ~1 GiB batches,
  one signature each, no protocol cap.
- Peak client RAM for external uploads drops from ≈ file size (held across
  the entire signing window) to the same ~256 MB bound as the wallet path.
- The external merkle store converges onto the battle-tested spill engine —
  deferred retries, accurate partial accounting — instead of a parallel
  resident-body implementation.
- Partial payment is no longer all-or-nothing: k-of-N approved batches make
  forward progress and report the remainder honestly.
- The whole flow becomes E2E-testable with small files in the existing
  Merkle E2E CI job.

### Negative / Trade-offs

- Semver-breaking for every `ExternalPaymentInfo`/finalize consumer; FFI and
  desktop must move in lockstep during the 0.6.0 window.
- Wallet UX costs one approval per ~1 GiB batch until contract batching
  (V2-949); consumers should collapse the ERC-20 allowance to a single
  approval for the summed amount.
- The spill directory now lives as long as the prepared-upload session
  (disk ≈ 1.05× file until finalize/cancel), and a leaked session leaves it
  to the existing stale-spill reaper rather than being freed on prepare
  return.
- The wave-batch external variant keeps resident bodies this pass (< 64
  chunks ⇒ < ~256 MB); folding it onto the spill is follow-up work.

### Neutral / Operational

- The payment contract, node verification, and wire protocol are untouched;
  nodes see identical per-tree payment records (T2 boundary).
- `merkle_payment_timestamp` expiry semantics are unchanged; the merged
  receipt tracks the oldest sub-batch timestamp, as the wallet path does.

## Validation

- Unit: partition sizes under injected caps (incl. singleton-remainder
  cases); per-batch proof folding (paid/unpaid mixes); winner-hash count
  validation; single-hash wrapper refusing multi-batch uploads.
- E2E (Merkle E2E job, small files, batch cap 3): full multi-batch round
  trip — prepare with forced merkle → N `Wallet::pay_for_merkle_tree` calls
  as the simulated signer → `finalize_upload_merkle_multi` → download →
  byte equality; and a partial-payment run (first batch paid only) asserting
  `PartialUpload` with the unpaid chunks failed and the paid chunks stored.
- Memory: the external path inherits the spill engine's ≤64-bodies-in-flight
  bound; the existing huge-file RSS harness pattern applies if a large-file
  soak is wanted.
- Review trigger: revisit when contract batching (V2-949) lands, and when
  the wave-batch external variant is folded onto the spill.

## Notes for AI-assisted work

AI tools may help draft this ADR, but **must not mark it Accepted without
human review**. Accepted ADRs are immutable: create a new superseding ADR
rather than editing an Accepted ADR.
