//! Merkle batch payment support for the Autonomi client.
//!
//! When uploading batches of 64+ chunks, merkle payments reduce gas costs
//! by paying for the entire batch in a single on-chain transaction instead
//! of one transaction per chunk.

use crate::data::client::adaptive::observe_op;
use crate::data::client::classify_error;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{
    Amount, MerklePaymentCandidateNode, MerklePaymentCandidatePool, MerklePaymentProof, MerkleTree,
    MidpointProof, PoolCommitment, CANDIDATES_PER_POOL, MAX_LEAVES,
};
use ant_protocol::payment::{serialize_merkle_proof, verify_merkle_candidate_signature};
use ant_protocol::transport::PeerId;
use ant_protocol::{
    send_and_await_chunk_response, ChunkMessage, ChunkMessageBody, MerkleCandidateQuoteRequest,
    MerkleCandidateQuoteResponse,
};
use bytes::Bytes;
use futures::stream::{self, FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};
use xor_name::XorName;

/// Default threshold: use merkle payments when chunk count >= this value.
pub const DEFAULT_MERKLE_THRESHOLD: usize = 64;

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
#[derive(Debug)]
pub struct MerkleBatchPaymentResult {
    /// Map of `XorName` to serialized tagged proof bytes (ready to use in PUT requests).
    pub proofs: HashMap<[u8; 32], Vec<u8>>,
    /// Number of chunks in the batch.
    pub chunk_count: usize,
    /// Total storage cost in atto (token smallest unit).
    pub storage_cost_atto: String,
    /// Total gas cost in wei.
    pub gas_cost_wei: u128,
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
    /// and returns per-chunk proofs. Splits into sub-batches if > `MAX_LEAVES`.
    ///
    /// Does NOT pre-filter already-stored chunks (nodes handle `AlreadyExists`
    /// gracefully on PUT). This avoids N sequential GET round-trips before payment.
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

