//! Merkle batch payment support for the Autonomi client.
//!
//! When uploading batches of 64+ chunks, merkle payments reduce gas costs
//! by paying for the entire batch in a single on-chain transaction instead
//! of one transaction per chunk.

use crate::data::client::adaptive::{observe_op, Outcome};
use crate::data::client::classify_error;
use crate::data::client::file::UploadEvent;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{
    Amount, MerklePaymentCandidateNode, MerklePaymentCandidatePool, MerklePaymentProof, MerkleTree,
    MidpointProof, PoolCommitment, CANDIDATES_PER_POOL, MAX_LEAVES,
};
use ant_protocol::payment::commitment::{
    commitment_hash, verify_commitment_signature, StorageCommitment, MAX_COMMITMENT_KEY_COUNT,
    MAX_COMMITMENT_SIDECAR_BYTES,
};
use ant_protocol::payment::{
    calculate_price, serialize_merkle_proof, verify_merkle_candidate_signature,
};
use ant_protocol::transport::PeerId;
use ant_protocol::{
    compute_address, send_and_await_chunk_response, ChunkMessage, ChunkMessageBody,
    MerkleCandidateQuoteRequest, MerkleCandidateQuoteRequestV2, MerkleCandidateQuoteResponse,
    ProtocolError,
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use rand::Rng;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use xor_name::XorName;

/// Default threshold: use merkle payments when chunk count >= this value.
pub const DEFAULT_MERKLE_THRESHOLD: usize = 64;

/// Payment multiplier applied to a quoted price before settlement.
///
/// Deliberately the **same constant** the single-node path uses rather than a
/// second copy of `3`: the whole defect this fixes was the two paths disagreeing
/// about the multiplier, so they now read it from one place. (The storer's
/// `PAID_QUOTE_PAYMENT_MULTIPLIER` and `ant_protocol`'s single-node builder are
/// still separate literals; folding all of them into one `ant-protocol`
/// constant is a follow-up.)
///
/// The single-node path pays the median-priced issuer 3× its quoted price, so
/// the network receives the same revenue as paying three members of the close
/// group while costing one transaction's gas. The merkle path never applied it:
/// it submitted the raw quoted price as the on-chain payable amount, so the
/// contract's `median16(amount) × 2^depth` came to **1×** the median per padded
/// leaf — a third of what the same chunk earns on the single-node path, for
/// identical storage and replication.
///
/// The on-chain field is `CandidateNode.amount`, the sum the vault pays out,
/// not the quote itself; the signed candidate keeps its 1× quoted `price` and
/// the pool hash is unchanged, so every proof still verifies against the
/// quotes the nodes actually signed.
use crate::data::client::payment::SINGLE_NODE_PAYMENT_MULTIPLIER as MERKLE_PAYMENT_MULTIPLIER;

/// ADR-0004 resolve-before-pay gate for a merkle candidate — the merkle-path
/// equivalent of the single-node `quote_commitment_binding_is_valid`. Runs the
/// FULL binding check (shape, cap, exact price, and for bound candidates the
/// commitment parse, peer-binding, signature, `hash == pin`, and
/// `count == key_count`) before the candidate is allowed into a pool the client
/// may pay. `peer_id` is derived from the candidate's `pub_key`
/// (`BLAKE3(pub_key)`), matching the storer.
///
/// Returns `Ok(())` if the binding fully resolves, else `Err(detail)`.
fn merkle_candidate_binding_is_valid(
    peer_id: &PeerId,
    candidate: &MerklePaymentCandidateNode,
    commitment: &Option<Vec<u8>>,
) -> std::result::Result<(), String> {
    let count = candidate.committed_key_count;
    let pin = candidate.commitment_pin;
    match (count, pin.is_some()) {
        (0, false) | (1.., true) => {}
        (1.., false) => {
            return Err(format!(
                "committed_key_count={count} > 0 but commitment_pin is None (unauditable count)"
            ));
        }
        (0, true) => {
            return Err("committed_key_count=0 with a commitment_pin (incoherent baseline)".into());
        }
    }
    if count > MAX_COMMITMENT_KEY_COUNT {
        return Err(format!(
            "committed_key_count={count} exceeds MAX_COMMITMENT_KEY_COUNT={MAX_COMMITMENT_KEY_COUNT}"
        ));
    }
    let expected = calculate_price(count as usize);
    if candidate.price != expected {
        return Err(format!(
            "price {} does not equal calculate_price(committed_key_count={count}) = {expected}",
            candidate.price
        ));
    }

    let Some(pin) = pin else {
        return Ok(()); // baseline candidate pins nothing
    };
    let Some(blob) = commitment else {
        return Err("bound candidate did not ship its commitment; pin is unresolvable".into());
    };
    if blob.len() > MAX_COMMITMENT_SIDECAR_BYTES {
        return Err(format!(
            "shipped commitment is {} bytes, exceeds MAX_COMMITMENT_SIDECAR_BYTES={MAX_COMMITMENT_SIDECAR_BYTES}",
            blob.len()
        ));
    }
    let commitment: StorageCommitment = rmp_serde::from_slice(blob)
        .map_err(|e| format!("shipped commitment did not deserialize: {e}"))?;
    if compute_address(&commitment.sender_public_key) != *peer_id.as_bytes()
        || commitment.sender_peer_id != *peer_id.as_bytes()
    {
        return Err("shipped commitment is not bound to the candidate peer".into());
    }
    if !verify_commitment_signature(&commitment) {
        return Err("shipped commitment has an invalid signature".into());
    }
    if commitment_hash(&commitment) != Some(pin) {
        return Err("shipped commitment does not hash to the candidate's pin".into());
    }
    if commitment.key_count != count {
        return Err(format!(
            "shipped commitment attests key_count={} but the candidate claims {count}",
            commitment.key_count
        ));
    }
    Ok(())
}

/// Build the on-chain [`PoolCommitment`] for a candidate pool, applying
/// [`MERKLE_PAYMENT_MULTIPLIER`] to every candidate's payable amount.
///
/// The contract derives what it pays out from the amounts submitted here
/// (`total = median16(amount) × 2^depth`, split evenly across `depth`
/// winners), so multiplying here — and only here — brings the merkle path to
/// the same per-chunk revenue as the single-node path.
///
/// The pool hash is deliberately left as `pool.hash()`, computed over the
/// candidates' **signed** 1× prices. It is the key the storer resolves the
/// on-chain payment record under, and it commits to the quotes the nodes
/// actually signed; multiplying the payable amount must not disturb it.
fn pool_commitment_with_payment_multiplier(
    pool: &MerklePaymentCandidatePool,
) -> Result<PoolCommitment> {
    let mut commitment = pool.to_commitment();
    let multiplier = Amount::from(MERKLE_PAYMENT_MULTIPLIER);
    for candidate in &mut commitment.candidates {
        candidate.price = candidate.price.checked_mul(multiplier).ok_or_else(|| {
            Error::Payment(format!(
                "Merkle candidate amount overflow applying {MERKLE_PAYMENT_MULTIPLIER}x to price {}",
                candidate.price
            ))
        })?;
    }
    Ok(commitment)
}

/// Turn a merkle candidate quote response into the candidate it carries, or
/// the error that explains why there is none.
///
/// Shared by the versioned request and its legacy retry so the two cannot
/// interpret the same response differently.
///
/// `ClientUpdateRequired` is lifted out of the generic protocol-error case
/// deliberately. It is the one rejection that is about this client rather than
/// about this request, it already carries wording aimed at the person running
/// the upload, and it must not be retried in a shape that would get a quote
/// anyway.
fn map_merkle_candidate_response(
    peer_id: PeerId,
    body: ChunkMessageBody,
) -> Option<Result<(MerklePaymentCandidateNode, Option<Vec<u8>>)>> {
    match body {
        ChunkMessageBody::MerkleCandidateQuoteResponse(MerkleCandidateQuoteResponse::Success {
            candidate_node,
            commitment,
        }) => match rmp_serde::from_slice::<MerklePaymentCandidateNode>(&candidate_node) {
            Ok(node) => Some(Ok((node, commitment))),
            Err(e) => Some(Err(Error::Serialization(format!(
                "Failed to deserialize candidate node from {peer_id}: {e}"
            )))),
        },
        ChunkMessageBody::MerkleCandidateQuoteResponse(MerkleCandidateQuoteResponse::Error(
            refusal @ ProtocolError::ClientUpdateRequired { .. },
        )) => Some(Err(Error::ClientUpdateRequired(refusal.to_string()))),
        ChunkMessageBody::MerkleCandidateQuoteResponse(MerkleCandidateQuoteResponse::Error(
            behind @ ProtocolError::StorerUpdateRequired { .. },
        )) => Some(Err(Error::StorerUpdateRequired(behind.to_string()))),
        ChunkMessageBody::MerkleCandidateQuoteResponse(MerkleCandidateQuoteResponse::Error(e)) => {
            Some(Err(Error::Protocol(format!(
                "Merkle quote error from {peer_id}: {e}"
            ))))
        }
        _ => None,
    }
}

/// Should this failure be retried as an unversioned request?
///
/// A storer built before the settlement version existed cannot decode the
/// versioned request, so it never replies and the send fails at the transport
/// layer. Any response this client *recognises*, refusal included, means the
/// storer understood us, and retrying that in an older shape would defeat the
/// gate.
///
/// A reply that decodes but carries a body the mapper does not recognise is
/// not distinguished: the wait continues and ends in a timeout, so it takes
/// the fallback too. That is no more than a peer could achieve by saying
/// nothing, which is why it is bounded by the cutover guard rather than
/// special-cased here.
///
/// # This is a downgrade path, and it is only safe because nothing can be
/// refused yet
///
/// Silence is **not** proof that a peer cannot parse the request. A dropped
/// response, packet loss, an overloaded peer, or one deliberately discarding
/// versioned requests all look identical from here. So this predicate can be
/// provoked, and a peer that provokes it gets asked again without a version.
///
/// That is harmless only while no client can be refused on version grounds,
/// which is exactly the case while `MIN_SUPPORTED_SETTLEMENT_VERSION` is the
/// first declarable version. The moment the minimum is raised, this becomes a
/// way to obtain a quote the gate meant to withhold, and then to burn a
/// payment against it.
///
/// The guard below turns "remember to delete the fallback before raising the
/// minimum" from a comment into a build failure. It lives in
/// [`crate::data::client::UNVERSIONED_RETRY_REQUIRES_MIN_V1`] and is referenced
/// from every fallback site, including the independent single-node one in
/// `quote.rs`, so deleting one path cannot silently leave the other unguarded.
const _: () = crate::data::client::UNVERSIONED_RETRY_REQUIRES_MIN_V1;

const fn is_version_unaware(error: &Error) -> bool {
    matches!(error, Error::Network(_) | Error::Timeout(_))
}

/// Payment mode for uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentMode {
    /// Automatically choose: merkle for batches >= threshold, single otherwise.
    #[default]
    Auto,
    /// Force merkle batch payment regardless of batch size (min 2 chunks).
    Merkle,
    /// Force single-node payment (one tx per chunk).
    Single,
}

/// Result of a merkle batch payment.
///
/// Serializable so it can be persisted across runs for resume after a
/// partial-upload failure. See `crate::data::client::cached_merkle`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MerkleBatchPaymentResult {
    /// Map of `XorName` to serialized tagged proof bytes (ready to use in PUT requests).
    pub proofs: HashMap<[u8; 32], Vec<u8>>,
    /// Number of chunks in the batch.
    pub chunk_count: usize,
    /// Total storage cost in atto (token smallest unit).
    pub storage_cost_atto: String,
    /// Total gas cost in wei.
    pub gas_cost_wei: u128,
    /// Unix timestamp (seconds) used for the on-chain merkle payment.
    /// Persisted so resume can check whether the on-chain payment has
    /// aged out beyond the merkle expiration window and the cached
    /// receipt must be discarded.
    #[serde(default)]
    pub merkle_payment_timestamp: u64,
}

/// Prepared merkle batch ready for external payment.
///
/// Contains everything needed to submit the on-chain merkle payment
/// and then finalize proof generation without a wallet.
pub struct PreparedMerkleBatch {
    /// Merkle tree depth (needed for the on-chain call).
    pub depth: u8,
    /// Pool commitments for the on-chain call.
    pub pool_commitments: Vec<PoolCommitment>,
    /// Timestamp used for the merkle payment.
    pub merkle_payment_timestamp: u64,
    /// Internal: candidate pools (needed for proof generation after payment).
    candidate_pools: Vec<MerklePaymentCandidatePool>,
    /// Internal: the merkle tree (needed for proof generation).
    tree: MerkleTree,
    /// Internal: chunk addresses in order.
    addresses: Vec<[u8; 32]>,
}

/// Result of checking a merkle upload batch before payment.
#[derive(Debug, Clone, Default)]
pub(crate) struct MerkleUploadPlan {
    /// Chunks already confirmed by their close group.
    pub already_stored: Vec<[u8; 32]>,
    /// Chunks that still need payment and storage.
    pub to_upload: Vec<[u8; 32]>,
    /// Total byte size of chunks in `to_upload`.
    to_upload_total_bytes: u64,
}

impl MerkleUploadPlan {
    /// Average byte size of chunks that still need upload.
    #[must_use]
    pub fn to_upload_avg_size(&self) -> u64 {
        if self.to_upload.is_empty() {
            return 0;
        }

        self.to_upload_total_bytes / self.to_upload.len() as u64
    }
}

impl std::fmt::Debug for PreparedMerkleBatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedMerkleBatch")
            .field("depth", &self.depth)
            .field("pool_commitments", &self.pool_commitments.len())
            .field("merkle_payment_timestamp", &self.merkle_payment_timestamp)
            .field("candidate_pools", &self.candidate_pools.len())
            .field("addresses", &self.addresses.len())
            .finish()
    }
}

/// Select chunk contents that correspond to `addresses`, preserving address order.
///
/// Extra chunk contents are ignored; missing contents for any requested address
/// are treated as corrupted upload state.
pub(crate) fn chunk_contents_for_upload_addresses(
    chunk_contents: Vec<Bytes>,
    addresses: &[[u8; 32]],
) -> Result<Vec<Bytes>> {
    if addresses.is_empty() {
        return Ok(Vec::new());
    }

    let mut needed_by_address: HashMap<[u8; 32], usize> = HashMap::new();
    for address in addresses {
        *needed_by_address.entry(*address).or_default() += 1;
    }

    let mut chunks_by_address: HashMap<[u8; 32], VecDeque<Bytes>> =
        HashMap::with_capacity(needed_by_address.len());
    let mut remaining = addresses.len();
    for chunk in chunk_contents {
        let address = compute_address(&chunk);
        if let Some(needed) = needed_by_address.get_mut(&address) {
            if *needed > 0 {
                chunks_by_address
                    .entry(address)
                    .or_default()
                    .push_back(chunk);
                *needed -= 1;
                remaining -= 1;
                if remaining == 0 {
                    break;
                }
            }
        }
    }

    for (address, needed) in &needed_by_address {
        if *needed == 0 {
            continue;
        }

        if chunks_by_address.contains_key(address) {
            return Err(Error::InvalidData(format!(
                "missing duplicate chunk content for merkle address {}",
                hex::encode(address)
            )));
        }

        return Err(Error::InvalidData(format!(
            "missing chunk content for merkle address {}",
            hex::encode(address)
        )));
    }

    let mut selected = Vec::with_capacity(addresses.len());
    for address in addresses {
        let chunks = chunks_by_address.get_mut(address).ok_or_else(|| {
            Error::InvalidData(format!(
                "missing chunk content for merkle address {}",
                hex::encode(address)
            ))
        })?;
        let chunk = chunks.pop_front().ok_or_else(|| {
            Error::InvalidData(format!(
                "missing duplicate chunk content for merkle address {}",
                hex::encode(address)
            ))
        })?;
        selected.push(chunk);
    }

    Ok(selected)
}

