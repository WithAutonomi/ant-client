// Copyright 2026 MaidSafe.net limited.
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser byte source, endpoint admission, wallet and checkpoint adapters.
use super::*;
use crate::data::client::batch::ChunkPaymentPlan;
use crate::data::client::upload::{UploadAdapter, UploadPayment, UploadRecord as Record};
use crate::data::client::upload_state::UploadState;
use crate::data::error::{Error, Result as DataResult};
use ant_protocol::evm::{Amount, QuoteHash, TxHash};

pub(super) struct BrowserUploadAdapter<'a> {
    pub network: &'a BrowserNetworkClient,
    pub records: &'a [UploadRecord],
    pub payment_network: &'a BrowserPaymentNetwork,
    pub loader: Option<&'a js_sys::Function>,
    pub merkle_wallet: Option<&'a js_sys::Function>,
    pub wallet: &'a js_sys::Function,
    pub progress: &'a ProgressReporter,
    pub checkpoint: &'a UploadCheckpoint,
    pub scope: &'a str,
    pub last_transaction: RefCell<Option<String>>,
}

#[async_trait::async_trait(?Send)]
impl UploadAdapter for BrowserUploadAdapter<'_> {
    async fn load(&self, record: Record) -> DataResult<bytes::Bytes> {
        let metadata = self
            .records
            .get(record.index)
            .ok_or_else(|| Error::InvalidData("missing staged record".into()))?;
        let bytes = load_upload_record(record.index, metadata, self.loader)
            .await
            .map_err(Error::InvalidData)?;
        Ok(bytes::Bytes::copy_from_slice(bytes.as_slice()))
    }
    async fn admit(&self, plan: &mut ChunkPaymentPlan) -> DataResult<()> {
        let eligible = join_all(
            plan.quoted_peers
                .iter()
                .map(|(peer, addresses)| async move {
                    let endpoint = addresses.iter().find(|a| a.is_webrtc_direct())?;
                    let endpoint = BrowserEndpoint {
                        multiaddr: endpoint.to_string(),
                    };
                    let node = self.network.inner.pool.client(&endpoint).await.ok()?;
                    let hello = node.hello().await.ok()?;
                    assert_upload_node(&hello, self.payment_network).ok()?;
                    if !hello.capabilities.iter().any(|cap| cap == "chunk_protocol") {
                        return None;
                    }
                    Some((*peer, addresses.clone()))
                }),
        )
        .await
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        let required = crate::quote_policy::witness_quorum(0);
        if eligible.len() < required {
            return Err(Error::InsufficientPeers(format!(
                "Fewer than {required} eligible initial witness PUT peers: got {}",
                eligible.len()
            )));
        }
        plan.quoted_peers = eligible;
        Ok(())
    }
    async fn pay(&self, plans: &[ChunkPaymentPlan]) -> DataResult<UploadPayment> {
        let verified = plans
            .iter()
            .map(|plan| {
                let payable = plan
                    .payment
                    .quotes
                    .iter()
                    .find(|q| !q.amount.is_zero())
                    .ok_or_else(|| Error::Payment("payment plan has no paid quote".into()))?;
                let (_, quote) = plan
                    .peer_quotes
                    .iter()
                    .find(|(_, q)| q.hash() == payable.quote_hash)
                    .ok_or_else(|| Error::Payment("paid quote missing from plan".into()))?;
                Ok(VerifiedStorageQuote {
                    quote: native_quote_artifact(quote, &plan.commitment_sidecars)
                        .map_err(Error::Payment)?,
                    quote_hash: hex::encode(payable.quote_hash),
                    rewards_address: format!("0x{}", hex::encode(payable.rewards_address)),
                    amount: payable.amount.to_string(),
                })
            })
            .collect::<DataResult<Vec<_>>>()?;
        let payment = invoke_payment(self.wallet, self.payment_network, &verified)
            .await
            .map_err(Error::Payment)?;
        let mut transactions = HashMap::new();
        for quote in &verified {
            let hash = if payment.transaction_hashes.is_empty() {
                payment.transaction_hash.as_ref()
            } else {
                payment
                    .transaction_hashes
                    .get(&quote.quote_hash)
                    .or_else(|| {
                        payment
                            .transaction_hashes
                            .get(&format!("0x{}", quote.quote_hash))
                    })
            }
            .ok_or_else(|| {
                Error::Payment("wallet returned no transaction for a paid quote".into())
            })?;
            transactions.insert(
                QuoteHash::from(
                    parse_lookup_key(&quote.quote_hash, "quote hash").map_err(Error::Payment)?,
                ),
                TxHash::from(parse_lookup_key(hash, "transaction hash").map_err(Error::Payment)?),
            );
        }
        *self.last_transaction.borrow_mut() = payment.transaction_hash;
        Ok(UploadPayment {
            transactions,
            amount: payment
                .total_amount
                .parse::<Amount>()
                .map_err(|e| Error::Payment(e.to_string()))?,
            gas: 0,
        })
    }
    async fn pay_merkle(
        &self,
        batch: &crate::data::client::merkle::PreparedMerkleBatch,
    ) -> DataResult<crate::data::client::upload::MerkleUploadPayment> {
        let callback = self.merkle_wallet.ok_or_else(|| Error::Payment("wallet adapter does not support Merkle payments; provide payMerkle or select single mode".into()))?;
        let request = super::super::payment::MerklePaymentRequest::from_batch(batch);
        let network = serde_wasm_bindgen::to_value(self.payment_network)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let value = serde_wasm_bindgen::to_value(&request)
            .map_err(|e| Error::Serialization(e.to_string()))?;
        let returned = callback
            .call2(&JsValue::NULL, &network, &value)
            .map_err(|e| Error::Payment(js_error_message(e)))?;
        let returned = JsFuture::from(Promise::resolve(&returned))
            .await
            .map_err(|e| Error::Payment(js_error_message(e)))?;
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Receipt {
            transaction_hash: String,
            winner_pool_hash: String,
            total_amount: String,
        }
        let receipt: Receipt =
            serde_wasm_bindgen::from_value(returned).map_err(|e| Error::Payment(e.to_string()))?;
        let winner_pool =
            parse_lookup_key(&receipt.winner_pool_hash, "winner pool").map_err(Error::Payment)?;
        let amount = receipt
            .total_amount
            .parse::<Amount>()
            .map_err(|e| Error::Payment(e.to_string()))?;
        if !batch
            .pool_commitments
            .iter()
            .any(|pool| pool.pool_hash == winner_pool)
            || amount
                > request
                    .maximum_amount
                    .parse::<Amount>()
                    .map_err(|e| Error::Payment(e.to_string()))?
        {
            return Err(Error::Payment(
                "Merkle receipt does not match prepared payment".into(),
            ));
        }
        *self.last_transaction.borrow_mut() = Some(
            super::super::protocol::normalize_hex(&receipt.transaction_hash, 32)
                .map_err(Error::Payment)?,
        );
        Ok(crate::data::client::upload::MerkleUploadPayment {
            winner_pool,
            amount,
            gas: 0,
        })
    }
    async fn checkpoint(&self, state: &UploadState, _: Option<&UploadPayment>) -> DataResult<()> {
        self.checkpoint
            .save(self.scope, state)
            .await
            .map_err(Error::Payment)
    }
    fn quote_limit(&self) -> usize {
        DEFAULT_BROWSER_QUOTE_CONCURRENCY
    }
    fn stored(&self, stored: usize, total: usize) {
        self.progress
            .report(&format!("Stored record {stored}/{total}"));
    }
    fn quoted(&self, quoted: usize, total: usize) {
        self.progress
            .report(&format!("Preparing record {quoted}/{total}"));
    }
}
