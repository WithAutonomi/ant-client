//! Payment orchestration for the Autonomi client.
//!
//! Connects quote collection, on-chain EVM payment, and proof serialization.
//! Every PUT to the network requires a valid payment proof.

use crate::data::client::quote::{select_paid_quote_for_payment, StoreQuote};
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::evm::{Amount, EncodedPeerId, PaymentQuote, ProofOfPayment, Wallet};
use ant_protocol::payment::{serialize_single_node_proof, PaymentProof, QuotePaymentInfo};
use ant_protocol::transport::{MultiAddr, PeerId};
use std::sync::Arc;
use tracing::{debug, info};

/// Single-node payment amount multiplier required by storer verification.
const PAID_QUOTE_PAYMENT_MULTIPLIER: u64 = 3;

pub(crate) fn paid_quote_payment_info(quote: &PaymentQuote) -> Result<QuotePaymentInfo> {
    if quote.price.is_zero() {
        return Err(Error::Payment(
            "Paid quote has zero price; refusing to build an unpaid storage proof".to_string(),
        ));
    }

    let amount = quote
        .price
        .checked_mul(Amount::from(PAID_QUOTE_PAYMENT_MULTIPLIER))
        .ok_or_else(|| {
            Error::Payment(format!(
                "Price overflow when calculating {PAID_QUOTE_PAYMENT_MULTIPLIER}x paid quote"
            ))
        })?;

    Ok(QuotePaymentInfo {
        quote_hash: quote.hash(),
        rewards_address: quote.rewards_address,
        amount,
        price: quote.price,
    })
}

pub(crate) fn paid_quote_payment_from_store_quotes(
    quotes: &[StoreQuote],
) -> Result<(PeerId, PaymentQuote, QuotePaymentInfo)> {
    let (peer_id, _, quote, _) = select_paid_quote_for_payment(quotes).ok_or_else(|| {
        Error::Payment("No successful quote available for single-node payment".to_string())
    })?;
    let payment_info = paid_quote_payment_info(quote)?;
    Ok((*peer_id, quote.clone(), payment_info))
}

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
    /// 1. Query `CLOSE_GROUP_SIZE` witnessed peers and collect enough quotes
    ///    to pick one that should satisfy the witnessed-quorum price floors
    /// 2. Select one paid quote and pay 3x its node-reported price
    /// 3. Pay on-chain via the wallet
    /// 4. Serialize `PaymentProof` with transaction hashes
    ///
    /// # Errors
    ///
    /// Returns an error if the wallet is not set, quotes cannot be collected,
    /// on-chain payment fails, or serialization fails.
    /// Returns `(proof_bytes, quoted_peers)`. `quoted_peers` are the
    /// `CLOSE_GROUP_SIZE` witnessed PUT targets — callers should store the
    /// chunk to at least `CLOSE_GROUP_MAJORITY` of these peers.
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
        let quote_plan = self
            .get_store_quote_plan(address, data_size, data_type)
            .await?;
        let quotes_with_peers = quote_plan.quotes;
        let (paid_peer_id, paid_quote, paid_quote_info) =
            paid_quote_payment_from_store_quotes(&quotes_with_peers)?;

        // Capture all quoted peers for replication by the caller.
        let quoted_peers = quote_plan.put_peers;

        let peer_quotes = vec![(peer_id_to_encoded(&paid_peer_id)?, paid_quote)];

        info!(
            "Selected SNP paid quote issuer {} for address {} (price: {}, amount: {})",
            paid_peer_id,
            hex::encode(address),
            paid_quote_info.price,
            paid_quote_info.amount
        );

        // 4. Pay on-chain
        let payments = vec![(
            paid_quote_info.quote_hash,
            paid_quote_info.rewards_address,
            paid_quote_info.amount,
        )];
        let (tx_hash_map, _gas_info) = wallet.pay_for_quotes(payments).await.map_err(
            |ant_protocol::evm::PayForQuotesError(err, _)| {
                Error::Payment(format!("On-chain payment failed: {err}"))
            },
        )?;
        let tx_hash = tx_hash_map
            .get(&paid_quote_info.quote_hash)
            .copied()
            .ok_or_else(|| {
                Error::Payment(format!(
                    "Missing transaction hash for paid quote {}",
                    paid_quote_info.quote_hash
                ))
            })?;
        let tx_hashes = vec![tx_hash];

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

/// Convert an ant-node `PeerId` to an `EncodedPeerId` for payment proofs.
pub(crate) fn peer_id_to_encoded(peer_id: &PeerId) -> Result<EncodedPeerId> {
    Ok(EncodedPeerId::new(*peer_id.as_bytes()))
}