/// Map a per-chunk quote-collection result to its merkle-preflight stored
/// status, degrading gracefully on transient failures.
///
/// The preflight only needs to answer "is this chunk already on the network?"
/// so it can be skipped before payment — it is an optimisation, never a gate.
/// Therefore:
/// - `Ok(_)` (quotes gathered) → `Ok(false)`: chunk is not stored, upload it.
/// - `Err(AlreadyStored)` → `Ok(true)`: skip it.
/// - a transient failure (timeout / insufficient peers / transport error) →
///   `Ok(false)`: we could not confirm, so queue it for upload rather than
///   aborting the whole batch. Re-storing an existing chunk is idempotent.
/// - any other (application) error → propagate; it would recur on a healthy
///   link and is not something the preflight should paper over.
///
/// Without this, a single chunk's transient quote timeout failed the entire
/// upload — fatal for forced `--merkle` uploads, which (unlike `Auto`) have no
/// wave-batch fallback, and catastrophic for large files via the multiplicative
/// per-chunk effect.
fn preflight_stored_status<T>(result: Result<T>) -> Result<bool> {
    match result {
        Ok(_) => Ok(false),
        Err(Error::AlreadyStored) => Ok(true),
        Err(e) if matches!(classify_error(&e), Outcome::Timeout | Outcome::NetworkError) => {
            Ok(false)
        }
        Err(e) => Err(e),
    }
}

/// Split `total` addresses into the merkle batches an upload is actually paid in.
///
/// Every batch becomes one `MerkleTree`, and a tree is only valid with
/// `2..=MAX_LEAVES` leaves. The obvious `addresses.chunks(MAX_LEAVES)` split
/// respects the upper bound but not the lower one: 257 addresses come out as
/// `[256, 1]`, the 256-address batch is paid for on-chain, and the singleton
/// remainder cannot build a tree — so the upload fails as a *paid* partial.
/// Every count congruent to 1 modulo `MAX_LEAVES` (257, 513, 769, …) hits it,
/// and the merkle preflight can leave an arbitrary count behind.
///
/// Borrowing one address from the preceding batch removes the case entirely:
/// 257 splits as `[255, 2]` and 513 as `[256, 255, 2]`. Order is preserved and
/// no address is duplicated or synthesised, so the partition is a plain
/// in-order cover of the input.
///
/// Returns an empty vector for `total < 2`, which no merkle path may pay for —
/// `pay_for_merkle_batch` rejects those counts up front.
#[must_use]
pub fn merkle_batch_sizes(total: usize) -> Vec<usize> {
    merkle_batch_sizes_with_cap(total, MAX_LEAVES)
}

/// [`merkle_batch_sizes`] with an explicit per-batch leaf cap.
///
/// `cap` is clamped to `3..=MAX_LEAVES`: above `MAX_LEAVES` the contract's
/// depth bound rejects the tree, and below 3 the partition is unsound — with
/// a cap of 2 every odd total needs a 1-leaf part, which cannot build a
/// tree (parts of 3 and 2 compose any total ≥ 2, so 3 is the smallest safe
/// cap). Production callers use [`merkle_batch_sizes`]
/// (cap = `MAX_LEAVES`); a smaller cap lets tests exercise real multi-batch
/// signing with kilobyte files (ADR-0003).
#[must_use]
pub fn merkle_batch_sizes_with_cap(total: usize, cap: usize) -> Vec<usize> {
    if total < 2 {
        return Vec::new();
    }
    let cap = cap.clamp(3, MAX_LEAVES);

    let mut sizes = Vec::with_capacity(total.div_ceil(cap));
    let mut remaining = total;
    while remaining > cap {
        // Taking a full cap here would strand a single address as the
        // final batch; take one fewer so the tail is a payable two-leaf tree.
        let take = if remaining - cap == 1 { cap - 1 } else { cap };
        sizes.push(take);
        remaining -= take;
    }
    sizes.push(remaining);
    sizes
}

/// Split `addresses` into the sub-batches [`merkle_batch_sizes`] describes.
///
/// The slices borrow `addresses` in order, so the partition cannot introduce a
/// duplicate or a synthetic address.
#[must_use]
pub fn merkle_batch_partitions(addresses: &[[u8; 32]]) -> Vec<&[[u8; 32]]> {
    merkle_batch_partitions_with_cap(addresses, MAX_LEAVES)
}

/// [`merkle_batch_partitions`] under an explicit per-batch leaf cap
/// (see [`merkle_batch_sizes_with_cap`] for the clamping rules).
#[must_use]
pub fn merkle_batch_partitions_with_cap(addresses: &[[u8; 32]], cap: usize) -> Vec<&[[u8; 32]]> {
    let mut partitions = Vec::new();
    let mut rest = addresses;
    for size in merkle_batch_sizes_with_cap(addresses.len(), cap) {
        let (batch, tail) = rest.split_at(size);
        partitions.push(batch);
        rest = tail;
    }
    partitions
}

/// Fold per-batch [`MerkleBatchPaymentResult`]s into one combined receipt.
///
/// Mirrors the fold `pay_for_merkle_multi_batch` performs on the wallet path:
/// proofs merge by extension, costs sum, and the combined
/// `merkle_payment_timestamp` is the **oldest** sub-batch timestamp so expiry
/// checks use the worst case. Batches the external signer never paid simply
/// do not appear in `results` — their chunks end up with no proof, which the
/// store path reports through `PartialUpload` (ADR-0003).
#[must_use]
pub(crate) fn merge_merkle_batch_results(
    results: Vec<MerkleBatchPaymentResult>,
) -> MerkleBatchPaymentResult {
    let mut merged = MerkleBatchPaymentResult {
        proofs: HashMap::new(),
        chunk_count: 0,
        storage_cost_atto: "0".to_string(),
        gas_cost_wei: 0,
        merkle_payment_timestamp: 0,
    };
    let mut total_storage = Amount::ZERO;
    for result in results {
        merged.proofs.extend(result.proofs);
        merged.chunk_count += result.chunk_count;
        if let Ok(cost) = result.storage_cost_atto.parse::<Amount>() {
            total_storage += cost;
        }
        merged.gas_cost_wei = merged.gas_cost_wei.saturating_add(result.gas_cost_wei);
        if merged.merkle_payment_timestamp == 0
            || (result.merkle_payment_timestamp > 0
                && result.merkle_payment_timestamp < merged.merkle_payment_timestamp)
        {
            merged.merkle_payment_timestamp = result.merkle_payment_timestamp;
        }
    }
    merged.storage_cost_atto = total_storage.to_string();
    merged
}

/// Leaves one merkle batch of `batch_size` addresses is billed for.
///
/// `MerkleTree` pads its leaf count up to a power of two and the vault charges
/// `median16 × 2^depth`, so a 65-address batch pays for 128 leaves.
fn padded_leaf_count(batch_size: usize) -> u64 {
    // Saturate rather than wrap: a batch is at most MAX_LEAVES, so the
    // overflow arm is unreachable, and erring high never under-quotes.
    let padded = batch_size
        .max(2)
        .checked_next_power_of_two()
        .unwrap_or(usize::MAX);
    u64::try_from(padded).unwrap_or(u64::MAX)
}

/// Leaves a merkle upload of `chunk_count` chunks is actually billed for.
///
/// Summed over the batches [`merkle_batch_sizes`] will really pay for, so the
/// estimate and the execution path share one batching model rather than two
/// that can drift. Billing the raw chunk count would under-quote a
/// non-power-of-two batch by up to 2×, and an under-quote is the harmful
/// direction: a caller sizing its wallet from the estimate runs dry
/// mid-upload.
#[must_use]
pub fn merkle_billable_leaves(chunk_count: u64) -> u64 {
    let total = usize::try_from(chunk_count).unwrap_or(usize::MAX);
    let batches = merkle_batch_sizes(total);
    if batches.is_empty() {
        // No payable partition exists below two chunks. Nothing costs nothing;
        // a lone chunk is still quoted as the two-leaf minimum a tree needs.
        return if total == 0 { 0 } else { 2 };
    }

    batches
        .into_iter()
        .map(padded_leaf_count)
        .fold(0u64, u64::saturating_add)
}

/// Reject an address set that cannot be prepared as a single merkle tree.
///
/// The wallet path splits an oversized upload with [`merkle_batch_sizes`] and
/// pays each batch in its own transaction. The external-signer contract is one
/// prepared batch → one signature → one payment, so it has no way to express
/// that split. Refusing before any candidate collection or on-chain spend is
/// the honest answer; silently switching the caller to a different payment
/// model is not.
fn ensure_single_merkle_tree_batch(address_count: usize) -> Result<()> {
    if address_count > MAX_LEAVES {
        return Err(Error::MerkleBatchTooLarge {
            addresses: address_count,
            max_leaves: MAX_LEAVES,
        });
    }
    Ok(())
}

/// Determine whether to use merkle payments for a given batch size.
/// Free function — no Client needed.
#[must_use]
pub fn should_use_merkle(chunk_count: usize, mode: PaymentMode) -> bool {
    match mode {
        PaymentMode::Auto => chunk_count >= DEFAULT_MERKLE_THRESHOLD,
        PaymentMode::Merkle => chunk_count >= 2,
        PaymentMode::Single => false,
    }
}

impl Client {
    /// Determine whether to use merkle payments for a given batch size.
    #[must_use]
    pub fn should_use_merkle(&self, chunk_count: usize, mode: PaymentMode) -> bool {
        should_use_merkle(chunk_count, mode)
    }

