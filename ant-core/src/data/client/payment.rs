//! Payment orchestration for the saorsa client.
//!
//! Connects quote collection, on-chain EVM payment, and proof serialization.
//! Every PUT to the network requires a valid payment proof.

use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_evm::{EncodedPeerId, ProofOfPayment};
use evmlib::wallet::Wallet;
use saorsa_node::client::hex_node_id_to_encoded_peer_id;
use saorsa_node::core::PeerId;
use saorsa_node::payment::{serialize_single_node_proof, PaymentProof, SingleNodePayment};
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
    /// 1. Collect 5 quotes from closest peers
    /// 2. Build `SingleNodePayment` (median 3x, others 0)
    /// 3. Pay on-chain via the wallet
    /// 4. Serialize `PaymentProof` with transaction hashes
    ///
    /// # Errors
    ///
    /// Returns an error if the wallet is not set, quotes cannot be collected,
    /// on-chain payment fails, or serialization fails.
    /// Returns `(proof_bytes, target_peer)`. The `target_peer` is the closest
    /// peer from quote collection — callers **must** send the PUT to this peer
    /// to avoid a mismatch between the paid quotes and the storage target.
    pub async fn pay_for_storage(
        &self,
        address: &[u8; 32],
        data_size: u64,
        data_type: u32,
    ) -> Result<(Vec<u8>, PeerId)> {
        let wallet = self.require_wallet()?;

        debug!("Collecting quotes for address {}", hex::encode(address));

        // 1. Collect quotes from network
        let quotes_with_peers = self.get_store_quotes(address, data_size, data_type).await?;

        // Pin the closest peer from the quote set as the PUT target.
        // This peer was among the quoted set so the proof includes it.
        let target_peer = quotes_with_peers
            .first()
            .map(|(peer_id, _, _)| *peer_id)
            .ok_or_else(|| Error::InsufficientPeers("no quotes collected".to_string()))?;

        // 2. Fetch prices from the on-chain contract rather than using the
        // locally-computed estimates. The contract's getQuote() is the authoritative
        // price source — the verifyPayment() call recomputes prices from QuotingMetrics
        // using the same formula, so we must use matching prices.
        let metrics_batch: Vec<_> = quotes_with_peers
            .iter()
            .map(|(_, quote, _)| quote.quoting_metrics.clone())
            .collect();

        let contract_prices =
            evmlib::contract::payment_vault::get_market_price(wallet.network(), metrics_batch)
                .await
                .map_err(|e| {
                    Error::Payment(format!("Failed to get market prices from contract: {e}"))
                })?;

        if contract_prices.len() != quotes_with_peers.len() {
            return Err(Error::Payment(format!(
                "Contract returned {} prices for {} quotes",
                contract_prices.len(),
                quotes_with_peers.len()
            )));
        }

        // 3. Build peer_quotes for ProofOfPayment + quotes for SingleNodePayment
        let mut peer_quotes = Vec::with_capacity(quotes_with_peers.len());
        let mut quotes_for_payment = Vec::with_capacity(quotes_with_peers.len());

        for ((peer_id, quote, _local_price), contract_price) in
            quotes_with_peers.into_iter().zip(contract_prices)
        {
            let encoded = peer_id_to_encoded(&peer_id)?;
            peer_quotes.push((encoded, quote.clone()));
            quotes_for_payment.push((quote, contract_price));
        }

        // 4. Create SingleNodePayment (sorts by price, selects median)
        let payment = SingleNodePayment::from_quotes(quotes_for_payment)
            .map_err(|e| Error::Payment(format!("Failed to create payment: {e}")))?;

        info!("Payment total: {} atto", payment.total_amount());

        // 5. Pay on-chain
        let tx_hashes = payment
            .pay(wallet)
            .await
            .map_err(|e| Error::Payment(format!("On-chain payment failed: {e}")))?;

        info!(
            "On-chain payment succeeded: {} transactions",
            tx_hashes.len()
        );

        // 6. Build and serialize proof with version tag
        let proof = PaymentProof {
            proof_of_payment: ProofOfPayment { peer_quotes },
            tx_hashes,
        };

        let proof_bytes = serialize_single_node_proof(&proof)
            .map_err(|e| Error::Serialization(format!("Failed to serialize payment proof: {e}")))?;

        Ok((proof_bytes, target_peer))
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
        let network = wallet.network();

        // Approve data payments contract
        let data_payments_address = network.data_payments_address();
        wallet
            .approve_to_spend_tokens(*data_payments_address, evmlib::common::U256::MAX)
            .await
            .map_err(|e| Error::Payment(format!("Token approval failed: {e}")))?;
        info!("Token spend approved for data payments contract");

        // Approve merkle payments contract (if deployed)
        if let Some(merkle_address) = network.merkle_payments_address() {
            wallet
                .approve_to_spend_tokens(*merkle_address, evmlib::common::U256::MAX)
                .await
                .map_err(|e| Error::Payment(format!("Merkle token approval failed: {e}")))?;
            info!("Token spend approved for merkle payments contract");
        }

        Ok(())
    }
}

/// Convert a saorsa-core `PeerId` to an `EncodedPeerId` for payment proofs.
fn peer_id_to_encoded(peer_id: &PeerId) -> Result<EncodedPeerId> {
    hex_node_id_to_encoded_peer_id(&peer_id.to_hex())
        .map_err(|e| Error::Payment(format!("Failed to encode peer ID: {e}")))
}
