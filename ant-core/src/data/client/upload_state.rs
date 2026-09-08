// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Portable prepared/paid upload state. Storage and wallet adapters own persistence and I/O.

use super::batch::{
    build_plan_proof, proof_is_safely_fresh, ChunkPaymentPlan, PaidChunk, PreparedChunk,
};
use crate::data::error::{Error, Result};
use ant_protocol::{
    evm::{QuoteHash, TxHash},
    payment::deserialize_proof,
    XorName,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    time::{Duration, SystemTime},
};

/// Prepared plans and confirmed proofs keyed by content address.
/// Checkpoints contain no file bytes or wallet secrets.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct UploadState {
    plans: HashMap<XorName, ChunkPaymentPlan>,
    proofs: HashMap<XorName, Vec<u8>>,
}

/// Native proof expiry and future-clock tolerance, shared by every client adapter.
pub fn reusable_proof(address: &XorName, bytes: &[u8], now: SystemTime) -> bool {
    let Ok((proof, _)) = deserialize_proof(bytes) else {
        return false;
    };
    !proof.peer_quotes.is_empty()
        && proof
            .peer_quotes
            .iter()
            .all(|(_, quote)| quote.content.0 == *address)
        && proof_is_safely_fresh(
            &proof,
            now,
            Duration::from_secs(
                super::batch::CACHED_PROOF_MAX_AGE_SECS
                    - super::batch::CACHED_PROOF_SAFETY_MARGIN_SECS,
            ),
            Duration::from_secs(super::batch::CACHED_PROOF_FUTURE_SKEW_TOLERANCE_SECS),
        )
}

impl UploadState {
    /// Confirm a native prepared chunk through the same state transition as browser checkpoints.
    pub(super) fn pay_prepared(
        chunk: PreparedChunk,
        transactions: &HashMap<QuoteHash, TxHash>,
    ) -> Result<PaidChunk> {
        let mut state = Self::default();
        state.prepare(ChunkPaymentPlan {
            address: chunk.address,
            data_size: chunk.content.len() as u64,
            quoted_peers: chunk.quoted_peers.clone(),
            payment: chunk.payment,
            peer_quotes: chunk.peer_quotes,
            commitment_sidecars: chunk.commitment_sidecars,
        });
        state.confirm(
            &[chunk.address],
            transactions,
            crate::runtime::system_time(),
        )?;
        let proof_bytes = state
            .proofs
            .remove(&chunk.address)
            .ok_or_else(|| Error::Payment("confirmed proof missing".into()))?;
        Ok(PaidChunk {
            content: chunk.content,
            address: chunk.address,
            quoted_peers: chunk.quoted_peers,
            proof_bytes,
        })
    }

    /// Import the same per-content proofs used by native disk receipts.
    pub fn from_proofs(proofs: HashMap<XorName, Vec<u8>>) -> Self {
        Self {
            plans: HashMap::new(),
            proofs,
        }
    }

    /// Reuse a previously prepared plan after a wallet callback was interrupted.
    /// Paid records are re-discovered so current storage targets replace stale ones.
    pub fn retained_plan(
        &self,
        address: &XorName,
        size: u64,
        now: SystemTime,
    ) -> Option<ChunkPaymentPlan> {
        if self.proofs.contains_key(address) {
            return None;
        }
        let plan = self.plans.get(address)?;
        if plan.data_size != size || plan.peer_quotes.is_empty() {
            return None;
        }
        let proof = ant_protocol::evm::ProofOfPayment {
            peer_quotes: plan.peer_quotes.clone(),
        };
        proof_is_safely_fresh(
            &proof,
            now,
            Duration::from_secs(
                super::batch::CACHED_PROOF_MAX_AGE_SECS
                    - super::batch::CACHED_PROOF_SAFETY_MARGIN_SECS,
            ),
            Duration::from_secs(super::batch::CACHED_PROOF_FUTURE_SKEW_TOLERANCE_SECS),
        )
        .then(|| plan.clone())
    }

    /// Retain a verified plan before invoking an external wallet.
    pub fn prepare(&mut self, plan: ChunkPaymentPlan) {
        self.plans.insert(plan.address, plan);
    }