    /// Pay for a batch of chunks using merkle batch payment.
    ///
    /// Builds a merkle tree, collects candidate pools, pays on-chain in one tx,
    /// and returns per-chunk proofs. Anything longer than `MAX_LEAVES` is split
    /// by [`merkle_batch_sizes`] and paid one transaction per sub-batch.
    ///
    /// This low-level helper assumes the caller has already selected the
    /// addresses that need payment. User-facing upload paths first run the
    /// merkle upload planner to skip chunks already stored on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if the batch is too small, candidate collection fails,
    /// on-chain payment fails, or proof generation fails.
    pub async fn pay_for_merkle_batch(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<MerkleBatchPaymentResult> {
        // A refusal established by any earlier upload on this client stops
        // this one before it spends. The verdict is about this build, not
        // about one operation, so an upload that started after another had
        // already been told the settlement rules are wrong must not pay.
        if let Some(refusal) = self.corroborated_settlement_refusal() {
            return Err(Error::ClientUpdateRequired(refusal));
        }
        let chunk_count = addresses.len();
        if chunk_count < 2 {
            return Err(Error::Payment(
                "Merkle batch payment requires at least 2 chunks".to_string(),
            ));
        }

        if chunk_count > MAX_LEAVES {
            return self
                .pay_for_merkle_multi_batch(addresses, data_type, data_size)
                .await;
        }

        self.pay_for_merkle_single_batch(addresses, data_type, data_size)
            .await
    }

    /// Check which chunks in a merkle upload still need payment/storage.
    ///
    /// Uses the normal per-chunk quote path because it already has the
    /// close-group majority rule for `AlreadyStored`. Non-stored chunks only
    /// use the quote response as a probe; their actual payment still happens
    /// through the merkle batch.
    ///
    /// `chunks` contains `(address, data_size)` pairs.
    pub(crate) async fn plan_merkle_upload(
        &self,
        chunks: Vec<([u8; 32], u64)>,
        data_type: u32,
        progress: Option<&mpsc::Sender<UploadEvent>>,
    ) -> Result<MerkleUploadPlan> {
        let total_chunks = chunks.len();
        if total_chunks == 0 {
            return Ok(MerkleUploadPlan::default());
        }

        info!("Checking {total_chunks} merkle chunks for existing storage before payment");

        let quote_limiter = self.controller().quote.clone();
        let quote_concurrency = quote_limiter.current().min(total_chunks.max(1));
        let mut check_stream = stream::iter(chunks.into_iter().enumerate())
            .map(|(index, (address, data_size))| {
                let limiter = quote_limiter.clone();
                async move {
                    let result = observe_op(
                        &limiter,
                        || async move {
                            self.chunk_already_stored_for_merkle(&address, data_type, data_size)
                                .await
                        },
                        classify_error,
                    )
                    .await;
                    (index, address, data_size, result)
                }
            })
            .buffer_unordered(quote_concurrency);

        let mut already_stored: Vec<(usize, [u8; 32])> = Vec::new();
        let mut to_upload: Vec<(usize, [u8; 32], u64)> = Vec::new();
        let mut checked = 0usize;

        while let Some((index, address, data_size, result)) = check_stream.next().await {
            let is_already_stored = result?;
            checked += 1;

            if let Some(tx) = progress {
                let _ = tx.try_send(UploadEvent::ChunkQuoted {
                    quoted: checked,
                    total: total_chunks,
                });
            }

            if is_already_stored {
                debug!(
                    "Merkle preflight {checked}/{total_chunks}: chunk {} already stored",
                    hex::encode(address)
                );
                already_stored.push((index, address));
                if let Some(tx) = progress {
                    let _ = tx.try_send(UploadEvent::ChunkStored {
                        stored: already_stored.len(),
                        total: total_chunks,
                    });
                }
            } else {
                debug!(
                    "Merkle preflight {checked}/{total_chunks}: chunk {} needs upload",
                    hex::encode(address)
                );
                to_upload.push((index, address, data_size));
            }
        }

        already_stored.sort_by_key(|(index, _)| *index);
        to_upload.sort_by_key(|(index, _, _)| *index);

        let to_upload_total_bytes = to_upload.iter().fold(0u64, |acc, (_, _, data_size)| {
            acc.saturating_add(*data_size)
        });

        let already_stored = already_stored
            .into_iter()
            .map(|(_, address)| address)
            .collect::<Vec<_>>();
        let to_upload = to_upload
            .into_iter()
            .map(|(_, address, _)| address)
            .collect::<Vec<_>>();

        info!(
            "Merkle preflight complete: {} already stored, {} need upload",
            already_stored.len(),
            to_upload.len()
        );

        Ok(MerkleUploadPlan {
            already_stored,
            to_upload,
            to_upload_total_bytes,
        })
    }

    async fn chunk_already_stored_for_merkle(
        &self,
        address: &[u8; 32],
        data_type: u32,
        data_size: u64,
    ) -> Result<bool> {
        let result = self
            .get_store_quotes_with_fault_tolerance(address, data_size, data_type)
            .await;
        if let Err(e) = &result {
            if matches!(classify_error(e), Outcome::Timeout | Outcome::NetworkError) {
                debug!(
                    "Merkle preflight: could not determine stored status for {} ({e}); \
                     treating as not stored and queuing for upload",
                    hex::encode(address)
                );
            }
        }
        preflight_stored_status(result)
    }

    /// Phase 1 of external-signer merkle payment for an address set of any
    /// size: partition into `MerkleTree`-sized sub-batches and prepare each.
    ///
    /// The partition follows [`merkle_batch_partitions_with_cap`] (≤ `cap`
    /// leaves per batch, singleton-remainder rebalanced), so the external
    /// signer pays one transaction per returned batch — the same shape the
    /// wallet path's `pay_for_merkle_multi_batch` signs internally
    /// (ADR-0003). Batch order matches address order; the caller's finalize
    /// supplies one winner hash per batch in the same order.
    ///
    /// `cap` is clamped to `3..=MAX_LEAVES` (see
    /// [`merkle_batch_sizes_with_cap`] for why 3 is the floor); production
    /// callers pass `MAX_LEAVES`, tests pass small caps to get real
    /// multi-batch flows from kilobyte files.
    ///
    /// # Errors
    ///
    /// Returns an error if any sub-batch's candidate collection fails.
    /// Nothing is spent in either case — payment happens externally after
    /// this returns.
    pub async fn prepare_merkle_batches_external(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
        cap: usize,
    ) -> Result<Vec<PreparedMerkleBatch>> {
        if addresses.len() < 2 {
            return Err(Error::Payment(
                "Merkle batch payment requires at least 2 chunks".to_string(),
            ));
        }
        let partitions = merkle_batch_partitions_with_cap(addresses, cap);
        let total = partitions.len();
        let mut batches = Vec::with_capacity(total);
        for (i, partition) in partitions.into_iter().enumerate() {
            debug!(
                "Preparing external merkle sub-batch {}/{total} ({} chunks)",
                i + 1,
                partition.len()
            );
            batches.push(
                self.prepare_merkle_batch_external(partition, data_type, data_size)
                    .await?,
            );
        }
        Ok(batches)
    }

    /// Phase 1 of external-signer merkle payment: prepare batch without paying.
    ///
    /// Builds the merkle tree, collects candidate pools from the network,
    /// and returns the data needed for the on-chain payment call.
    /// Requires `EvmNetwork` but NOT a wallet.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MerkleBatchTooLarge`] if `addresses` holds more than
    /// `MAX_LEAVES` entries. One prepared batch is one signature and one
    /// payment, so an oversized set has no valid external-signing form; the
    /// wallet path splits it across transactions instead
    /// ([`Client::prepare_merkle_batches_external`] is the partitioned
    /// equivalent for external signing). The check runs before any candidate
    /// collection, so nothing is spent.
    pub async fn prepare_merkle_batch_external(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<PreparedMerkleBatch> {
        // A refusal established by any earlier upload on this client stops
        // this one before it spends. The verdict is about this build, not
        // about one operation, so an upload that started after another had
        // already been told the settlement rules are wrong must not pay.
        if let Some(refusal) = self.corroborated_settlement_refusal() {
            return Err(Error::ClientUpdateRequired(refusal));
        }
        ensure_single_merkle_tree_batch(addresses.len())?;

        let chunk_count = addresses.len();
        let xornames: Vec<XorName> = addresses.iter().map(|a| XorName(*a)).collect();

        debug!("Building merkle tree for {chunk_count} chunks");

        // 1. Build merkle tree
        let tree = MerkleTree::from_xornames(xornames)
            .map_err(|e| Error::Payment(format!("Failed to build merkle tree: {e}")))?;

        let depth = tree.depth();
        let merkle_payment_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::Payment(format!("System time error: {e}")))?
            .as_secs();

        debug!("Merkle tree: depth={depth}, leaves={chunk_count}, ts={merkle_payment_timestamp}");

        // 2. Get reward candidates (midpoint proofs)
        let midpoint_proofs = tree
            .reward_candidates(merkle_payment_timestamp)
            .map_err(|e| Error::Payment(format!("Failed to generate reward candidates: {e}")))?;

        debug!(
            "Collecting candidate pools from {} midpoints (concurrent)",
            midpoint_proofs.len()
        );

        // 3. Collect candidate pools from the network (all pools in parallel).
        //    Each candidate's ADR-0004 binding is fully verified during
        //    collection (shape, cap, exact price, commitment resolution); the
        //    sidecars themselves are consumed by that validation and NOT
        //    forwarded in the PUT bundles (see `finalize_merkle_batch`).
        let candidate_pools = self
            .build_candidate_pools(
                &midpoint_proofs,
                data_type,
                data_size,
                merkle_payment_timestamp,
            )
            .await?;

        // 4. Build pool commitments for on-chain payment. Every candidate's
        //    payable amount carries MERKLE_PAYMENT_MULTIPLIER so a merkle
        //    chunk settles for the same amount a single-node chunk does; the
        //    signed candidate prices and the pool hashes are untouched.
        let pool_commitments: Vec<PoolCommitment> = candidate_pools
            .iter()
            .map(pool_commitment_with_payment_multiplier)
            .collect::<Result<Vec<_>>>()?;

        Ok(PreparedMerkleBatch {
            depth,
            pool_commitments,
            merkle_payment_timestamp,
            candidate_pools,
            tree,
            addresses: addresses.to_vec(),
        })
    }

    /// Pay for a single batch (up to `MAX_LEAVES` chunks).
    async fn pay_for_merkle_single_batch(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<MerkleBatchPaymentResult> {
        let wallet = self.require_wallet()?;
        let prepared = self
            .prepare_merkle_batch_external(addresses, data_type, data_size)
            .await?;

        info!(
            "Submitting merkle batch payment on-chain (depth={})",
            prepared.depth
        );
        let (winner_pool_hash, amount, gas_info) = wallet
            .pay_for_merkle_tree(
                prepared.depth,
                prepared.pool_commitments.clone(),
                prepared.merkle_payment_timestamp,
            )
            .await
            .map_err(|e| Error::Payment(format!("Merkle batch payment failed: {e}")))?;

        info!(
            "Merkle payment succeeded: winner pool {}",
            hex::encode(winner_pool_hash)
        );

        let mut result = finalize_merkle_batch(prepared, winner_pool_hash)?;
        result.storage_cost_atto = amount.to_string();
        result.gas_cost_wei = gas_info.gas_cost_wei;
        Ok(result)
    }

    /// Handle batches larger than `MAX_LEAVES` by splitting into sub-batches.
    async fn pay_for_merkle_multi_batch(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<MerkleBatchPaymentResult> {
        // Partition with the shared helper, NOT `chunks(MAX_LEAVES)`: the naive
        // split leaves a one-address final batch for every count congruent to 1
        // modulo MAX_LEAVES, which cannot build a tree and so turns a paid
        // upload into a partial failure.
        let sub_batches = merkle_batch_partitions(addresses);
        let total_sub_batches = sub_batches.len();
        let mut all_proofs = HashMap::with_capacity(addresses.len());
        let mut total_storage = Amount::ZERO;
        let mut total_gas: u128 = 0;
        // Track the oldest sub-batch timestamp so the overall receipt
        // expires when the *first* sub-batch's on-chain payment ages
        // out (worst case for resume).
        let mut oldest_ts: u64 = 0;

        for (i, chunk) in sub_batches.into_iter().enumerate() {
            match self
                .pay_for_merkle_single_batch(chunk, data_type, data_size)
                .await
            {
                Ok(sub_result) => {
                    if let Ok(cost) = sub_result.storage_cost_atto.parse::<Amount>() {
                        total_storage += cost;
                    }
                    total_gas = total_gas.saturating_add(sub_result.gas_cost_wei);
                    if oldest_ts == 0
                        || (sub_result.merkle_payment_timestamp > 0
                            && sub_result.merkle_payment_timestamp < oldest_ts)
                    {
                        oldest_ts = sub_result.merkle_payment_timestamp;
                    }
                    all_proofs.extend(sub_result.proofs);
                }
                Err(e) => {
                    if all_proofs.is_empty() {
                        // First sub-batch failed, nothing paid yet -- propagate directly.
                        return Err(e);
                    }
                    // A storer saying this client cannot settle is terminal
                    // even mid-batch. Folding it into a partial result would
                    // report success, hide the upgrade instruction, and leave
                    // the caller to rediscover the same refusal on its next
                    // upload. Every remaining sub-batch would be refused the
                    // same way, so there is nothing to salvage by continuing.
                    if matches!(e, Error::ClientUpdateRequired(_)) {
                        // Do NOT return Err here. The caller writes the receipt
                        // cache only on the Ok path, so failing the call would
                        // discard proofs for sub-batches whose payment has
                        // already settled on-chain and cannot be undone. That is
                        // the very destruction this work exists to stop.
                        //
                        // The verdict is not lost by returning the partial
                        // result: it was latched client-wide when it was
                        // corroborated, so the next quote collection refuses
                        // before spending anything and the upgrade instruction
                        // reaches the user from there.
                        warn!(
                            "Merkle sub-batch {}/{total_sub_batches}: storers refused this \
                             client's settlement version. Returning {} proofs from \
                             already-paid sub-batches so that spend is not stranded; the \
                             refusal is latched and will stop the next payment.",
                            i + 1,
                            all_proofs.len()
                        );
                    }
                    // Return partial result so caller can still store already-paid chunks.
                    warn!(
                        "Merkle sub-batch {}/{total_sub_batches} failed: {e}. \
                         Returning {} proofs from prior sub-batches",
                        i + 1,
                        all_proofs.len()
                    );
                    return Ok(MerkleBatchPaymentResult {
                        chunk_count: all_proofs.len(),
                        proofs: all_proofs,
                        storage_cost_atto: total_storage.to_string(),
                        gas_cost_wei: total_gas,
                        merkle_payment_timestamp: oldest_ts,
                    });
                }
            }
        }

        Ok(MerkleBatchPaymentResult {
            chunk_count: addresses.len(),
            proofs: all_proofs,
            storage_cost_atto: total_storage.to_string(),
            gas_cost_wei: total_gas,
            merkle_payment_timestamp: oldest_ts,
        })
    }

    /// Build candidate pools by querying the network for each midpoint (concurrently).
    async fn build_candidate_pools(
        &self,
        midpoint_proofs: &[MidpointProof],
        data_type: u32,
        data_size: u64,
        merkle_payment_timestamp: u64,
    ) -> Result<Vec<MerklePaymentCandidatePool>> {
        let mut pool_futures = FuturesUnordered::new();

        for midpoint_proof in midpoint_proofs {
            let pool_address = midpoint_proof.address();
            let mp = midpoint_proof.clone();
            pool_futures.push(async move {
                let candidate_nodes = self
                    .get_merkle_candidate_pool(
                        &pool_address.0,
                        data_type,
                        data_size,
                        merkle_payment_timestamp,
                    )
                    .await?;
                Ok::<_, Error>(MerklePaymentCandidatePool {
                    midpoint_proof: mp,
                    candidate_nodes,
                })
            });
        }

        let mut pools = Vec::with_capacity(midpoint_proofs.len());
        while let Some(result) = pool_futures.next().await {
            pools.push(result?);
        }

        Ok(pools)
    }

    /// Collect `CANDIDATES_PER_POOL` (16) merkle candidate quotes from the network.
    #[allow(clippy::too_many_lines)]
    async fn get_merkle_candidate_pool(
        &self,
        address: &[u8; 32],
        data_type: u32,
        data_size: u64,
        merkle_payment_timestamp: u64,
    ) -> Result<[MerklePaymentCandidateNode; CANDIDATES_PER_POOL]> {
        let node = self.network().node();
        let timeout = Duration::from_secs(self.config().quote_timeout_secs);

        // Query extra peers to handle validation failures (bad sigs, wrong type, etc.)
        let query_count = CANDIDATES_PER_POOL * 2;
        let mut remote_peers = self
            .network()
            .find_closest_peers(address, query_count)
            .await?;

        // If DHT closest-nodes didn't return enough, supplement with connected peers.
        // On small networks the DHT iterative lookup may not discover enough peers
        // close to a random pool address, but we know more peers via direct connections.
        if remote_peers.len() < CANDIDATES_PER_POOL {
            let connected = self.network().connected_peers().await;
            for peer in connected {
                if !remote_peers.iter().any(|(id, _)| *id == peer) {
                    remote_peers.push((peer, vec![]));
                }
            }
        }

        if remote_peers.len() < CANDIDATES_PER_POOL {
            return Err(Error::InsufficientPeers(format!(
                "Found {} peers, need {CANDIDATES_PER_POOL} for merkle candidate pool. \
                 Use --no-merkle or a larger network.",
                remote_peers.len()
            )));
        }

        let mut candidate_futures = FuturesUnordered::new();

        let unversioned_peers = self.unversioned_quote_peers();
        let versioned_capable = self.versioned_quote_capable_handle();

        for (peer_id, peer_addrs) in &remote_peers {
            let request_id = self.next_request_id();
            // A peer that already failed to answer a versioned request is
            // asked in the legacy shape directly. Re-probing costs a full
            // per-peer timeout every time, and a merkle pool asks sixteen
            // candidates per pool, so against a fleet that predates the
            // versioned requests the probes dominate the run: measured on this
            // suite it went from ~24 minutes to past the 60-minute CI cap.
            // The capable set wins. The two sets are updated under separate
            // locks, so a slow probe can insert into the legacy set after a
            // concurrent request has already proved the peer capable; letting
            // capability win makes that interleaving harmless instead of
            // permanently preferring the legacy shape.
            let known_legacy = !versioned_capable
                .lock()
                .is_ok_and(|peers| peers.contains(peer_id))
                && unversioned_peers
                    .lock()
                    .is_ok_and(|peers| peers.contains(peer_id));

            let legacy_request = MerkleCandidateQuoteRequest {
                address: *address,
                data_type,
                data_size,
                merkle_payment_timestamp,
            };

            // Declare the settlement version so a storer can turn us away
            // before we pay, rather than refusing the payment afterwards.
            let message = ChunkMessage {
                request_id,
                body: if known_legacy {
                    ChunkMessageBody::MerkleCandidateQuoteRequest(legacy_request.clone())
                } else {
                    ChunkMessageBody::MerkleCandidateQuoteRequestV2(
                        MerkleCandidateQuoteRequestV2::new(
                            *address,
                            data_size,
                            merkle_payment_timestamp,
                        ),
                    )
                },
            };

            let message_bytes = match message.encode() {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("Failed to encode merkle candidate request for {peer_id}: {e}");
                    continue;
                }
            };

            // Fallback for the mixed fleet. A storer that predates the
            // versioned request cannot decode it and simply never answers, so
            // without this every quote would fail until the whole network had
            // upgraded. Retried only on a transport-level failure, never on a
            // refusal: see `is_version_unaware` below.
            //
            // Delete this, and the second request id, once the fleet is known
            // to answer V2.
            let legacy_request_id = self.next_request_id();
            let legacy_message_bytes = match (ChunkMessage {
                request_id: legacy_request_id,
                body: ChunkMessageBody::MerkleCandidateQuoteRequest(legacy_request),
            })
            .encode()
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("Failed to encode legacy merkle candidate request for {peer_id}: {e}");
                    continue;
                }
            };

            let peer_id_clone = *peer_id;
            let addrs_clone = peer_addrs.clone();
            let node_clone = node.clone();
            let peers_handle = Arc::clone(&unversioned_peers);
            let capable_handle = Arc::clone(&versioned_capable);

            let fut = async move {
                // First contact waits only long enough to learn whether the
                // peer can parse the shape. A peer already known to be legacy
                // gets the caller's full patience, because that request is the
                // real one rather than a probe.
                let attempt_timeout = if known_legacy {
                    timeout
                } else {
                    timeout.min(crate::data::client::VERSIONED_QUOTE_PROBE_CEILING)
                };

                let result = send_and_await_chunk_response(
                    &node_clone,
                    &peer_id_clone,
                    message_bytes,
                    request_id,
                    attempt_timeout,
                    &addrs_clone,
                    |body| map_merkle_candidate_response(peer_id_clone, body),
                    |e| {
                        Error::Network(format!(
                            "Failed to send merkle candidate request to {peer_id_clone}: {e}"
                        ))
                    },
                    || {
                        Error::Timeout(format!(
                            "Timeout waiting for merkle candidate from {peer_id_clone}"
                        ))
                    },
                )
                .await;

                // Any answer at all to the versioned shape proves the peer
                // can parse it, so it can never later be demoted.
                // Any recognised answer proves the peer parsed the versioned
                // shape, including a structured error. Only silence and send
                // failures leave the question open.
                let answered = match &result {
                    Ok(_) => true,
                    Err(e) => !is_version_unaware(e),
                };
                if !known_legacy && answered {
                    if let Ok(mut peers) = capable_handle.lock() {
                        peers.insert(peer_id_clone);
                    }
                }

                // Silence from a storer means it could not decode the
                // versioned request, so ask again in the shape it understands.
                // A storer that answered with a refusal is NOT retried: it
                // understood us and said no, and asking again without the
                // version would talk it into quoting a client that cannot pay.
                let result = match result {
                    Err(ref e) if is_version_unaware(e) && !known_legacy => {
                        // Only silence is evidence the peer cannot parse the
                        // shape. A send failure means the request never
                        // arrived and teaches nothing, so it must not strand
                        // the peer in the legacy shape for the whole session.
                        // Nor may a peer that has answered a versioned request
                        // before be demoted by one lost response.
                        let ever_answered = capable_handle
                            .lock()
                            .is_ok_and(|peers| peers.contains(&peer_id_clone));
                        if matches!(e, Error::Timeout(_)) && !ever_answered {
                            if let Ok(mut peers) = peers_handle.lock() {
                                peers.insert(peer_id_clone);
                            }
                        }
                        debug!(
                            "Peer {peer_id_clone} did not answer a versioned merkle quote; \
                             retrying in the legacy shape"
                        );
                        send_and_await_chunk_response(
                            &node_clone,
                            &peer_id_clone,
                            legacy_message_bytes,
                            legacy_request_id,
                            timeout,
                            &addrs_clone,
                            |body| map_merkle_candidate_response(peer_id_clone, body),
                            |e| {
                                Error::Network(format!(
                                    "Failed to send merkle candidate request to {peer_id_clone}: {e}"
                                ))
                            },
                            || {
                                Error::Timeout(format!(
                                    "Timeout waiting for merkle candidate from {peer_id_clone}"
                                ))
                            },
                        )
                        .await
                    }
                    other => other,
                };

                (peer_id_clone, result)
            };

            candidate_futures.push(fut);
        }

        self.collect_validated_candidates(&mut candidate_futures, address, merkle_payment_timestamp)
            .await
    }