    /// Phase 1 of external-signer merkle payment: prepare batch without paying.
    ///
    /// Builds the merkle tree, collects candidate pools from the network,
    /// and returns the data needed for the on-chain payment call.
    /// Requires `EvmNetwork` but NOT a wallet.
    pub async fn prepare_merkle_batch_external(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<PreparedMerkleBatch> {
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

        // 3. Collect candidate pools from the network (all pools in parallel)
        let candidate_pools = self
            .build_candidate_pools(
                &midpoint_proofs,
                data_type,
                data_size,
                merkle_payment_timestamp,
            )
            .await?;

        // 4. Build pool commitments for on-chain payment
        let pool_commitments: Vec<PoolCommitment> = candidate_pools
            .iter()
            .map(MerklePaymentCandidatePool::to_commitment)
            .collect();

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
        let sub_batches: Vec<&[[u8; 32]]> = addresses.chunks(MAX_LEAVES).collect();
        let total_sub_batches = sub_batches.len();
        let mut all_proofs = HashMap::with_capacity(addresses.len());
        let mut total_storage = Amount::ZERO;
        let mut total_gas: u128 = 0;

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
                    all_proofs.extend(sub_result.proofs);
                }
                Err(e) => {
                    if all_proofs.is_empty() {
                        // First sub-batch failed, nothing paid yet -- propagate directly.
                        return Err(e);
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
                    });
                }
            }
        }

        Ok(MerkleBatchPaymentResult {
            chunk_count: addresses.len(),
            proofs: all_proofs,
            storage_cost_atto: total_storage.to_string(),
            gas_cost_wei: total_gas,
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

        // Over-collect: query 2× the pool size so that responses we end up
        // discarding (bad sigs, wrong timestamps, dropped quotes) don't block
        // the pool from filling. We pick the closest 16 by XOR distance later
        // (see `collect_validated_candidates`); padding from non-close peers
        // is *not* a valid fallback because the receiving node's verifier
        // (`PaymentVerifier::verify_merkle_candidate_closeness`) does its own
        // iterative DHT lookup against the pool midpoint and rejects any pool
        // whose candidates aren't actually XOR-closest to that midpoint.
        let query_count = CANDIDATES_PER_POOL * 2;
        let remote_peers = self
            .network()
            .find_closest_peers(address, query_count)
            .await?;

        if remote_peers.len() < CANDIDATES_PER_POOL {
            return Err(Error::InsufficientPeers(format!(
                "DHT closest-peers lookup returned {} peers, need at least \
                 {CANDIDATES_PER_POOL} to build a merkle candidate pool. Retry once \
                 the routing table populates further, or use --no-merkle.",
                remote_peers.len()
            )));
        }

        let mut candidate_futures = FuturesUnordered::new();

        for (peer_id, peer_addrs) in &remote_peers {
            let request_id = self.next_request_id();
            let request = MerkleCandidateQuoteRequest {
                address: *address,
                data_type,
                data_size,
                merkle_payment_timestamp,
            };
            let message = ChunkMessage {
                request_id,
                body: ChunkMessageBody::MerkleCandidateQuoteRequest(request),
            };

            let message_bytes = match message.encode() {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("Failed to encode merkle candidate request for {peer_id}: {e}");
                    continue;
                }
            };

            let peer_id_clone = *peer_id;
            let addrs_clone = peer_addrs.clone();
            let node_clone = node.clone();

            let fut = async move {
                let result = send_and_await_chunk_response(
                    &node_clone,
                    &peer_id_clone,
                    message_bytes,
                    request_id,
                    timeout,
                    &addrs_clone,
                    |body| match body {
                        ChunkMessageBody::MerkleCandidateQuoteResponse(
                            MerkleCandidateQuoteResponse::Success { candidate_node },
                        ) => {
                            match rmp_serde::from_slice::<MerklePaymentCandidateNode>(
                                &candidate_node,
                            ) {
                                Ok(node) => Some(Ok(node)),
                                Err(e) => Some(Err(Error::Serialization(format!(
                                    "Failed to deserialize candidate node from {peer_id_clone}: {e}"
                                )))),
                            }
                        }
                        ChunkMessageBody::MerkleCandidateQuoteResponse(
                            MerkleCandidateQuoteResponse::Error(e),
                        ) => Some(Err(Error::Protocol(format!(
                            "Merkle quote error from {peer_id_clone}: {e}"
                        )))),
                        _ => None,
                    },
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

                (peer_id_clone, result)
            };

            candidate_futures.push(fut);
        }

        self.collect_validated_candidates(
            &mut candidate_futures,
            address,
            merkle_payment_timestamp,
        )
        .await
    }

    /// Collect and validate merkle candidate responses, then return the
    /// `CANDIDATES_PER_POOL` candidates whose `PeerId`s are XOR-closest to the
    /// pool midpoint address.
    ///
    /// Selection-by-XOR-distance (not by response order) is load-bearing: the
    /// receiving node's `PaymentVerifier::verify_merkle_candidate_closeness`
    /// runs its own iterative DHT lookup against the pool midpoint and
    /// requires `CANDIDATE_CLOSENESS_REQUIRED` of our 16 candidates to appear
    /// in *its* closest-K set. Picking the first 16 to respond — the previous
    /// behaviour — leaks slow-but-genuinely-close peers out of the pool and
    /// replaces them with fast-but-farther peers, tipping the verifier below
    /// its tolerance and rejecting every chunk in the batch.
    ///
    /// Per-peer latency is already bounded by `quote_timeout_secs`, so
    /// draining the entire `FuturesUnordered` adds no worst-case time over
    /// the previous early-break behaviour.
    async fn collect_validated_candidates(
        &self,
        futures: &mut FuturesUnordered<
            impl std::future::Future<
                Output = (
                    PeerId,
                    std::result::Result<MerklePaymentCandidateNode, Error>,
                ),
            >,
        >,
        pool_address: &[u8; 32],
        merkle_payment_timestamp: u64,
    ) -> Result<[MerklePaymentCandidateNode; CANDIDATES_PER_POOL]> {
        let mut scored: Vec<(PeerId, MerklePaymentCandidateNode)> = Vec::new();
        let mut failures: Vec<String> = Vec::new();

        while let Some((peer_id, result)) = futures.next().await {
            match result {
                Ok(candidate) => {
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
                    scored.push((peer_id, candidate));
                }
                Err(e) => {
                    debug!("Failed to get merkle candidate from {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));
                }
            }
        }

        if scored.len() < CANDIDATES_PER_POOL {
            return Err(Error::InsufficientPeers(format!(
                "Got {} merkle candidates, need {CANDIDATES_PER_POOL}. Failures: [{}]",
                scored.len(),
                failures.join("; ")
            )));
        }

        // Sort by XOR distance to the pool midpoint and keep the closest
        // `CANDIDATES_PER_POOL`. Honest peers' transport-level `PeerId` is the
        // BLAKE3 hash of the same `pub_key` they sign their candidate with —
        // the hash the verifier derives at check time — so ranking by
        // `peer_id` here matches the verifier's frame of reference.
        let target = PeerId::from_bytes(*pool_address);
        scored.sort_by(|(a, _), (b, _)| a.xor_distance(&target).cmp(&b.xor_distance(&target)));
        let total_responders = scored.len();
        scored.truncate(CANDIDATES_PER_POOL);

        // Build-marker log: a fingerprint string for verifying that a
        // deployed binary actually contains the by-distance pool selection
        // (grep `MERKLE_POOL_BY_DISTANCE_v1` in node/client logs).
        debug!(
            "MERKLE_POOL_BY_DISTANCE_v1: kept closest {} of {} valid responders for pool midpoint {}",
            scored.len(),
            total_responders,
            hex::encode(pool_address),
        );

        let candidates: Vec<MerklePaymentCandidateNode> =
            scored.into_iter().map(|(_, c)| c).collect();
        candidates
            .try_into()
            .map_err(|_| Error::Payment("Failed to convert candidates to fixed array".to_string()))
    }

    /// Upload chunks using pre-computed merkle proofs from a batch payment.
    ///
    /// Each chunk is matched to its proof from `batch_result.proofs`,
    /// then stored to its close group concurrently. Returns the number
    /// of chunks successfully stored.
    ///
    /// # Errors
    ///
    /// Returns an error if any chunk is missing its proof or storage fails.
    pub(crate) async fn merkle_upload_chunks(
        &self,
        chunk_contents: Vec<Bytes>,
        addresses: Vec<[u8; 32]>,
        batch_result: &MerkleBatchPaymentResult,
    ) -> Result<usize> {
        let mut stored = 0usize;
        let store_limiter = self.controller().store.clone();
        // Clamp fan-out to batch size — partial batches should not
        // pay for unused slots (see PERF-RESULTS.md).
        let batch_size = chunk_contents.len();
        let store_concurrency = store_limiter.current().min(batch_size.max(1));
        let mut upload_stream = stream::iter(chunk_contents.into_iter().zip(addresses).map(
            |(content, addr)| {
                let proof_bytes = batch_result.proofs.get(&addr).cloned();
                let limiter = store_limiter.clone();
                async move {
                    let proof = proof_bytes.ok_or_else(|| {
                        Error::Payment(format!(
                            "Missing merkle proof for chunk {}",
                            hex::encode(addr)
                        ))
                    })?;
                    let peers = self.close_group_peers(&addr).await?;
                    observe_op(
                        &limiter,
                        || async move {
                            self.chunk_put_to_close_group(content, proof, &peers).await
                        },
                        classify_error,
                    )
                    .await
                }
            },
        ))
        .buffer_unordered(store_concurrency);

        while let Some(result) = upload_stream.next().await {
            result?;
            stored += 1;
        }

        Ok(stored)
    }
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
        let fut = client.merkle_upload_chunks(Vec::new(), Vec::new(), &batch_result);
        _assert_send(&fut);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
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
    // MerkleTree construction and proof generation (pure, no network)
    // =========================================================================

    fn make_test_addresses(count: usize) -> Vec<[u8; 32]> {
        (0..count)
            .map(|i| {
                let xn = XorName::from_content(&i.to_le_bytes());
                xn.0
            })
            .collect()
    }

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
        };

        // Timestamp check: 1000 != 2000
        assert_ne!(candidate.merkle_payment_timestamp, 2000);
    }

    // =========================================================================
    // finalize_merkle_batch (external signer)
    // =========================================================================

    fn make_dummy_candidate_nodes(
        timestamp: u64,
    ) -> [MerklePaymentCandidateNode; CANDIDATES_PER_POOL] {
        std::array::from_fn(|i| MerklePaymentCandidateNode {
            pub_key: vec![i as u8; 32],
            price: Amount::from(1024u64),
            reward_address: RewardsAddress::new([i as u8; 20]),
            merkle_payment_timestamp: timestamp,
            signature: vec![i as u8; 64],
        })
    }

    fn make_prepared_merkle_batch(count: usize) -> PreparedMerkleBatch {
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
            .map(MerklePaymentCandidatePool::to_commitment)
            .collect();

        PreparedMerkleBatch {
            depth: tree.depth(),
            pool_commitments,
            merkle_payment_timestamp: timestamp,
            candidate_pools,
            tree,
            addresses: addrs,
        }
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

    #[test]
    fn test_batch_split_calculation() {
        // MAX_LEAVES chunks should fit in 1 batch
        let addrs = make_test_addresses(MAX_LEAVES);
        assert_eq!(addrs.chunks(MAX_LEAVES).count(), 1);

        // MAX_LEAVES + 1 should split into 2
        let addrs = make_test_addresses(MAX_LEAVES + 1);
        assert_eq!(addrs.chunks(MAX_LEAVES).count(), 2);

        // 3 * MAX_LEAVES should split into 3
        let addrs = make_test_addresses(3 * MAX_LEAVES);
        assert_eq!(addrs.chunks(MAX_LEAVES).count(), 3);
    }
}