    /// Whether a confirmed, safely fresh proof already covers this record.
    pub fn is_paid(&self, address: &XorName, now: SystemTime) -> bool {
        self.proofs
            .get(address)
            .is_some_and(|bytes| reusable_proof(address, bytes, now))
    }

    /// Bind a confirmed payment to all pending plans, before loading or storing bytes.
    /// Missing transactions fail atomically, preserving the prepared plans for recovery.
    pub fn confirm(
        &mut self,
        addresses: &[XorName],
        transactions: &HashMap<QuoteHash, TxHash>,
        now: SystemTime,
    ) -> Result<()> {
        let proofs = addresses
            .iter()
            .filter(|address| !self.is_paid(address, now))
            .map(|address| {
                let plan = self
                    .plans
                    .get(address)
                    .ok_or_else(|| Error::Payment("missing prepared payment plan".into()))?;
                build_plan_proof(plan, transactions).map(|proof| (*address, proof))
            })
            .collect::<Result<Vec<_>>>()?;
        self.proofs.extend(proofs);
        for address in addresses {
            self.plans.remove(address);
        }
        Ok(())
    }

    /// Attach an existing proof to refreshed native or browser PUT targets.
    pub fn reuse_prepared(&self, prepared: &PreparedChunk, now: SystemTime) -> Option<PaidChunk> {
        let proof_bytes = self.proofs.get(&prepared.address)?;
        if !reusable_proof(&prepared.address, proof_bytes, now) {
            return None;
        }
        Some(PaidChunk {
            content: prepared.content.clone(),
            address: prepared.address,
            quoted_peers: prepared.quoted_peers.clone(),
            proof_bytes: proof_bytes.clone(),
        })
    }

    /// Serialize a local recovery checkpoint. Treat it like a native payment receipt.
    pub fn checkpoint(&self) -> Result<Vec<u8>> {
        rmp_serde::to_vec_named(self).map_err(|e| Error::Serialization(e.to_string()))
    }