    /// Collect and validate merkle candidate responses, then return the
    /// `CANDIDATES_PER_POOL` valid responders that are XOR-closest to the
    /// pool midpoint.
    ///
    /// Why distance-sort instead of "first N to respond":
    /// the storing-node verifier re-runs a network closest-peers lookup of
    /// the pool midpoint and rejects the pool if fewer than 13 of the 16
    /// candidate `pub_keys` appear in that authoritative close-set. Pools
    /// built from the fastest-to-respond quoters fail this check whenever
    /// truly-close peers are slower (NAT/relay paths) than farther peers.
    async fn collect_validated_candidates(
        &self,
        futures: &mut FuturesUnordered<
            impl std::future::Future<
                Output = (
                    PeerId,
                    std::result::Result<(MerklePaymentCandidateNode, Option<Vec<u8>>), Error>,
                ),
            >,
        >,
        target_address: &[u8; 32],
        merkle_payment_timestamp: u64,
    ) -> Result<[MerklePaymentCandidateNode; CANDIDATES_PER_POOL]> {
        let mut valid: Vec<(PeerId, MerklePaymentCandidateNode)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        while let Some((peer_id, result)) = futures.next().await {
            match result {
                Ok((candidate, commitment)) => {
                    if !verify_merkle_candidate_signature(&candidate) {
                        warn!("Invalid ML-DSA-65 signature from merkle candidate {peer_id}");
                        failures.push(format!("{peer_id}: invalid signature"));
                        continue;
                    }
                    if candidate.merkle_payment_timestamp != merkle_payment_timestamp {
                        warn!("Timestamp mismatch from merkle candidate {peer_id}");
                        failures.push(format!("{peer_id}: timestamp mismatch"));
                        continue;
                    }
                    // The candidate's identity is `BLAKE3(candidate.pub_key)` —
                    // this is what the storer derives (verifier.rs). Require it
                    // to equal the network responder so a two-identity operator
                    // cannot answer as B while shipping A's commitment.
                    let candidate_peer = PeerId::from_bytes(compute_address(&candidate.pub_key));
                    if candidate_peer != peer_id {
                        warn!(
                            "Dropping merkle candidate {peer_id} — pub_key derives {candidate_peer}, \
                             not the responding peer"
                        );
                        failures.push(format!("{peer_id}: candidate pub_key/peer mismatch"));
                        continue;
                    }
                    // ADR-0004: the FULL resolve-before-pay binding check, same as
                    // the single-node path — a candidate priced off its committed
                    // count, or shipping an unresolvable/forged commitment, is
                    // dropped before it can enter a pool the client pays. Checked
                    // against the CANDIDATE peer (the one the storer audits). The
                    // shipped commitment is consumed here (resolution only) and
                    // not forwarded in the PUT bundles (see
                    // `finalize_merkle_batch`).
                    if let Err(detail) =
                        merkle_candidate_binding_is_valid(&candidate_peer, &candidate, &commitment)
                    {
                        warn!("Dropping merkle candidate {peer_id} — ADR-0004 binding invalid: {detail}");
                        failures.push(format!("{peer_id}: bad commitment binding ({detail})"));
                        continue;
                    }
                    valid.push((candidate_peer, candidate));
                }
                // A storer that has explicitly declared this client
                // incompatible ends the batch, but only once enough distinct
                // peers agree. Collecting a corroborated refusal as one more
                // failed peer would let the pool fill from the remaining
                // sixteen and go on to pay, which is the burn this mechanism
                // exists to prevent; acting on a single peer's unauthenticated
                // word would instead let one hostile responder deny every
                // upload.
                Err(e @ Error::ClientUpdateRequired(_)) => {
                    if let Some(corroborated) =
                        self.note_settlement_refusal(peer_id, &e.to_string())
                    {
                        return Err(Error::ClientUpdateRequired(corroborated));
                    }
                    warn!("Merkle candidate {peer_id} refused this client's settlement version; awaiting corroboration");
                    failures.push(format!("{peer_id}: {e}"));
                }
                Err(e) => {
                    debug!("Failed to get merkle candidate from {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));
                }
            }
        }

        if valid.len() < CANDIDATES_PER_POOL {
            return Err(Error::InsufficientPeers(format!(
                "Got {} merkle candidates, need {CANDIDATES_PER_POOL}. Failures: [{}]",
                valid.len(),
                failures.join("; ")
            )));
        }

        let target_peer = PeerId::from_bytes(*target_address);
        valid.sort_by_key(|(peer_id, _)| peer_id.xor_distance(&target_peer));

        let candidates: Vec<MerklePaymentCandidateNode> = valid
            .into_iter()
            .take(CANDIDATES_PER_POOL)
            .map(|(_, candidate)| candidate)
            .collect();

        let array: [MerklePaymentCandidateNode; CANDIDATES_PER_POOL] =
            candidates.try_into().map_err(|_| {
                Error::Payment("Failed to convert candidates to fixed array".to_string())
            })?;
        Ok(array)
    }

    /// Upload chunks using pre-computed merkle proofs from a batch payment.
    ///
    /// Each chunk is matched to its proof from `batch_result.proofs`, then
    /// stored to its close group concurrently. A per-chunk quorum shortfall
    /// (`InsufficientPeers`) does **not** abort the file: such chunks are
    /// collected and retried — with the same reusable proof and a freshly
    /// re-collected close group — for up to [`MERKLE_STORE_MAX_ATTEMPTS`]
    /// attempts. `stored_offset` carries chunks already confirmed by an earlier
    /// preflight (used for progress numbering and the returned `stored` count),
    /// and `total_chunks` is the whole-file total for progress events.
    ///
    /// Returns how many chunks are stored (including `stored_offset`), how many
    /// remained short of quorum after all retries, and the aggregate store
    /// stats.
    ///
    /// # Errors
    ///
    /// Returns an error only for non-quorum failures (e.g. a missing proof, or a
    /// chunk-count/address mismatch); quorum shortfalls are reported via
    /// [`MerkleStoreOutcome::failed`].
    pub(crate) async fn merkle_upload_chunks(
        &self,
        chunk_contents: Vec<Bytes>,
        addresses: Vec<[u8; 32]>,
        batch_result: &MerkleBatchPaymentResult,
        progress: Option<&mpsc::Sender<UploadEvent>>,
        stored_offset: usize,
        total_chunks: usize,
    ) -> Result<MerkleStoreOutcome> {
        let store_limiter = self.controller().store.clone();
        // Clamp fan-out to batch size — partial batches should not
        // pay for unused slots (see PERF-RESULTS.md).
        let batch_size = chunk_contents.len();
        if batch_size != addresses.len() {
            return Err(Error::InvalidData(format!(
                "merkle upload has {batch_size} chunk contents but {} addresses",
                addresses.len()
            )));
        }
        // Cap closure re-read per scheduler refill so mid-flight limiter growth
        // is applied to the rest of the batch (V2-554). Clamped to batch size —
        // partial batches should not pay for unused slots (see PERF-RESULTS.md).
        let cap = || store_limiter.current().min(batch_size.max(1));

        // External-signer path: the chunk bodies are already resident in memory,
        // so `store_one` clones each from this map on demand. (The whole-file
        // path instead reads bodies from the on-disk spill — same `store_one(addr)`
        // shape, so the store scheduler never carries a resident set of bodies.)
        let bodies: std::collections::HashMap<[u8; 32], Bytes> =
            addresses.iter().copied().zip(chunk_contents).collect();
        let addrs = addresses;

        // Store one chunk to its (freshly re-collected) close group. Called
        // once per chunk per attempt, so a retry round naturally lands on a
        // converged routing table. Only `InsufficientPeers` is recoverable;
        // a missing proof stays fatal.
        let store_one = |addr: [u8; 32]| {
            let limiter = store_limiter.clone();
            let content = bodies.get(&addr).cloned();
            let proof_bytes = batch_result.proofs.get(&addr).cloned();
            async move {
                let started = std::time::Instant::now();
                let content = content.ok_or_else(|| {
                    Error::InvalidData(format!("missing chunk body for {}", hex::encode(addr)))
                })?;
                let proof = proof_bytes.ok_or_else(|| {
                    Error::Payment(format!(
                        "Missing merkle proof for chunk {}",
                        hex::encode(addr)
                    ))
                })?;
                let peers = self.put_target_peers(&addr).await?;
                observe_op(
                    &limiter,
                    || async move { self.chunk_put_to_close_group(content, proof, &peers).await },
                    classify_error,
                )
                .await
                .map(|_| started)
            }
        };

        let outcome = merkle_store_with_retry(
            addrs,
            cap,
            MERKLE_STORE_MAX_ATTEMPTS,
            MERKLE_RETRY_BACKOFF,
            progress,
            stored_offset,
            total_chunks,
            store_one,
        )
        .await?;

        // The external-signer path treats a non-quorum error as terminal (its
        // finalize has no further waves to fold progress into), so re-raise
        // the fatal that `merkle_store_with_retry` now carries in the outcome.
        // The CLI/spill paths, which fold progress across waves before
        // surfacing `PartialUpload`, read `fatal` directly instead. Quorum
        // shortfalls stay in `failed`/`failed_addresses`; the external-signer
        // finalize turns those into `PartialUpload` too (issue #166).
        if let Some(e) = outcome.fatal {
            return Err(e);
        }
        Ok(outcome)
    }
}

/// Total store-attempt budget for a merkle batch: the initial attempt plus up
/// to three retries. Chosen to match the wave path's contract
/// (`batch.rs` iterates `0..=MAX_RETRIES` with `MAX_RETRIES = 3`) and the
/// four-slot [`WaveAggregateStats::retries_histogram`], so a chunk that lands
/// on the final retry is recorded in `retries_histogram[3]`.
///
/// A chunk's close group can transiently reject its `winner_pool` midpoint
/// while a few nodes' routing tables disagree about that midpoint; the network
/// converges within minutes. Per-chunk proofs are reusable, so retrying the
/// same proof after a short backoff recovers these shortfalls for free — no
/// re-payment and no new pool.
pub(crate) const MERKLE_STORE_MAX_ATTEMPTS: usize = 4;

/// Base backoff between merkle store attempts. The routing-table divergence
/// that causes `InsufficientPeers` resolves on the order of minutes, so a short
/// sleep between rounds is enough to land on a converged close group. The
/// actual wait is jittered by [`MERKLE_RETRY_JITTER`] so a large failed set
/// does not re-fire against the same divergent nodes in lockstep.
pub(crate) const MERKLE_RETRY_BACKOFF: Duration = Duration::from_secs(30);

/// Fractional jitter applied to [`MERKLE_RETRY_BACKOFF`] (±10%), spreading the
/// retry wave so convergent nodes are not all probed at the same instant.
const MERKLE_RETRY_JITTER: f64 = 0.1;

/// Outcome of storing a merkle batch: how many chunks landed, how many
/// remained short of quorum after all retries, and the aggregate store stats.
#[derive(Debug, Default)]
pub(crate) struct MerkleStoreOutcome {
    /// Chunks that reached quorum, including any `stored_offset` carried in
    /// from a preflight (counted once, even if they needed retries).
    pub stored: usize,
    /// Addresses confirmed stored by this call (excludes the `stored_offset`
    /// preflight carry-in — those have no address here). The caller appends
    /// these to the file's stored set; using the explicit set (rather than
    /// inferring "input minus failed") keeps accounting correct even when a
    /// `fatal` error aborts the pass mid-flight, leaving some input chunks
    /// neither stored nor in `failed_addresses`.
    pub stored_addresses: Vec<[u8; 32]>,
    /// Chunks still short of quorum after [`MERKLE_STORE_MAX_ATTEMPTS`].
    pub failed: usize,
    /// Addresses (and the last error message) of chunks still short of quorum
    /// after all retries. Empty when `failed == 0`. Both the CLI path and the
    /// external-signer finalize build
    /// [`crate::data::Error::PartialUpload`] from this set.
    pub failed_addresses: Vec<([u8; 32], String)>,
    /// Set when a non-quorum (fatal) store error aborted the pass. Successes
    /// completed before the abort are still recorded in `stored`/
    /// `stored_addresses`; the chunks that had already failed quorum are in
    /// `failed_addresses`; chunks still in flight when the abort hit are in
    /// neither (the caller treats input-minus-stored as failed). Callers that
    /// want the old "fatal aborts everything" contract re-raise this as `Err`.
    pub fatal: Option<Error>,
    /// Aggregate store stats (durations, attempts, per-round retry histogram).
    pub stats: crate::data::client::batch::WaveAggregateStats,
}

/// Drive a set of merkle chunk stores with bounded retry of quorum shortfalls.
///
/// Runs `store_one(addr)` over all `addrs` concurrently, keeping up to `cap()`
/// stores in flight and RE-READING `cap()` as each slot frees — so mid-flight
/// adaptive growth is applied to the rest of the round instead of being frozen
/// at a per-round snapshot (V2-554). `store_one` acquires the chunk body itself
/// (e.g. reads it from the on-disk spill), so only the ≤`cap()` in-flight stores
/// hold a body in memory — the caller passes addresses, never a resident set of
/// bodies, which is what lets the whole-file store run as one cap-bounded
/// fan-out instead of memory-bounded waves. Collects quorum shortfalls
/// (`InsufficientPeers`, `CloseGroupShortfall`, `RemotePut`) rather than
/// aborting. Failed chunks are retried — `store_one` re-collects their close
/// group on each call, so a converged routing table can yield a fresh group —
/// for up to `max_attempts` rounds, sleeping a jittered `backoff` between
/// rounds. A chunk's success is counted once and recorded in the retry round it
/// landed on (`retries_histogram[round]`). `stored_offset` seeds the returned
/// `stored` count and the progress numbering; `total` is the whole-file total
/// reported in progress events.
///
/// A non-quorum error stops the pass but does **not** discard progress: the
/// successes already completed this pass stay in `stored`/`stored_addresses`,
/// the quorum shortfalls so far stay in `failed_addresses`, and the error is
/// returned in [`MerkleStoreOutcome::fatal`] (as `Ok(outcome)`, not `Err`).
/// Callers that want the old abort-everything behaviour re-raise `fatal` as
/// `Err`; CLI callers fold it into `PartialUpload` while keeping the stores.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn merkle_store_with_retry<F, Fut, C>(
    addrs: Vec<[u8; 32]>,
    cap: C,
    max_attempts: usize,
    backoff: Duration,
    progress: Option<&mpsc::Sender<UploadEvent>>,
    stored_offset: usize,
    total: usize,
    store_one: F,
) -> Result<MerkleStoreOutcome>
where
    F: Fn([u8; 32]) -> Fut,
    Fut: std::future::Future<Output = Result<std::time::Instant>>,
    C: Fn() -> usize,
{
    let attempts = max_attempts.max(1);
    let mut outcome = MerkleStoreOutcome {
        stored: stored_offset,
        ..MerkleStoreOutcome::default()
    };
    let mut pending = addrs;

    for attempt in 0..attempts {
        // Carries the failing address forward for the next round plus the last
        // quorum-shortfall message, so an exhausted set can report per-chunk
        // errors via `failed_addresses`. The chunk BODY is not carried — each
        // `store_one` re-reads it on demand, so at most `cap` bodies are ever
        // resident regardless of the total chunk count.
        let mut next_failed: Vec<([u8; 32], String)> = Vec::new();

        // Rolling scheduler: keep up to `cap()` stores in flight, re-reading the
        // cap as each slot frees so limiter growth mid-round is applied to the
        // remaining chunks (V2-554). Iterator exhaustion bounds the launch count
        // to the pending set, so no explicit clamp to `pending.len()` is needed.
        let mut pending_iter = pending.into_iter();
        let mut in_flight = FuturesUnordered::new();
        loop {
            let slots = cap().max(1);
            while in_flight.len() < slots {
                match pending_iter.next() {
                    Some(addr) => {
                        let fut = store_one(addr);
                        in_flight.push(async move { (addr, fut.await) });
                    }
                    None => break,
                }
            }
            let Some((addr, result)) = in_flight.next().await else {
                break;
            };
            outcome.stats.chunk_attempts_total =
                outcome.stats.chunk_attempts_total.saturating_add(1);
            match result {
                Ok(started) => {
                    let duration_ms =
                        u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                    outcome.stats.store_durations_ms.push(duration_ms);
                    let idx = attempt.min(outcome.stats.retries_histogram.len().saturating_sub(1));
                    outcome.stats.retries_histogram[idx] =
                        outcome.stats.retries_histogram[idx].saturating_add(1);
                    outcome.stored += 1;
                    outcome.stored_addresses.push(addr);
                    if let Some(tx) = progress {
                        let _ = tx.try_send(UploadEvent::ChunkStored {
                            stored: outcome.stored,
                            total,
                        });
                    }
                }
                // A quorum shortfall — whether a timeout-bearing capacity
                // shortfall (`InsufficientPeers`), a pure dial-churn shortfall
                // (`CloseGroupShortfall`, V2-554), or an app-only rejection
                // (`RemotePut`, e.g. pool-rejected / quote-stale / disk-full,
                // which are transient) — is recoverable: defer and retry the
                // chunk rather than aborting the whole upload (V2-468 / V2-554).
                Err(
                    e @ (Error::InsufficientPeers(_)
                    | Error::CloseGroupShortfall(_)
                    | Error::RemotePut { .. }),
                ) => {
                    next_failed.push((addr, e.to_string()));
                }
                Err(e) => {
                    // Non-quorum error: fatal. Stop consuming the stream but do
                    // NOT discard the outcome — successes already completed this
                    // pass stay recorded in `stored`/`stored_addresses`. Record
                    // the fatal chunk itself (and any quorum shortfalls seen so
                    // far) as failed; anything still in flight is left for the
                    // caller to treat as not-stored (input minus
                    // `stored_addresses`).
                    next_failed.push((addr, e.to_string()));
                    outcome.fatal = Some(e);
                    break;
                }
            }
        }

        if outcome.fatal.is_some() {
            outcome.failed = next_failed.len();
            outcome.failed_addresses = next_failed;
            return Ok(outcome);
        }

        if next_failed.is_empty() {
            break;
        }

        if attempt + 1 < attempts {
            warn!(
                failed = next_failed.len(),
                attempt = attempt + 1,
                "merkle chunks short of quorum, retrying after backoff"
            );
            pending = next_failed.into_iter().map(|(addr, _msg)| addr).collect();
            if backoff > Duration::ZERO {
                // Jitter the wait (±MERKLE_RETRY_JITTER) so a large failed set
                // does not re-probe the same divergent nodes in lockstep.
                // `thread_rng` is !Send, so the value is computed and the rng
                // dropped before the await to keep this future Send.
                let wait = {
                    let mut rng = rand::thread_rng();
                    let factor = 1.0 + rng.gen_range(-MERKLE_RETRY_JITTER..=MERKLE_RETRY_JITTER);
                    backoff.mul_f64(factor)
                };
                tokio::time::sleep(wait).await;
            }
        } else {
            outcome.failed = next_failed.len();
            outcome.failed_addresses = next_failed;
            break;
        }
    }

    Ok(outcome)
}

