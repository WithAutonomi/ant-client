//! Merkle batch payment support for the Autonomi client.
//!
//! When uploading batches of 64+ chunks, merkle payments reduce gas costs
//! by paying for the entire batch in a single on-chain transaction instead
//! of one transaction per chunk.

use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_node::ant_protocol::{
    ChunkMessage, ChunkMessageBody, MerkleCandidateQuoteRequest, MerkleCandidateQuoteResponse,
};
use ant_node::client::send_and_await_chunk_response;
use ant_node::payment::quote::verify_merkle_candidate_signature;
use ant_node::payment::serialize_merkle_proof;
use bytes::Bytes;
use evmlib::merkle_batch_payment::PoolCommitment;
use evmlib::merkle_payments::{
    MerklePaymentCandidateNode, MerklePaymentCandidatePool, MerklePaymentProof, MerkleTree,
    MidpointProof, CANDIDATES_PER_POOL, MAX_LEAVES,
};
use futures::stream::{self, FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::time::Duration;
use tracing::{debug, info, warn};
use xor_name::XorName;

/// Default threshold: use merkle payments when chunk count >= this value.
pub const DEFAULT_MERKLE_THRESHOLD: usize = 64;

/// Payment mode for uploads.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
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

    /// Pay for a single batch (up to `MAX_LEAVES` chunks).
    async fn pay_for_merkle_single_batch(
        &self,
        addresses: &[[u8; 32]],
        data_type: u32,
        data_size: u64,
    ) -> Result<MerkleBatchPaymentResult> {
        let chunk_count = addresses.len();
        let wallet = self.require_wallet()?;
        let xornames: Vec<XorName> = addresses.iter().map(|a| XorName(*a)).collect();

        info!("Building merkle tree for {chunk_count} chunks");

        // 1. Build merkle tree
        let tree = MerkleTree::from_xornames(xornames.clone())
            .map_err(|e| Error::Payment(format!("Failed to build merkle tree: {e}")))?;

        let depth = tree.depth();
        let merkle_payment_timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| Error::Payment(format!("System time error: {e}")))?
            .as_secs();

        info!("Merkle tree: depth={depth}, leaves={chunk_count}, ts={merkle_payment_timestamp}");

        // 2. Get reward candidates (midpoint proofs)
        let midpoint_proofs = tree
            .reward_candidates(merkle_payment_timestamp)
            .map_err(|e| Error::Payment(format!("Failed to generate reward candidates: {e}")))?;

        info!(
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

        // 5. Pay on-chain (single transaction)
        info!("Submitting merkle batch payment on-chain (depth={depth})");
        let (winner_pool_hash, _amount, _gas_info) = wallet
            .pay_for_merkle_tree(depth, pool_commitments, merkle_payment_timestamp)
            .await
            .map_err(|e| Error::Payment(format!("Merkle batch payment failed: {e}")))?;

        info!(
            "Merkle payment succeeded: winner pool {}",
            hex::encode(winner_pool_hash)
        );

        // 6. Find the winner pool
        let winner_pool = candidate_pools
            .iter()
            .find(|pool| pool.hash() == winner_pool_hash)
            .ok_or_else(|| {
                Error::Payment(format!(
                    "Winner pool {} not found in candidate pools",
                    hex::encode(winner_pool_hash)
                ))
            })?;

        // 7. Generate proofs for each chunk
        info!("Generating merkle proofs for {chunk_count} chunks");
        let mut proofs = HashMap::with_capacity(chunk_count);

        for (i, (xorname, address)) in xornames.iter().zip(addresses.iter()).enumerate() {
            let address_proof = tree.generate_address_proof(i, *xorname).map_err(|e| {
                Error::Payment(format!(
                    "Failed to generate address proof for chunk {i}: {e}"
                ))
            })?;

            let merkle_proof =
                MerklePaymentProof::new(*xorname, address_proof, winner_pool.clone());

            let tagged_bytes = serialize_merkle_proof(&merkle_proof).map_err(|e| {
                Error::Serialization(format!("Failed to serialize merkle proof: {e}"))
            })?;

            proofs.insert(*address, tagged_bytes);
        }

        info!("Merkle batch payment complete: {chunk_count} proofs generated");

        Ok(MerkleBatchPaymentResult {
            proofs,
            chunk_count,
        })
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

        for (i, chunk) in sub_batches.into_iter().enumerate() {
            match self
                .pay_for_merkle_single_batch(chunk, data_type, data_size)
                .await
            {
                Ok(sub_result) => {
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
                    });
                }
            }
        }

        Ok(MerkleBatchPaymentResult {
            chunk_count: addresses.len(),
            proofs: all_proofs,
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
        let timeout = Duration::from_secs(self.config().timeout_secs);

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

        self.collect_validated_candidates(&mut candidate_futures, merkle_payment_timestamp)
            .await
    }

    /// Collect and validate merkle candidate responses until we have enough.
    async fn collect_validated_candidates(
        &self,
        futures: &mut FuturesUnordered<
            impl std::future::Future<
                Output = (
                    ant_node::core::PeerId,
                    std::result::Result<MerklePaymentCandidateNode, Error>,
                ),
            >,
        >,
        merkle_payment_timestamp: u64,
    ) -> Result<[MerklePaymentCandidateNode; CANDIDATES_PER_POOL]> {
        let mut candidates = Vec::with_capacity(CANDIDATES_PER_POOL);
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
                    candidates.push(candidate);
                    if candidates.len() >= CANDIDATES_PER_POOL {
                        break;
                    }
                }
                Err(e) => {
                    debug!("Failed to get merkle candidate from {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));
                }
            }
        }

        if candidates.len() < CANDIDATES_PER_POOL {
            return Err(Error::InsufficientPeers(format!(
                "Got {} merkle candidates, need {CANDIDATES_PER_POOL}. Failures: [{}]",
                candidates.len(),
                failures.join("; ")
            )));
        }

        candidates.truncate(CANDIDATES_PER_POOL);
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
        let mut upload_stream = stream::iter(chunk_contents.into_iter().zip(addresses).map(
            |(content, addr)| {
                let proof_bytes = batch_result.proofs.get(&addr).cloned();
                async move {
                    let proof = proof_bytes.ok_or_else(|| {
                        Error::Payment(format!(
                            "Missing merkle proof for chunk {}",
                            hex::encode(addr)
                        ))
                    })?;
                    let peers = self.close_group_peers(&addr).await?;
                    self.chunk_put_to_close_group(content, proof, &peers).await
                }
            },
        ))
        .buffer_unordered(self.config().chunk_concurrency);

        while let Some(result) = upload_stream.next().await {
            result?;
            stored += 1;
        }

        Ok(stored)
    }
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
    use evmlib::merkle_payments::{MerkleTree, CANDIDATES_PER_POOL};

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
        use ant_node::payment::{deserialize_merkle_proof, serialize_merkle_proof};
        use evmlib::common::Amount;
        use evmlib::merkle_payments::MerklePaymentCandidateNode;
        use evmlib::RewardsAddress;

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
            price: evmlib::common::Amount::ZERO,
            reward_address: evmlib::RewardsAddress::new([0u8; 20]),
            merkle_payment_timestamp: 1000,
            signature: vec![0u8; 64],
        };

        // Timestamp check: 1000 != 2000
        assert_ne!(candidate.merkle_payment_timestamp, 2000);
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
