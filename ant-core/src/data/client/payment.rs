//! Payment orchestration for the Autonomi client.
//!
//! Connects quote collection, on-chain EVM payment, and proof serialization.
//! Every PUT to the network requires a valid payment proof.

use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{EncodedPeerId, ProofOfPayment, Wallet};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{serialize_single_node_proof, PaymentProof, SingleNodePayment};
use std::sync::Arc;
use tracing::{debug, info};

impl Client {
    /// Get the wallet, returning an error if not configured.
    pub(crate) fn require_wallet(&self) -> Result<&Arc<Wallet>> {
        self.wallet().ok_or_else(|| {
            Error::Payment("Wallet not configured — call with_wallet() first".to_string())
        })
    }

    /// Pay for storage and return the serialized payment proof bytes.
    ///
    /// This orchestrates the full payment flow:
    /// 1. Collect `CLOSE_GROUP_SIZE` quotes from closest peers
    /// 2. Build `SingleNodePayment` using node-reported prices (median 3x, others 0)
    /// 3. Pay on-chain via the wallet
    /// 4. Serialize `PaymentProof` with transaction hashes
    ///
    /// # Errors
    ///
    /// Returns an error if the wallet is not set, quotes cannot be collected,
    /// on-chain payment fails, or serialization fails.
    /// Returns `(proof_bytes, quoted_peers)`. `quoted_peers` are the
    /// `CLOSE_GROUP_SIZE` peers that provided quotes — callers should store
    /// the chunk to at least `CLOSE_GROUP_MAJORITY` of these peers.
    pub async fn pay_for_storage(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<(Vec<u8>, Vec<(PeerId, Vec<MultiAddr>)>)> {
        // Wallet is required for the on-chain payment step (step 4 below).
        // Check early so we don't waste time collecting quotes for a misconfigured client.
        let wallet = self.require_wallet()?;

        debug!("Collecting quotes for address {}", hex::encode(address));

        // 1. Collect quotes from network
        let quotes_with_peers = self.get_store_quotes(address, data_size, data_type).await?;

        // Capture all quoted peers for replication by the caller.
        let quoted_peers: Vec<(PeerId, Vec<MultiAddr>)> = quotes_with_peers
            .iter()
            .map(|(peer_id, addrs, _, _)| (*peer_id, addrs.clone()))
            .collect();

        // 2. Build peer_quotes for ProofOfPayment + quotes for SingleNodePayment.
        // Use node-reported prices directly — no contract price fetch needed.
        let mut peer_quotes = Vec::with_capacity(quotes_with_peers.len());
        let mut quotes_for_payment = Vec::with_capacity(quotes_with_peers.len());

        for (peer_id, _addrs, quote, price) in quotes_with_peers {
            let encoded = peer_id_to_encoded(&peer_id)?;
            peer_quotes.push((encoded, quote.clone()));
            quotes_for_payment.push((quote, price));
        }

        // 3. Create SingleNodePayment (sorts by price, selects median)
        let payment = SingleNodePayment::from_quotes(quotes_for_payment)
            .map_err(|e| Error::Payment(format!("Failed to create payment: {e}")))?;

        info!("Payment total: {} atto", payment.total_amount());

        // 4. Pay on-chain
        let tx_hashes = payment
            .pay(wallet)
            .await
            .map_err(|e| Error::Payment(format!("On-chain payment failed: {e}")))?;

        info!(
            "On-chain payment succeeded: {} transactions",
            tx_hashes.len()
        );

        // 5. Build and serialize proof with version tag
        let proof = PaymentProof {
            proof_of_payment: ProofOfPayment { peer_quotes },
            tx_hashes,
        };

        let proof_bytes = serialize_single_node_proof(&proof)
            .map_err(|e| Error::Serialization(format!("Failed to serialize payment proof: {e}")))?;

        Ok((proof_bytes, quoted_peers))
    }

    /// Approve the wallet to spend tokens on the payment vault contract.
    ///
    /// This must be called once before any payments can be made.
    /// Approves `U256::MAX` (unlimited) spending.
    ///
    /// # Errors
    ///
    /// Returns an error if the wallet is not set or the approval transaction fails.
    pub async fn approve_token_spend(&self) -> Result<()> {
        let wallet = self.require_wallet()?;
        let evm_network = self.require_evm_network()?;

        let vault_address = evm_network.payment_vault_address();
        wallet
            .approve_to_spend_tokens(*vault_address, ant_protocol::evm::U256::MAX)
            .await
            .map_err(|e| Error::Payment(format!("Token approval failed: {e}")))?;
        info!("Token spend approved for payment vault contract");

        Ok(())
    }
}

/// Convert a saorsa-core `PeerId` (re-exported via `ant_protocol::transport`)
/// to an `EncodedPeerId` suitable for payment proofs.
pub(crate) fn peer_id_to_encoded(peer_id: &PeerId) -> Result<EncodedPeerId> {
    Ok(EncodedPeerId::new(*peer_id.as_bytes()))
}