/// Round delays (seconds) for the merkle upload deferred-retry pass. Round 0
/// fires immediately — most quorum shortfalls on a healthy network are
/// momentary close-group divergence that clears in well under a second, and
/// serializing them behind mandatory sleeps was the single biggest throughput
/// sink in the wave path (one bad chunk parked the other 63 slots for minutes).
/// Only chunks that survive a round get a longer back-off before the next, so a
/// genuinely saturated/diverged group still gets time to settle. Mirrors the
/// download path's `DEFERRED_ROUND_DELAYS_SECS`.
pub(crate) const DEFERRED_ROUND_DELAYS_SECS: [u64; 3] = [0, 15, 45];

/// Histogram slot for a deferred-retry round's successes.
///
/// The wave first pass lands in slot 0; deferred round `r` (0-indexed) lands in
/// slot `r + 1`, clamped to the last slot so the four-slot
/// [`WaveAggregateStats::retries_histogram`] keeps recording "which round a
/// chunk landed on" under the post-wave deferred structure.
pub(crate) fn deferred_round_histogram_slot(round: usize, hist_len: usize) -> usize {
    (round + 1).min(hist_len.saturating_sub(1))
}

/// Outcome of the post-wave deferred-retry pass.
#[derive(Debug, Default)]
pub(crate) struct DeferredRetryOutcome {
    /// Running total of stored chunks, seeded with the `stored_offset` passed in
    /// (i.e. everything the wave passes already stored) and advanced by each
    /// deferred round's successes.
    pub stored: usize,
    /// Addresses that reached quorum during the deferred rounds (to be appended
    /// to the file's `stored` set).
    pub stored_addresses: Vec<[u8; 32]>,
    /// Count of chunks still short of quorum after the final deferred round.
    pub failed: usize,
    /// Addresses (and last quorum-shortfall message) still short after the final
    /// round, or — when `fatal` is set — the chunks that were still pending when
    /// a non-quorum error aborted the pass.
    pub failed_addresses: Vec<([u8; 32], String)>,
    /// Set when a deferred round hit a non-quorum (fatal) store error. The
    /// caller surfaces this as `PartialUpload` preserving everything stored so
    /// far, mirroring the wave path's fatal handling.
    pub fatal: Option<String>,
    /// Aggregate store stats merged across rounds, with each round's successes
    /// already mapped into its [`deferred_round_histogram_slot`].
    pub stats: crate::data::client::batch::WaveAggregateStats,
}

/// Retry a file-level set of quorum-short merkle chunks in concurrent rounds.
///
/// This is the upload analogue of the download path's deferred-retry loop. The
/// whole-file store pass hands its quorum-short chunks here. Each round stores
/// all still-pending chunks in a single cap-bounded pass via
/// [`merkle_store_with_retry`] at `concurrency_for(len)` — `store_one(addr)`
/// re-reads each body on demand (from the spill), so only the ≤`concurrency_for`
/// in-flight stores hold a body and peak resident memory stays bounded
/// regardless of the deferred-chunk count. Survivors carry to the next round
/// after a `round_delays_secs` sleep. Chunks still short after the final round
/// become `failed_addresses`; a non-quorum store error stops the pass and is
/// reported via `fatal` (with the quorum shortfalls seen so far recorded as
/// `failed_addresses`) so the caller can surface `PartialUpload` — reconciled
/// against the full address list — without discarding earlier progress.
///
/// `store_one`, `progress`, `stored_offset` and `total` mirror
/// [`merkle_store_with_retry`].
#[allow(clippy::too_many_arguments)]
pub(crate) async fn merkle_deferred_retry<CF, SF, Fut>(
    deferred: Vec<([u8; 32], String)>,
    round_delays_secs: &[u64],
    concurrency_for: CF,
    progress: Option<&mpsc::Sender<UploadEvent>>,
    stored_offset: usize,
    total: usize,
    store_one: SF,
) -> Result<DeferredRetryOutcome>
where
    CF: Fn(usize) -> usize,
    SF: Fn([u8; 32]) -> Fut,
    Fut: std::future::Future<Output = Result<std::time::Instant>>,
{
    let mut outcome = DeferredRetryOutcome {
        stored: stored_offset,
        ..DeferredRetryOutcome::default()
    };
    let mut remaining = deferred;
    let rounds = round_delays_secs.len();

    for (round, &delay_secs) in round_delays_secs.iter().enumerate() {
        if remaining.is_empty() {
            break;
        }
        if delay_secs > 0 {
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
        }
        info!(
            "Deferred merkle retry round {}/{}: {} chunk(s) short of quorum",
            round + 1,
            rounds,
            remaining.len(),
        );

        // Store this round's whole pending set in one cap-bounded pass. A
        // single-pass round records its successes in histogram slot 0, so
        // redirect them into the round's own slot.
        let slot = deferred_round_histogram_slot(round, outcome.stats.retries_histogram.len());
        let round_addrs: Vec<[u8; 32]> = std::mem::take(&mut remaining)
            .into_iter()
            .map(|(addr, _msg)| addr)
            .collect();
        let round_len = round_addrs.len();
        // Re-read the cap per scheduler refill (V2-554) via `concurrency_for`,
        // which re-samples the store limiter clamped to this round's size.
        let cap = || concurrency_for(round_len);

        let round_outcome = merkle_store_with_retry(
            round_addrs,
            cap,
            1,
            Duration::ZERO,
            progress,
            outcome.stored,
            total,
            &store_one,
        )
        .await?;

        outcome.stored = round_outcome.stored;
        outcome
            .stored_addresses
            .extend(round_outcome.stored_addresses);

        // Merge stats, redirecting this round's successes to its slot.
        outcome.stats.chunk_attempts_total = outcome
            .stats
            .chunk_attempts_total
            .saturating_add(round_outcome.stats.chunk_attempts_total);
        outcome
            .stats
            .store_durations_ms
            .extend(round_outcome.stats.store_durations_ms);
        let landed: usize = round_outcome.stats.retries_histogram.iter().sum();
        outcome.stats.retries_histogram[slot] =
            outcome.stats.retries_histogram[slot].saturating_add(landed);

        if let Some(fatal) = round_outcome.fatal {
            // Fatal mid-pass: confirmed stores are preserved above. The store
            // helper left this round's quorum shortfalls in `failed_addresses`;
            // chunks still in flight / not yet launched are reconciled against
            // the full address list by the caller's `partial_upload_after_fatal`.
            outcome.fatal = Some(fatal.to_string());
            outcome.failed = round_outcome.failed_addresses.len();
            outcome.failed_addresses = round_outcome.failed_addresses;
            return Ok(outcome);
        }

        // Quorum-short chunks from this round survive to the next.
        remaining = round_outcome.failed_addresses;
    }

    outcome.failed = remaining.len();
    outcome.failed_addresses = remaining;
    Ok(outcome)
}

/// Phase 2 of external-signer merkle payment: generate proofs from winner.
///
/// Takes the prepared batch and the winner pool hash returned by the
/// on-chain payment transaction. Generates per-chunk merkle proofs.
pub fn finalize_merkle_batch(
    prepared: PreparedMerkleBatch,
    winner_pool_hash: [u8; 32],
) -> Result<MerkleBatchPaymentResult> {
    let chunk_count = prepared.addresses.len();
    let xornames: Vec<XorName> = prepared.addresses.iter().map(|a| XorName(*a)).collect();

    // Find the winner pool
    let winner_pool = prepared
        .candidate_pools
        .iter()
        .find(|pool| pool.hash() == winner_pool_hash)
        .ok_or_else(|| {
            Error::Payment(format!(
                "Winner pool {} not found in candidate pools",
                hex::encode(winner_pool_hash)
            ))
        })?;

    // ADR-0004: commitment sidecars are deliberately NOT forwarded in the
    // per-chunk proofs. Sixteen sidecars are ~214 KB serialized, and copying
    // them into every chunk's bundle pushed the proof past the storer's
    // payment-proof size cap, rejecting every merkle PUT once nodes carried
    // live commitments. The client still fully resolves every candidate's
    // commitment before paying (during pool collection); the storer's
    // cross-check is best-effort and resolves pins from its gossip cache or a
    // `GetCommitmentByPin` fetch when no sidecar is shipped.

    // Generate proofs for each chunk
    info!("Generating merkle proofs for {chunk_count} chunks");
    let mut proofs = HashMap::with_capacity(chunk_count);

    for (i, xorname) in xornames.iter().enumerate() {
        let address_proof = prepared
            .tree
            .generate_address_proof(i, *xorname)
            .map_err(|e| {
                Error::Payment(format!(
                    "Failed to generate address proof for chunk {i}: {e}"
                ))
            })?;

        let merkle_proof = MerklePaymentProof::new(*xorname, address_proof, winner_pool.clone());

        let tagged_bytes = serialize_merkle_proof(&merkle_proof)
            .map_err(|e| Error::Serialization(format!("Failed to serialize merkle proof: {e}")))?;

        proofs.insert(prepared.addresses[i], tagged_bytes);
    }

    info!("Merkle batch payment complete: {chunk_count} proofs generated");

    Ok(MerkleBatchPaymentResult {
        proofs,
        chunk_count,
        storage_cost_atto: "0".to_string(),
        gas_cost_wei: 0,
        merkle_payment_timestamp: prepared.merkle_payment_timestamp,
    })
}

/// Compile-time assertions that merkle method futures are Send.
#[cfg(test)]
mod send_assertions {
    use super::*;
    use crate::data::client::Client;

    fn _assert_send<T: Send>(_: &T) {}

    #[allow(
        dead_code,
        unreachable_code,
        unused_variables,
        clippy::diverging_sub_expression
    )]
    async fn _merkle_upload_chunks_is_send(client: &Client) {
        let batch_result: MerkleBatchPaymentResult = todo!();
        let fut = client.merkle_upload_chunks(Vec::new(), Vec::new(), &batch_result, None, 0, 0);
        _assert_send(&fut);
    }
}