    /// Restore a local checkpoint, validating payment amounts and signed quote identities.
    pub fn restore(bytes: &[u8]) -> Result<Self> {
        let state: Self =
            rmp_serde::from_slice(bytes).map_err(|e| Error::Serialization(e.to_string()))?;
        for (address, plan) in &state.plans {
            if address != &plan.address
                || plan.peer_quotes.is_empty()
                || plan.peer_quotes.iter().any(|(peer, quote)| {
                    quote.content.0 != *address
                        || *peer
                            != ant_protocol::evm::EncodedPeerId::new(
                                *blake3::hash(&quote.pub_key).as_bytes(),
                            )
                        || !ant_protocol::payment::verify_quote_signature(quote)
                })
            {
                return Err(Error::InvalidData("invalid checkpoint quote".into()));
            }
            let canonical = super::batch::SingleNodeQuotePayment::from_quotes(
                plan.peer_quotes
                    .iter()
                    .map(|(_, quote)| quote.clone())
                    .collect(),
            )?;
            let signature = |quotes: &[ant_protocol::payment::QuotePaymentInfo]| {
                quotes
                    .iter()
                    .map(|q| (q.quote_hash, q.rewards_address, q.amount, q.price))
                    .collect::<Vec<_>>()
            };
            if signature(&canonical.quotes) != signature(&plan.payment.quotes) {
                return Err(Error::InvalidData(
                    "checkpoint payment differs from signed quotes".into(),
                ));
            }
        }
        for (address, bytes) in &state.proofs {
            let proof = ant_protocol::payment::proof::deserialize_single_node_proof(bytes)
                .map_err(Error::InvalidData)?;
            if proof.tx_hashes.is_empty()
                || proof.proof_of_payment.peer_quotes.is_empty()
                || proof
                    .proof_of_payment
                    .peer_quotes
                    .iter()
                    .any(|(peer, quote)| {
                        quote.content.0 != *address
                            || *peer
                                != ant_protocol::evm::EncodedPeerId::new(
                                    *blake3::hash(&quote.pub_key).as_bytes(),
                                )
                            || !ant_protocol::payment::verify_quote_signature(quote)
                    })
            {
                return Err(Error::InvalidData(
                    "invalid checkpoint payment proof".into(),
                ));
            }
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::super::batch::{finalize_batch_payment, SingleNodeQuotePayment};
    use super::*;
    use ant_protocol::{
        evm::{Amount, EncodedPeerId, PaymentQuote, RewardsAddress},
        transport::NodeIdentity,
    };
    use bytes::Bytes;

    fn plan(content: &[u8], timestamp: SystemTime) -> ChunkPaymentPlan {
        let identity = NodeIdentity::generate().unwrap();
        let mut quote = PaymentQuote {
            content: xor_name::XorName(ant_protocol::compute_address(content)),
            timestamp,
            price: Amount::from(10),
            rewards_address: RewardsAddress::new([1; 20]),
            pub_key: identity.public_key().as_bytes().to_vec(),
            signature: Vec::new(),
            committed_key_count: 0,
            commitment_pin: None,
        };
        quote.signature = identity
            .sign(&quote.bytes_for_sig())
            .unwrap()
            .as_bytes()
            .to_vec();
        ChunkPaymentPlan {
            address: quote.content.0,
            data_size: content.len() as u64,
            quoted_peers: Vec::new(),
            payment: SingleNodeQuotePayment::from_quotes(vec![quote.clone()]).unwrap(),
            peer_quotes: vec![(
                EncodedPeerId::new(*blake3::hash(&quote.pub_key).as_bytes()),
                quote,
            )],
            commitment_sidecars: Vec::new(),
        }
    }

    #[test]
    fn checkpoint_reuses_original_proof_with_new_quotes_and_matches_native_finalization() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let content = Bytes::from_static(b"retained paid record");
        let original = plan(&content, now);
        let txs = HashMap::from([(original.payment.quotes[0].quote_hash, TxHash::from([7; 32]))]);
        let mut state = UploadState::default();
        state.prepare(original.clone());
        state = UploadState::restore(&state.checkpoint().unwrap()).unwrap();
        assert_eq!(
            state
                .retained_plan(&original.address, content.len() as u64, now)
                .unwrap()
                .payment
                .quotes[0]
                .quote_hash,
            original.payment.quotes[0].quote_hash
        );
        state.confirm(&[original.address], &txs, now).unwrap();
        state = UploadState::restore(&state.checkpoint().unwrap()).unwrap();
        let refreshed = plan(&content, now + Duration::from_secs(1));
        assert_ne!(
            refreshed.payment.quotes[0].quote_hash,
            original.payment.quotes[0].quote_hash
        );
        let recovered = state
            .reuse_prepared(&refreshed.with_content(content.clone()).unwrap(), now)
            .unwrap();
        let native =
            finalize_batch_payment(vec![original.with_content(content).unwrap()], &txs).unwrap();
        assert_eq!(recovered.proof_bytes, native[0].proof_bytes);
    }

    #[test]
    fn confirmation_is_atomic_and_supports_distinct_transactions() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let a = plan(b"first", now);
        let b = plan(b"second", now);
        let mut state = UploadState::default();
        state.prepare(a.clone());
        state.prepare(b.clone());
        let mut txs = HashMap::from([(a.payment.quotes[0].quote_hash, TxHash::from([1; 32]))]);
        assert!(state.confirm(&[a.address, b.address], &txs, now).is_err());
        assert!(!state.is_paid(&a.address, now));
        txs.insert(b.payment.quotes[0].quote_hash, TxHash::from([2; 32]));
        state.confirm(&[a.address, b.address], &txs, now).unwrap();
        assert!(state.is_paid(&a.address, now));
        assert!(state.is_paid(&b.address, now));
        assert!(!state.is_paid(&a.address, now + Duration::from_secs(24 * 60 * 60 - 299)));
        assert!(!reusable_proof(&b.address, &state.proofs[&a.address], now));
    }

    #[test]
    fn checkpoint_rejects_modified_payment_amount() {
        let mut state = UploadState::default();
        let mut a = plan(b"first", SystemTime::UNIX_EPOCH);
        a.payment.quotes[0].amount += Amount::from(1);
        state.prepare(a);
        assert!(UploadState::restore(&state.checkpoint().unwrap()).is_err());
    }
}