/// Test-only builders shared by this module's tests and the external-finalize
/// tests in `file.rs` (ADR-0003).
#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub(crate) mod test_support {
    use super::*;
    use ant_protocol::evm::RewardsAddress;

    pub(crate) fn make_test_addresses(count: usize) -> Vec<[u8; 32]> {
        (0..count)
            .map(|i| {
                let xn = XorName::from_content(&i.to_le_bytes());
                xn.0
            })
            .collect()
    }

    pub(crate) fn make_dummy_candidate_nodes(
        timestamp: u64,
    ) -> [MerklePaymentCandidateNode; CANDIDATES_PER_POOL] {
        std::array::from_fn(|i| MerklePaymentCandidateNode {
            pub_key: vec![i as u8; 32],
            price: Amount::from(1024u64),
            reward_address: RewardsAddress::new([i as u8; 20]),
            merkle_payment_timestamp: timestamp,
            signature: vec![i as u8; 64],
            committed_key_count: 0,
            commitment_pin: None,
        })
    }

    pub(crate) fn make_prepared_merkle_batch(count: usize) -> PreparedMerkleBatch {
        let addrs = make_test_addresses(count);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames).unwrap();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let midpoints = tree.reward_candidates(timestamp).unwrap();

        let candidate_pools: Vec<MerklePaymentCandidatePool> = midpoints
            .into_iter()
            .map(|mp| MerklePaymentCandidatePool {
                midpoint_proof: mp,
                candidate_nodes: make_dummy_candidate_nodes(timestamp),
            })
            .collect();

        let pool_commitments = candidate_pools
            .iter()
            .map(pool_commitment_with_payment_multiplier)
            .collect::<Result<Vec<_>>>()
            .unwrap();

        PreparedMerkleBatch {
            depth: tree.depth(),
            pool_commitments,
            merkle_payment_timestamp: timestamp,
            candidate_pools,
            tree,
            addresses: addrs,
        }
    }

    /// A winner pool hash `finalize_merkle_batch` will accept for `batch`
    /// (the first candidate pool's) — the same selection the existing
    /// finalize tests use. Lives here because `candidate_pools` is private
    /// outside this module.
    pub(crate) fn winner_hash_for(batch: &PreparedMerkleBatch) -> [u8; 32] {
        batch.candidate_pools[0].hash()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::test_support::*;
    use super::*;
    use ant_protocol::evm::{Amount, MerkleTree, RewardsAddress, CANDIDATES_PER_POOL};

    // =========================================================================
    // should_use_merkle (free function, no Client needed)
    // =========================================================================

    #[test]
    fn test_auto_below_threshold() {
        assert!(!should_use_merkle(1, PaymentMode::Auto));
        assert!(!should_use_merkle(10, PaymentMode::Auto));
        assert!(!should_use_merkle(63, PaymentMode::Auto));
    }

    #[test]
    fn test_auto_at_and_above_threshold() {
        assert!(should_use_merkle(64, PaymentMode::Auto));
        assert!(should_use_merkle(65, PaymentMode::Auto));
        assert!(should_use_merkle(1000, PaymentMode::Auto));
    }

    #[test]
    fn test_merkle_mode_forces_at_2() {
        assert!(!should_use_merkle(1, PaymentMode::Merkle));
        assert!(should_use_merkle(2, PaymentMode::Merkle));
        assert!(should_use_merkle(3, PaymentMode::Merkle));
    }

    #[test]
    fn test_single_mode_always_false() {
        assert!(!should_use_merkle(0, PaymentMode::Single));
        assert!(!should_use_merkle(64, PaymentMode::Single));
        assert!(!should_use_merkle(1000, PaymentMode::Single));
    }

    #[test]
    fn test_default_mode_is_auto() {
        assert_eq!(PaymentMode::default(), PaymentMode::Auto);
    }

    #[test]
    fn test_threshold_value() {
        assert_eq!(DEFAULT_MERKLE_THRESHOLD, 64);
    }

    // =========================================================================
    // preflight_stored_status — must degrade gracefully, never abort the batch
    // =========================================================================

    #[test]
    fn test_preflight_quotes_gathered_means_not_stored() {
        assert!(matches!(preflight_stored_status(Ok(())), Ok(false)));
    }

    #[test]
    fn test_preflight_already_stored_is_stored() {
        let r: Result<()> = Err(Error::AlreadyStored);
        assert!(matches!(preflight_stored_status(r), Ok(true)));
    }

    /// The regression: a transient quote-quorum failure during the preflight
    /// must NOT propagate (which would abort the whole forced-merkle upload).
    /// It is treated as "not known to be stored" → queue the chunk for upload.
    #[test]
    fn test_preflight_transient_quote_failure_does_not_abort() {
        // The exact error STG-01 hit: couldn't gather a 7-quote quorum.
        let insufficient: Result<()> =
            Err(Error::InsufficientPeers("Got 5 quotes, need 7".to_string()));
        assert!(
            matches!(preflight_stored_status(insufficient), Ok(false)),
            "insufficient-peers during preflight must degrade to not-stored, not error"
        );

        let timeout: Result<()> = Err(Error::Timeout("Timeout waiting for quote".to_string()));
        assert!(matches!(preflight_stored_status(timeout), Ok(false)));

        let network: Result<()> = Err(Error::Network("connection reset".to_string()));
        assert!(matches!(preflight_stored_status(network), Ok(false)));
    }

    /// Genuine application errors still propagate — the preflight should not
    /// silently swallow problems that would recur on a healthy link.
    #[test]
    fn test_preflight_application_error_propagates() {
        let payment: Result<()> = Err(Error::Payment("bad payment".to_string()));
        assert!(matches!(
            preflight_stored_status(payment),
            Err(Error::Payment(_))
        ));
    }

    #[test]
    fn chunk_contents_for_upload_addresses_preserves_requested_order() {
        let first = Bytes::from_static(b"first");
        let second = Bytes::from_static(b"second");
        let first_addr = compute_address(&first);
        let second_addr = compute_address(&second);

        let selected = chunk_contents_for_upload_addresses(
            vec![first.clone(), second.clone()],
            &[second_addr, first_addr],
        )
        .unwrap();

        assert_eq!(selected, vec![second, first]);
    }

    #[test]
    fn chunk_contents_for_upload_addresses_preserves_duplicate_requests() {
        let repeated = Bytes::from_static(b"same-content");
        let other = Bytes::from_static(b"other-content");
        let repeated_addr = compute_address(&repeated);

        let selected = chunk_contents_for_upload_addresses(
            vec![repeated.clone(), other, repeated.clone()],
            &[repeated_addr, repeated_addr],
        )
        .unwrap();

        assert_eq!(selected, vec![repeated.clone(), repeated]);
    }

    #[test]
    fn chunk_contents_for_upload_addresses_ignores_unrequested_duplicates() {
        let requested = Bytes::from_static(b"requested-content");
        let unrequested = Bytes::from_static(b"unrequested-content");
        let requested_addr = compute_address(&requested);

        let selected = chunk_contents_for_upload_addresses(
            vec![
                unrequested.clone(),
                requested.clone(),
                unrequested.clone(),
                unrequested,
            ],
            &[requested_addr],
        )
        .unwrap();

        assert_eq!(selected, vec![requested]);
    }

    #[test]
    fn chunk_contents_for_upload_addresses_errors_for_missing_content() {
        let present = Bytes::from_static(b"present-content");
        let missing = Bytes::from_static(b"missing-content");
        let missing_addr = compute_address(&missing);

        let result = chunk_contents_for_upload_addresses(vec![present], &[missing_addr]);

        assert!(matches!(result, Err(Error::InvalidData(_))));
    }

    // =========================================================================
    // MerkleTree construction and proof generation (pure, no network)
    // =========================================================================

    #[test]
    fn test_tree_depth_for_known_sizes() {
        let cases = [(2, 1), (4, 2), (16, 4), (100, 7), (256, 8)];
        for (count, expected_depth) in cases {
            let addrs = make_test_addresses(count);
            let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
            let tree = MerkleTree::from_xornames(xornames).unwrap();
            assert_eq!(
                tree.depth(),
                expected_depth,
                "depth mismatch for {count} leaves"
            );
        }
    }

    #[test]
    fn test_proof_generation_and_verification_for_all_leaves() {
        let addrs = make_test_addresses(16);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames.clone()).unwrap();

        for (i, xn) in xornames.iter().enumerate() {
            let proof = tree.generate_address_proof(i, *xn).unwrap();
            assert!(proof.verify(), "proof for leaf {i} should verify");
            assert_eq!(proof.depth(), tree.depth() as usize);
        }
    }

    #[test]
    fn test_proof_fails_for_wrong_address() {
        let addrs = make_test_addresses(8);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames).unwrap();

        let wrong = XorName::from_content(b"wrong");
        let proof = tree.generate_address_proof(0, wrong).unwrap();
        assert!(!proof.verify(), "proof with wrong address should fail");
    }

    #[test]
    fn test_tree_too_few_leaves() {
        let xornames = vec![XorName::from_content(b"only_one")];
        let result = MerkleTree::from_xornames(xornames);
        assert!(result.is_err());
    }

    #[test]
    fn test_tree_at_max_leaves() {
        let addrs = make_test_addresses(MAX_LEAVES);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames).unwrap();
        assert_eq!(tree.leaf_count(), MAX_LEAVES);
    }

    // =========================================================================
    // Proof serialization round-trip
    // =========================================================================

    #[test]
    fn test_merkle_proof_serialize_deserialize_roundtrip() {
        use ant_protocol::evm::{Amount, MerklePaymentCandidateNode, RewardsAddress};
        use ant_protocol::payment::{deserialize_merkle_proof, serialize_merkle_proof};

        let addrs = make_test_addresses(4);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames.clone()).unwrap();

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let candidates = tree.reward_candidates(timestamp).unwrap();
        let midpoint = candidates.first().unwrap().clone();

        // Build candidate nodes (with dummy signatures — not ML-DSA, just for serialization test)
        #[allow(clippy::cast_possible_truncation)]
        let candidate_nodes: [MerklePaymentCandidateNode; CANDIDATES_PER_POOL] =
            std::array::from_fn(|i| MerklePaymentCandidateNode {
                pub_key: vec![i as u8; 32],
                price: Amount::from(1024u64),
                reward_address: RewardsAddress::new([i as u8; 20]),
                merkle_payment_timestamp: timestamp,
                signature: vec![i as u8; 64],
                committed_key_count: 0,
                commitment_pin: None,
            });

        let pool = MerklePaymentCandidatePool {
            midpoint_proof: midpoint,
            candidate_nodes,
        };

        let address_proof = tree.generate_address_proof(0, xornames[0]).unwrap();
        let merkle_proof = MerklePaymentProof::new(xornames[0], address_proof, pool);

        let tagged = serialize_merkle_proof(&merkle_proof).unwrap();
        assert_eq!(
            tagged.first().copied(),
            Some(0x02),
            "tag should be PROOF_TAG_MERKLE"
        );

        let deserialized = deserialize_merkle_proof(&tagged).unwrap();
        assert_eq!(deserialized.address, merkle_proof.address);
        assert_eq!(
            deserialized.winner_pool.candidate_nodes.len(),
            CANDIDATES_PER_POOL
        );
    }

    // =========================================================================
    // Candidate validation logic
    // =========================================================================

    #[test]
    fn test_candidate_wrong_timestamp_rejected() {
        // Simulates what collect_validated_candidates checks
        let candidate = MerklePaymentCandidateNode {
            pub_key: vec![0u8; 32],
            price: ant_protocol::evm::Amount::ZERO,
            reward_address: ant_protocol::evm::RewardsAddress::new([0u8; 20]),
            merkle_payment_timestamp: 1000,
            signature: vec![0u8; 64],
            committed_key_count: 0,
            commitment_pin: None,
        };

        // Timestamp check: 1000 != 2000
        assert_ne!(candidate.merkle_payment_timestamp, 2000);
    }

    // =========================================================================
    // finalize_merkle_batch (external signer)
    // =========================================================================

    /// Candidate pool with distinct prices, so the median is a specific
    /// candidate rather than an artifact of every price being equal.
    fn pool_with_varied_prices(timestamp: u64) -> MerklePaymentCandidatePool {
        let addrs = make_test_addresses(4);
        let xornames: Vec<XorName> = addrs.iter().map(|a| XorName(*a)).collect();
        let tree = MerkleTree::from_xornames(xornames).unwrap();
        let midpoint = tree
            .reward_candidates(timestamp)
            .unwrap()
            .into_iter()
            .next()
            .unwrap();

        let candidate_nodes = std::array::from_fn(|i| MerklePaymentCandidateNode {
            pub_key: vec![i as u8; 32],
            // 100, 200, ... 1600 — upper median (index 8 of 16) is 900.
            price: Amount::from((i as u64 + 1) * 100),
            reward_address: RewardsAddress::new([i as u8; 20]),
            merkle_payment_timestamp: timestamp,
            signature: vec![i as u8; 64],
            committed_key_count: 0,
            commitment_pin: None,
        });

        MerklePaymentCandidatePool {
            midpoint_proof: midpoint,
            candidate_nodes,
        }
    }

    /// The contract's `median16`: upper median, index 8 of 16 ascending.
    fn median16(mut amounts: Vec<Amount>) -> Amount {
        amounts.sort_unstable();
        *amounts.get(amounts.len() / 2).unwrap()
    }

    #[test]
    fn pool_commitment_applies_payment_multiplier_to_every_candidate() {
        let pool = pool_with_varied_prices(1_700_000_000);
        let commitment = pool_commitment_with_payment_multiplier(&pool).unwrap();

        for (candidate, signed) in commitment
            .candidates
            .iter()
            .zip(pool.candidate_nodes.iter())
        {
            assert_eq!(
                candidate.price,
                signed.price * Amount::from(MERKLE_PAYMENT_MULTIPLIER),
                "on-chain payable amount must be {MERKLE_PAYMENT_MULTIPLIER}x the quoted price"
            );
        }
    }

    #[test]
    fn pool_commitment_multiplier_leaves_signed_prices_and_pool_hash_untouched() {
        let pool = pool_with_varied_prices(1_700_000_000);
        let before: Vec<Amount> = pool.candidate_nodes.iter().map(|c| c.price).collect();

        let commitment = pool_commitment_with_payment_multiplier(&pool).unwrap();

        let after: Vec<Amount> = pool.candidate_nodes.iter().map(|c| c.price).collect();
        assert_eq!(before, after, "signed candidate prices must not change");
        assert_eq!(
            commitment.pool_hash,
            pool.hash(),
            "pool hash is the storer's on-chain lookup key and must be \
             computed over the signed 1x prices"
        );
    }

    /// The invariant the fix exists for, stated at the level it actually
    /// holds: the median payable amount over the winning pool is
    /// `MERKLE_PAYMENT_MULTIPLIER x` the median *quoted* price. The contract
    /// spends `median16(amount) * 2^depth` over `2^depth` **padded** leaves, so
    /// this is the settlement per padded leaf — the same figure the single-node
    /// path pays its median-priced issuer per chunk. It says nothing about cost
    /// per *actual* chunk: a batch whose size is not a power of two pays for
    /// padding leaves too (that is what `merkle_billable_leaves` bills for).
    #[test]
    fn merkle_settlement_per_padded_leaf_is_the_multiplied_pool_median() {
        let pool = pool_with_varied_prices(1_700_000_000);
        let commitment = pool_commitment_with_payment_multiplier(&pool).unwrap();

        let quoted_median = median16(pool.candidate_nodes.iter().map(|c| c.price).collect());
        let per_chunk = median16(commitment.candidates.iter().map(|c| c.price).collect());

        assert_eq!(quoted_median, Amount::from(900u64));
        assert_eq!(
            per_chunk,
            quoted_median * Amount::from(MERKLE_PAYMENT_MULTIPLIER),
            "merkle per-chunk settlement must equal the single-node \
             {MERKLE_PAYMENT_MULTIPLIER}x median, not the bare quoted price"
        );
    }

    #[test]
    fn test_finalize_merkle_batch_with_valid_winner() {
        let prepared = make_prepared_merkle_batch(4);
        let winner_hash = prepared.candidate_pools[0].hash();

        let result = finalize_merkle_batch(prepared, winner_hash);
        assert!(
            result.is_ok(),
            "should succeed with valid winner: {result:?}"
        );

        let batch = result.unwrap();
        assert_eq!(batch.chunk_count, 4);
        assert_eq!(batch.proofs.len(), 4);

        // Every proof should be non-empty
        for proof_bytes in batch.proofs.values() {
            assert!(!proof_bytes.is_empty());
        }
    }

    /// DEV-01 regression: per-chunk merkle proofs must never ship commitment
    /// sidecars — all 16 winner-pool sidecars (~214 KB serialized) copied into
    /// every chunk's proof pushed it past the storer's payment-proof size cap,
    /// rejecting every merkle PUT once nodes carried live commitments. The
    /// e2e (`adr0004_merkle_upload_against_bound_candidates`) proves the flow
    /// end-to-end; this pins the wire invariant directly so it cannot slip
    /// back in behind a raised node-side cap.
    #[test]
    fn test_finalize_merkle_batch_ships_no_commitment_sidecars() {
        use ant_protocol::payment::deserialize_merkle_proof;

        let mut prepared = make_prepared_merkle_batch(4);
        // Bind every candidate to a pin, mirroring a network where all nodes
        // carry live commitments (the DEV-01 trigger state).
        for pool in &mut prepared.candidate_pools {
            for candidate in &mut pool.candidate_nodes {
                candidate.committed_key_count = 9_000;
                candidate.commitment_pin = Some([7u8; 32]);
            }
        }
        let winner_hash = prepared.candidate_pools[0].hash();

        let batch = finalize_merkle_batch(prepared, winner_hash).unwrap();
        assert_eq!(batch.proofs.len(), 4);
        for proof_bytes in batch.proofs.values() {
            let proof = deserialize_merkle_proof(proof_bytes).unwrap();
            assert!(
                proof.commitment_sidecars.is_empty(),
                "per-chunk merkle proofs must not ship commitment sidecars"
            );
        }
    }

    #[test]
    fn test_finalize_merkle_batch_with_invalid_winner() {
        let prepared = make_prepared_merkle_batch(4);
        let bad_hash = [0xFF; 32];

        let result = finalize_merkle_batch(prepared, bad_hash);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found in candidate pools"), "got: {err}");
    }

    #[test]
    fn test_finalize_merkle_batch_proofs_are_deserializable() {
        use ant_protocol::payment::deserialize_merkle_proof;

        let prepared = make_prepared_merkle_batch(8);
        let winner_hash = prepared.candidate_pools[0].hash();

        let batch = finalize_merkle_batch(prepared, winner_hash).unwrap();

        for (addr, proof_bytes) in &batch.proofs {
            let proof = deserialize_merkle_proof(proof_bytes);
            assert!(
                proof.is_ok(),
                "proof for {} should deserialize: {:?}",
                hex::encode(addr),
                proof.err()
            );
        }
    }

    // =========================================================================
    // Batch splitting edge cases
    // =========================================================================

    /// Counts spanning every interesting case: the minimum tree, the merkle
    /// threshold and just past it, either side of a full batch, and the
    /// `1 mod MAX_LEAVES` counts the naive `chunks(MAX_LEAVES)` split turned
    /// into an unpayable `[..., 1]` tail.
    const PARTITION_CASES: [(usize, &[usize]); 10] = [
        (2, &[2]),
        (64, &[64]),
        (65, &[65]),
        (255, &[255]),
        (256, &[256]),
        (257, &[255, 2]),
        (300, &[256, 44]),
        (512, &[256, 256]),
        (513, &[256, 255, 2]),
        (769, &[256, 256, 255, 2]),
    ];

    #[test]
    fn merkle_batch_sizes_rebalance_singleton_remainders() {
        for (total, expected) in PARTITION_CASES {
            assert_eq!(
                merkle_batch_sizes(total),
                expected,
                "{total} addresses must partition as {expected:?}"
            );
        }
    }

    /// The test-seam cap partitions like the production cap, floored at 3 —
    /// a cap of 2 cannot partition odd totals into payable ≥2-leaf trees
    /// (ADR-0003).
    #[test]
    fn merkle_batch_sizes_with_cap_partitions_and_clamps() {
        // Small caps: rebalanced, every part payable (2..=cap), sums correct.
        assert_eq!(merkle_batch_sizes_with_cap(6, 3), vec![3, 3]);
        assert_eq!(merkle_batch_sizes_with_cap(7, 3), vec![3, 2, 2]);
        assert_eq!(merkle_batch_sizes_with_cap(4, 3), vec![2, 2]);
        // A cap below the floor clamps to 3 instead of emitting a 1-leaf part.
        assert_eq!(merkle_batch_sizes_with_cap(5, 2), vec![3, 2]);
        // A cap above MAX_LEAVES clamps down to the contract bound.
        assert_eq!(
            merkle_batch_sizes_with_cap(MAX_LEAVES + 1, MAX_LEAVES * 4),
            vec![MAX_LEAVES - 1, 2]
        );
        // Exhaustive soundness under the smallest cap: parts within bounds,
        // no 1-leaf part, exact cover.
        for total in 2..200usize {
            let sizes = merkle_batch_sizes_with_cap(total, 3);
            assert_eq!(sizes.iter().sum::<usize>(), total, "cover for {total}");
            assert!(
                sizes.iter().all(|&s| (2..=3).contains(&s)),
                "unpayable part for {total}: {sizes:?}"
            );
        }
    }

    /// The external multi-batch fold mirrors the wallet path: proofs union,
    /// costs sum, and the merged timestamp is the OLDEST sub-batch's (worst
    /// case for the expiry window).
    #[test]
    fn merge_merkle_batch_results_unions_proofs_and_keeps_oldest_timestamp() {
        let a = MerkleBatchPaymentResult {
            proofs: [([1u8; 32], vec![1u8])].into_iter().collect(),
            chunk_count: 1,
            storage_cost_atto: "100".into(),
            gas_cost_wei: 7,
            merkle_payment_timestamp: 2_000,
        };
        let b = MerkleBatchPaymentResult {
            proofs: [([2u8; 32], vec![2u8]), ([3u8; 32], vec![3u8])]
                .into_iter()
                .collect(),
            chunk_count: 2,
            storage_cost_atto: "50".into(),
            gas_cost_wei: 5,
            merkle_payment_timestamp: 1_500,
        };
        let merged = merge_merkle_batch_results(vec![a, b]);
        assert_eq!(merged.proofs.len(), 3);
        assert_eq!(merged.chunk_count, 3);
        assert_eq!(merged.storage_cost_atto, "150");
        assert_eq!(merged.gas_cost_wei, 12);
        assert_eq!(merged.merkle_payment_timestamp, 1_500);
    }

    /// The defect: `[256, 1]` pays the first batch on-chain and then hands a
    /// single address to a tree that needs two, so the upload fails *after*
    /// spending. Every count must produce trees that can all be built.
    #[test]
    fn merkle_batch_sizes_are_always_buildable_trees() {
        for total in 2..=(4 * MAX_LEAVES + 3) {
            let sizes = merkle_batch_sizes(total);
            assert!(!sizes.is_empty(), "{total} addresses must produce batches");
            assert_eq!(
                sizes.iter().sum::<usize>(),
                total,
                "{total} addresses: partition must cover every address"
            );
            for size in sizes {
                assert!(
                    (2..=MAX_LEAVES).contains(&size),
                    "{total} addresses produced a batch of {size}, outside 2..={MAX_LEAVES}"
                );
            }
        }
    }

    #[test]
    fn merkle_batch_sizes_below_two_have_no_payable_partition() {
        assert!(merkle_batch_sizes(0).is_empty());
        assert!(merkle_batch_sizes(1).is_empty());
    }

    #[test]
    fn merkle_batch_partitions_preserve_order_and_use_each_address_once() {
        for (total, _) in PARTITION_CASES {
            let addrs = make_test_addresses(total);
            let partitions = merkle_batch_partitions(&addrs);

            let flattened: Vec<[u8; 32]> = partitions.concat();
            assert_eq!(
                flattened, addrs,
                "{total} addresses: partitions must concatenate back to the input in order"
            );

            let unique: std::collections::HashSet<[u8; 32]> = flattened.iter().copied().collect();
            assert_eq!(
                unique.len(),
                total,
                "{total} addresses: no address may be duplicated or synthesised"
            );
        }
    }

    /// A merkle upload of 257 chunks — the count the old split could not pay —
    /// is the shape preflight routinely leaves behind, since `to_upload` is
    /// whatever the network did not already hold.
    #[test]
    fn post_preflight_plan_of_257_partitions_into_payable_batches() {
        let plan = MerkleUploadPlan {
            already_stored: make_test_addresses(3),
            to_upload: make_test_addresses(257),
            to_upload_total_bytes: 257 * 1024,
        };
        assert_eq!(plan.to_upload.len(), 257);

        let partitions = merkle_batch_partitions(&plan.to_upload);
        let sizes: Vec<usize> = partitions.iter().map(|batch| batch.len()).collect();
        assert_eq!(sizes, vec![255, 2]);
        for batch in partitions {
            let xornames: Vec<XorName> = batch.iter().map(|a| XorName(*a)).collect();
            assert!(
                MerkleTree::from_xornames(xornames).is_ok(),
                "every partition of a 257-chunk plan must build a tree"
            );
        }
    }

    /// No batch may be paid for and then fail to build its tree: the partition
    /// is what payment iterates, so proving every batch builds proves no
    /// on-chain payment can be followed by a singleton-tree failure.
    #[test]
    fn no_partition_pays_before_a_singleton_tree_failure() {
        for total in [257usize, 513, 769] {
            let addrs = make_test_addresses(total);
            for batch in merkle_batch_partitions(&addrs) {
                let xornames: Vec<XorName> = batch.iter().map(|a| XorName(*a)).collect();
                assert!(
                    MerkleTree::from_xornames(xornames).is_ok(),
                    "{total} addresses: batch of {} is unpayable",
                    batch.len()
                );
            }
        }
    }

    #[test]
    fn merkle_billable_leaves_sum_the_padded_partitions() {
        for (total, expected) in PARTITION_CASES {
            let padded: u64 = expected
                .iter()
                .map(|size| size.next_power_of_two() as u64)
                .sum();
            assert_eq!(
                merkle_billable_leaves(total as u64),
                padded,
                "{total} chunks must bill for the padded partition {expected:?}"
            );
        }

        // Known figures, spelled out: padding is billed, never hidden.
        assert_eq!(merkle_billable_leaves(65), 128);
        assert_eq!(merkle_billable_leaves(257), 256 + 2);
        assert_eq!(merkle_billable_leaves(300), 256 + 64);
        // Nothing to upload costs nothing; a lone chunk still quotes the
        // two-leaf minimum a tree needs.
        assert_eq!(merkle_billable_leaves(0), 0);
        assert_eq!(merkle_billable_leaves(1), 2);
    }

    #[test]
    fn merkle_billable_leaves_never_under_quote() {
        for chunks in 1..2000u64 {
            assert!(
                merkle_billable_leaves(chunks) >= chunks,
                "{chunks} chunks must never be billed as fewer leaves"
            );
        }
    }

    /// The external signer prepares one tree, signs once, and pays once, so an
    /// oversized set is refused up front rather than being quietly paid under a
    /// different model.
    #[test]
    fn external_preparation_refuses_more_than_one_tree_of_addresses() {
        assert!(ensure_single_merkle_tree_batch(2).is_ok());
        assert!(ensure_single_merkle_tree_batch(MAX_LEAVES).is_ok());

        for oversized in [MAX_LEAVES + 1, 300, 513] {
            match ensure_single_merkle_tree_batch(oversized) {
                Err(Error::MerkleBatchTooLarge {
                    addresses,
                    max_leaves,
                }) => {
                    assert_eq!(addresses, oversized);
                    assert_eq!(max_leaves, MAX_LEAVES);
                }
                other => panic!("{oversized} addresses should be refused, got {other:?}"),
            }
        }
    }

    // =========================================================================
    // merkle_store_with_retry: collect-not-abort + bounded retry (C2.1 / C2.2)
    // =========================================================================

    use std::sync::{Arc, Mutex};

    /// Build `count` chunk addresses for the store helper. Bodies are read on
    /// demand by `store_one`, so the tests pass addresses only.
    fn make_addrs(count: usize) -> Vec<[u8; 32]> {
        make_test_addresses(count)
    }

    /// C2.1: a per-chunk `InsufficientPeers` is collected, not propagated —
    /// the whole batch must NOT abort. With a single attempt, the failing
    /// subset is reported via `failed` and the rest are `stored`.
    #[tokio::test]
    async fn store_with_retry_collects_failures_instead_of_aborting() {
        let chunks = make_addrs(6);
        let failing: std::collections::HashSet<[u8; 32]> = chunks.iter().take(2).copied().collect();
        let failing_for_closure = failing.clone();

        let store_one = move |addr: [u8; 32]| {
            let fail = failing_for_closure.contains(&addr);
            async move {
                if fail {
                    Err(Error::InsufficientPeers("test shortfall".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome =
            merkle_store_with_retry(chunks, || 8, 1, Duration::ZERO, None, 0, 6, store_one)
                .await
                .expect("quorum shortfalls must not abort the batch");

        assert_eq!(outcome.stored, 4);
        assert_eq!(outcome.failed, 2);
        // Single attempt → all successes recorded in round 0.
        assert_eq!(outcome.stats.retries_histogram[0], 4);
        assert_eq!(outcome.stats.chunk_attempts_total, 6);
    }

    /// #167 regression, preserved at the engine seam the spill path composes
    /// (`upload_merkle_from_spill` runs one single-attempt pass and then the
    /// deferred rounds): a genuine quorum shortfall — every batch PAID, the
    /// store short — survives all deferred retries with
    /// `stored + failed == total` and the exact shortfall set in
    /// `failed_addresses`, which is precisely what the spill path folds into
    /// `Error::PartialUpload` instead of reporting success (#166).
    #[tokio::test]
    async fn quorum_shortfall_survives_deferred_retries_with_exact_accounting() {
        let chunks = make_addrs(5);
        let short: std::collections::HashSet<[u8; 32]> = chunks.iter().take(2).copied().collect();
        let short_for_closure = short.clone();
        let store_one = move |addr: [u8; 32]| {
            let fail = short_for_closure.contains(&addr);
            async move {
                if fail {
                    Err(Error::InsufficientPeers("still short of quorum".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        // Initial pass exactly as the spill path runs it: one attempt,
        // shortfalls deferred rather than retried inline.
        let pass = merkle_store_with_retry(
            chunks.clone(),
            || 8,
            1,
            Duration::ZERO,
            None,
            0,
            5,
            &store_one,
        )
        .await
        .expect("quorum shortfalls must not abort the pass");
        assert!(pass.fatal.is_none());
        assert_eq!(pass.stored, 3);
        assert_eq!(pass.failed, 2);

        // Deferred rounds (zero delays for the test): the same chunks stay
        // short through every round.
        let dr = merkle_deferred_retry(
            pass.failed_addresses.clone(),
            &[0, 0, 0],
            |n: usize| n.max(1),
            None,
            pass.stored,
            5,
            &store_one,
        )
        .await
        .expect("deferred shortfalls must not abort");

        assert!(dr.fatal.is_none());
        assert_eq!(
            dr.stored + dr.failed,
            5,
            "stored + failed must account for every chunk"
        );
        assert_eq!(dr.stored, 3, "paid-and-stored chunks must stay counted");
        assert_eq!(dr.failed, 2);
        let failed_set: std::collections::HashSet<[u8; 32]> =
            dr.failed_addresses.iter().map(|(a, _)| *a).collect();
        assert_eq!(
            failed_set, short,
            "failed set must be exactly the shortfall chunks"
        );
    }

    /// V2-554: the store scheduler must RE-READ the cap as each slot frees
    /// (rolling), not snapshot it once like `buffer_unordered`. A snapshot would
    /// invoke the cap closure once per attempt; the rolling scheduler invokes it
    /// once per drained slot, so mid-flight limiter growth reaches the rest of
    /// the round. Proven here by counting cap-closure invocations.
    #[tokio::test]
    async fn store_with_retry_rereads_cap_per_slot() {
        let count = 6;
        let chunks = make_addrs(count);
        let cap_calls = Arc::new(Mutex::new(0usize));
        let cap_calls_for_closure = cap_calls.clone();
        let cap = move || {
            *cap_calls_for_closure.lock().expect("cap counter poisoned") += 1;
            2
        };
        let store_one = move |_addr: [u8; 32]| async move { Ok(std::time::Instant::now()) };

        let outcome =
            merkle_store_with_retry(chunks, cap, 1, Duration::ZERO, None, 0, count, store_one)
                .await
                .expect("all stores succeed");

        assert_eq!(outcome.stored, count);
        let calls = *cap_calls.lock().expect("cap counter poisoned");
        assert!(
            calls >= count,
            "cap must be re-read per drained slot (rolling), not snapshotted once — \
             expected >= {count} invocations, got {calls}",
        );
    }

    /// The whole-file store pass is a single cap-bounded fan-out with NO wave
    /// barrier: one slow straggler (a chunk whose peers take a long time) must
    /// not block the rest of the file, and must be driven to completion
    /// concurrently by the fast chunks. If the scheduler serialized (a barrier),
    /// the fast chunks could not run until the straggler returned — a deadlock
    /// the surrounding timeout would catch.
    #[tokio::test]
    async fn store_pass_has_no_barrier() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = 8;
        let addrs = make_addrs(count);
        let slow = addrs[0];
        let fast_completed = Arc::new(AtomicUsize::new(0));
        let release_slow = Arc::new(tokio::sync::Notify::new());

        let store_one = move |addr: [u8; 32]| {
            let fast_completed = fast_completed.clone();
            let release_slow = release_slow.clone();
            async move {
                if addr == slow {
                    // Block until every fast chunk has finished. This can only
                    // resolve if the fast chunks run WHILE this one is parked —
                    // i.e. there is no barrier serializing the store pass.
                    release_slow.notified().await;
                } else if fast_completed.fetch_add(1, Ordering::SeqCst) + 1 == count - 1 {
                    release_slow.notify_one();
                }
                Ok(std::time::Instant::now())
            }
        };

        let outcome = tokio::time::timeout(
            Duration::from_secs(5),
            merkle_store_with_retry(addrs, || 8, 1, Duration::ZERO, None, 0, count, store_one),
        )
        .await
        .expect("store pass must not deadlock — a slow chunk must not block the others")
        .expect("all stores succeed");

        assert_eq!(outcome.stored, count);
    }

    /// Peak memory is bounded by the store cap, not the file size: `store_one`
    /// reads each body on demand, so the scheduler holds at most `cap` stores
    /// (hence at most `cap` bodies) in flight at once — the property that lets
    /// the whole file store as one fan-out without the old 64-chunk waves.
    #[tokio::test]
    async fn store_pass_keeps_at_most_cap_in_flight() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let count = 40;
        let cap = 4;
        let addrs = make_addrs(count);
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight_for_closure = max_in_flight.clone();

        let store_one = move |_addr: [u8; 32]| {
            let in_flight = in_flight.clone();
            let max_in_flight = max_in_flight_for_closure.clone();
            async move {
                let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max_in_flight.fetch_max(now, Ordering::SeqCst);
                // Yield so sibling stores get a chance to start concurrently,
                // maximising the observed in-flight count.
                tokio::task::yield_now().await;
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(std::time::Instant::now())
            }
        };

        let outcome = merkle_store_with_retry(
            addrs,
            move || cap,
            1,
            Duration::ZERO,
            None,
            0,
            count,
            store_one,
        )
        .await
        .expect("all stores succeed");

        assert_eq!(outcome.stored, count);
        let peak = max_in_flight.load(Ordering::SeqCst);
        assert!(
            peak <= cap,
            "at most `cap` bodies may be in flight (memory bound), got peak {peak} > cap {cap}",
        );
        assert!(
            peak > 1,
            "the pass must actually run concurrently, not serialize (peak {peak})",
        );
    }

    /// V2-468: an app-only quorum shortfall surfaces as `Error::RemotePut`
    /// (pool-rejected / quote-stale / disk-full — transient), which must be
    /// treated as recoverable just like `InsufficientPeers`: collected and
    /// retried, never aborting the whole batch.
    #[tokio::test]
    async fn store_with_retry_treats_remote_put_as_recoverable() {
        let chunks = make_addrs(6);
        let failing: std::collections::HashSet<[u8; 32]> = chunks.iter().take(2).copied().collect();
        let failing_for_closure = failing.clone();

        let store_one = move |addr: [u8; 32]| {
            let fail = failing_for_closure.contains(&addr);
            async move {
                if fail {
                    Err(Error::RemotePut {
                        address: hex::encode(addr),
                        source: ant_protocol::ProtocolError::StorageFailed(
                            "insufficient disk space".into(),
                        ),
                    })
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome =
            merkle_store_with_retry(chunks, || 8, 1, Duration::ZERO, None, 0, 6, store_one)
                .await
                .expect("remote app-rejections must not abort the batch");

        assert_eq!(outcome.stored, 4);
        assert_eq!(outcome.failed, 2);
    }

    /// A non-quorum error (e.g. a missing proof) is captured in `fatal` rather
    /// than discarded — the call returns `Ok(outcome)` so the caller can decide
    /// whether to re-raise it or fold it into `PartialUpload`.
    #[tokio::test]
    async fn store_with_retry_reports_non_quorum_errors_as_fatal() {
        let chunks = make_addrs(3);
        let store_one = |_addr: [u8; 32]| async move {
            Err::<std::time::Instant, _>(Error::Payment("missing proof".into()))
        };

        let outcome =
            merkle_store_with_retry(chunks, || 8, 3, Duration::ZERO, None, 0, 3, store_one)
                .await
                .expect("fatal is carried in the outcome, not returned as Err");
        assert!(matches!(outcome.fatal, Some(Error::Payment(_))));
    }

    /// A fatal error mid-pass preserves the successes that already completed in
    /// the same pass — they are not discarded with the abort. Concurrency 1
    /// makes ordering deterministic: the first five chunks store, then the sixth
    /// aborts fatally.
    #[tokio::test]
    async fn store_with_retry_fatal_preserves_same_pass_successes() {
        let chunks = make_addrs(6);
        let bad = chunks[5];
        let store_one = move |addr: [u8; 32]| async move {
            if addr == bad {
                Err(Error::Payment("fatal".into()))
            } else {
                Ok(std::time::Instant::now())
            }
        };

        let outcome =
            merkle_store_with_retry(chunks, || 1, 1, Duration::ZERO, None, 0, 6, store_one)
                .await
                .expect("fatal carried in outcome, not returned as Err");
        assert!(matches!(outcome.fatal, Some(Error::Payment(_))));
        // The five chunks stored before the abort are preserved, not lost.
        assert_eq!(outcome.stored, 5);
        assert_eq!(outcome.stored_addresses.len(), 5);
        assert!(!outcome.stored_addresses.contains(&bad));
        // The fatal chunk is reported as failed (not silently dropped).
        assert!(outcome.failed_addresses.iter().any(|(a, _)| *a == bad));
    }

    /// C2.2: only the chunks that failed the previous round are retried.
    #[tokio::test]
    async fn store_with_retry_retries_only_the_failed_set() {
        let chunks = make_addrs(5);
        let total = chunks.len();
        let failing: std::collections::HashSet<[u8; 32]> = chunks.iter().take(2).copied().collect();
        let failing_for_closure = failing.clone();

        // Record every (addr) the store op was invoked with, in call order.
        let calls = Arc::new(Mutex::new(Vec::<[u8; 32]>::new()));
        let calls_for_closure = calls.clone();

        let store_one = move |addr: [u8; 32]| {
            let calls = calls_for_closure.clone();
            // Fails the first round only; succeeds thereafter.
            let already_seen = calls.lock().unwrap().iter().filter(|&&a| a == addr).count();
            let fail = failing_for_closure.contains(&addr) && already_seen == 0;
            calls.lock().unwrap().push(addr);
            async move {
                if fail {
                    Err(Error::InsufficientPeers("round-1 shortfall".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome =
            merkle_store_with_retry(chunks, || 8, 3, Duration::ZERO, None, 0, total, store_one)
                .await
                .expect("should converge after retry");

        assert_eq!(outcome.stored, total);
        assert_eq!(outcome.failed, 0);

        // Round 1 drains fully before round 2 starts, so the call log is
        // segmented: first `total` calls = round 1 (all chunks), the rest =
        // the retry round, which must contain ONLY the failing set.
        let calls = calls.lock().unwrap();
        assert_eq!(calls.len(), total + failing.len());
        let round_two: std::collections::HashSet<[u8; 32]> =
            calls[total..].iter().copied().collect();
        assert_eq!(round_two, failing);
    }

    /// C2.2: a chunk that fails attempt 1 and succeeds attempt 2 is counted
    /// once as stored and recorded as one retry in `retries_histogram[1]`.
    #[tokio::test]
    async fn store_with_retry_counts_retry_success_once_in_histogram() {
        let chunks = make_addrs(4);
        let total = chunks.len();
        let flaky_addr = chunks[0];

        let attempts = Arc::new(Mutex::new(HashMap::<[u8; 32], usize>::new()));
        let attempts_for_closure = attempts.clone();

        let store_one = move |addr: [u8; 32]| {
            let attempts = attempts_for_closure.clone();
            let n = {
                let mut m = attempts.lock().unwrap();
                let entry = m.entry(addr).or_insert(0);
                *entry += 1;
                *entry
            };
            let fail = addr == flaky_addr && n == 1;
            async move {
                if fail {
                    Err(Error::InsufficientPeers("transient".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome =
            merkle_store_with_retry(chunks, || 8, 3, Duration::ZERO, None, 0, total, store_one)
                .await
                .expect("flaky chunk should recover on retry");

        assert_eq!(outcome.stored, total);
        assert_eq!(outcome.failed, 0);
        // 3 chunks landed on the first attempt, 1 on the first retry.
        assert_eq!(outcome.stats.retries_histogram[0], total - 1);
        assert_eq!(outcome.stats.retries_histogram[1], 1);
        // One extra store attempt for the flaky chunk.
        assert_eq!(outcome.stats.chunk_attempts_total, total + 1);
    }

    /// C2.2: when every chunk stays short of quorum through the whole attempt
    /// budget, the helper still returns `Ok` (collect-not-abort) with the full
    /// batch reported as `failed`, having tried each chunk exactly
    /// `MERKLE_STORE_MAX_ATTEMPTS` times.
    #[tokio::test]
    async fn store_with_retry_reports_all_failed_when_retries_exhausted() {
        let chunks = make_addrs(3);
        let total = chunks.len();

        let store_one = |_addr: [u8; 32]| async move {
            Err::<std::time::Instant, _>(Error::InsufficientPeers("never converges".into()))
        };

        let outcome = merkle_store_with_retry(
            chunks,
            || 8,
            MERKLE_STORE_MAX_ATTEMPTS,
            Duration::ZERO,
            None,
            0,
            total,
            store_one,
        )
        .await
        .expect("an exhausted retry budget is reported, not propagated as Err");

        assert_eq!(outcome.stored, 0);
        assert_eq!(outcome.failed, total);
        // Every chunk was attempted once per round across the full budget.
        assert_eq!(
            outcome.stats.chunk_attempts_total,
            total * MERKLE_STORE_MAX_ATTEMPTS
        );
        // No successes, so the histogram stays empty.
        assert_eq!(outcome.stats.retries_histogram, [0; 4]);
    }

    /// D (CLI path): when retries are exhausted, `failed_addresses` names
    /// exactly the still-short-of-quorum chunks (with their last error message)
    /// and excludes the ones that stored. This is what `upload_merkle_from_spill`
    /// uses to build `PartialUpload`.
    #[tokio::test]
    async fn store_with_retry_records_failed_addresses_when_exhausted() {
        let chunks = make_addrs(6);
        let failing: std::collections::HashSet<[u8; 32]> = chunks.iter().take(2).copied().collect();
        let failing_for_closure = failing.clone();

        let store_one = move |addr: [u8; 32]| {
            let fail = failing_for_closure.contains(&addr);
            async move {
                if fail {
                    Err(Error::InsufficientPeers("permanent shortfall".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome = merkle_store_with_retry(
            chunks,
            || 8,
            MERKLE_STORE_MAX_ATTEMPTS,
            Duration::ZERO,
            None,
            0,
            6,
            store_one,
        )
        .await
        .expect("quorum shortfalls must not abort the batch");

        assert_eq!(outcome.stored, 4);
        assert_eq!(outcome.failed, 2);
        // `failed_addresses` names exactly the failing set, no stored chunks.
        assert_eq!(outcome.failed_addresses.len(), 2);
        let reported: std::collections::HashSet<[u8; 32]> =
            outcome.failed_addresses.iter().map(|(a, _)| *a).collect();
        assert_eq!(reported, failing);
        // Each carries a non-empty error message for the PartialUpload report.
        for (_, msg) in &outcome.failed_addresses {
            assert!(msg.contains("permanent shortfall"));
        }
    }

    /// `failed_addresses` is empty when every chunk reaches quorum (no
    /// `PartialUpload` is raised by the CLI path in that case).
    #[tokio::test]
    async fn store_with_retry_failed_addresses_empty_on_full_success() {
        let chunks = make_addrs(4);
        let total = chunks.len();
        let store_one = |_addr: [u8; 32]| async move { Ok(std::time::Instant::now()) };

        let outcome = merkle_store_with_retry(
            chunks,
            || 8,
            MERKLE_STORE_MAX_ATTEMPTS,
            Duration::ZERO,
            None,
            0,
            total,
            store_one,
        )
        .await
        .expect("all chunks store");

        assert_eq!(outcome.stored, total);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.failed_addresses.is_empty());
    }

    // =========================================================================
    // merkle_deferred_retry: download-style concurrent post-wave retry (V2-466)
    // =========================================================================

    /// The histogram slot mapping: the wave first pass is slot 0; deferred
    /// round `r` is slot `r + 1`, clamped to the last slot.
    #[test]
    fn deferred_round_histogram_slot_maps_and_clamps() {
        assert_eq!(deferred_round_histogram_slot(0, 4), 1);
        assert_eq!(deferred_round_histogram_slot(1, 4), 2);
        assert_eq!(deferred_round_histogram_slot(2, 4), 3);
        // Beyond the histogram width, clamp to the final slot.
        assert_eq!(deferred_round_histogram_slot(3, 4), 3);
        assert_eq!(deferred_round_histogram_slot(9, 4), 3);
    }

    fn deferred_set(count: usize) -> Vec<([u8; 32], String)> {
        make_test_addresses(count)
            .into_iter()
            .map(|addr| (addr, "short of quorum".to_string()))
            .collect()
    }

    /// A chunk that is quorum-short on early rounds but succeeds on a later
    /// round is stored exactly once, recorded in that round's histogram slot,
    /// and reported with no failures.
    #[tokio::test]
    async fn deferred_retry_succeeds_on_a_later_round() {
        let deferred = deferred_set(3);
        // Each chunk fails its first attempt (round 0) and succeeds the second
        // (round 1 → histogram slot 2).
        let attempts = Arc::new(Mutex::new(HashMap::<[u8; 32], usize>::new()));
        let attempts_for_closure = attempts.clone();
        let store_one = move |addr: [u8; 32]| {
            let attempts = attempts_for_closure.clone();
            async move {
                let n = {
                    let mut map = attempts.lock().unwrap();
                    let e = map.entry(addr).or_insert(0);
                    *e += 1;
                    *e
                };
                if n < 2 {
                    Err(Error::InsufficientPeers("still short".into()))
                } else {
                    Ok(std::time::Instant::now())
                }
            }
        };

        let outcome = merkle_deferred_retry(
            deferred,
            &[0, 0, 0],
            |n: usize| n.max(1),
            None,
            0,
            3,
            store_one,
        )
        .await
        .expect("deferred retry must not abort on quorum shortfalls");

        assert_eq!(outcome.stored, 3, "all three land by round 1");
        assert_eq!(outcome.stored_addresses.len(), 3);
        assert_eq!(outcome.failed, 0);
        assert!(outcome.failed_addresses.is_empty());
        assert!(outcome.fatal.is_none());
        // Round 1 → slot 2; round 0 (slot 1) saw zero successes.
        assert_eq!(outcome.stats.retries_histogram[1], 0);
        assert_eq!(outcome.stats.retries_histogram[2], 3);
        // Each chunk attempted twice: one failed round + one success round.
        assert_eq!(outcome.stats.chunk_attempts_total, 6);
    }

    /// Chunks still short of quorum after the final deferred round become
    /// `failed`, not silently dropped, and no fatal error is set.
    #[tokio::test]
    async fn deferred_retry_leftovers_become_failed() {
        let deferred = deferred_set(2);
        let store_one = |_addr: [u8; 32]| async move {
            Err::<std::time::Instant, _>(Error::InsufficientPeers("always short".into()))
        };

        let outcome = merkle_deferred_retry(
            deferred,
            &[0, 0, 0],
            |n: usize| n.max(1),
            None,
            0,
            2,
            store_one,
        )
        .await
        .expect("exhausted retries report failures, not an error");

        assert_eq!(outcome.stored, 0);
        assert!(outcome.stored_addresses.is_empty());
        assert_eq!(outcome.failed, 2);
        assert_eq!(outcome.failed_addresses.len(), 2);
        assert!(outcome.fatal.is_none());
        // Three rounds × two chunks, all failing.
        assert_eq!(outcome.stats.chunk_attempts_total, 6);
    }

    /// A non-quorum (fatal) error during a deferred round stops the pass, is
    /// surfaced via `fatal`, and preserves an earlier round's success in
    /// `stored`/`stored_addresses` while the still-pending chunk is reported as
    /// failed.
    #[tokio::test]
    async fn deferred_retry_fatal_error_preserves_prior_progress() {
        let addrs = make_test_addresses(2);
        let good = addrs[0];
        let bad = addrs[1];
        let deferred = vec![(good, "short".to_string()), (bad, "short".to_string())];

        // `good` succeeds on round 0; `bad` is quorum-short on round 0, then
        // hits a fatal Payment error on round 1.
        let attempts = Arc::new(Mutex::new(HashMap::<[u8; 32], usize>::new()));
        let attempts_for_closure = attempts.clone();
        let store_one = move |addr: [u8; 32]| {
            let attempts = attempts_for_closure.clone();
            async move {
                let n = {
                    let mut map = attempts.lock().unwrap();
                    let e = map.entry(addr).or_insert(0);
                    *e += 1;
                    *e
                };
                if addr == good {
                    Ok(std::time::Instant::now())
                } else if n == 1 {
                    Err(Error::InsufficientPeers("short".into()))
                } else {
                    Err(Error::Payment("fatal on retry".into()))
                }
            }
        };

        let outcome = merkle_deferred_retry(
            deferred,
            &[0, 0, 0],
            |n: usize| n.max(1),
            None,
            0,
            2,
            store_one,
        )
        .await
        .expect("a fatal round error is reported via `fatal`, not as Err");

        assert!(outcome.fatal.is_some(), "fatal error must be captured");
        assert_eq!(outcome.stored, 1, "round-0 success preserved");
        assert_eq!(outcome.stored_addresses, vec![good]);
        assert_eq!(outcome.failed, 1);
        assert_eq!(outcome.failed_addresses.len(), 1);
        assert_eq!(outcome.failed_addresses[0].0, bad);
    }

    /// An empty deferred set is a no-op: no rounds run, nothing stored or failed.
    #[tokio::test]
    async fn deferred_retry_empty_set_is_a_noop() {
        let store_one = |_addr: [u8; 32]| async move {
            Err::<std::time::Instant, _>(Error::InsufficientPeers("unused".into()))
        };

        let outcome = merkle_deferred_retry(
            Vec::new(),
            &DEFERRED_ROUND_DELAYS_SECS,
            |n: usize| n.max(1),
            None,
            7,
            7,
            store_one,
        )
        .await
        .expect("empty deferred set is a no-op");

        assert_eq!(outcome.stored, 7, "stored_offset carried through unchanged");
        assert_eq!(outcome.failed, 0);
        assert!(outcome.stored_addresses.is_empty());
        assert!(outcome.failed_addresses.is_empty());
        assert!(outcome.fatal.is_none());
    }
}
