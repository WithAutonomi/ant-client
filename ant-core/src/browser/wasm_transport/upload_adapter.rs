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
    pub placement: RecordPlacement,
    pub checkpoint: &'a UploadCheckpoint,
    pub scope: &'a str,
    pub last_transaction: RefCell<Option<String>>,
    pub payment_state: RefCell<Option<UploadState>>,
    pub recovering: Cell<bool>,
}

impl BrowserUploadAdapter<'_> {
    async fn invoke(&self, callback: &js_sys::Function, input: JsValue) -> DataResult<JsValue> {
        let state = self
            .payment_state
            .borrow()
            .clone()
            .ok_or_else(|| Error::Payment("payment attempt was not checkpointed".into()))?;
        let journal = Rc::new(RefCell::new(state));
        let pending = Rc::new(RefCell::new(Vec::<Promise>::new()));
        let lock = Rc::new(Mutex::new(()));
        let callback = if self.recovering.get() {
            js_sys::Reflect::get(callback, &JsValue::from_str("recover")).ok()
                .and_then(|value| value.dyn_into::<js_sys::Function>().ok())
                .ok_or_else(|| Error::Payment("payment outcome unknown; provide wallet.recover to observe the existing transaction without paying again".into()))?
        } else {
            callback.clone()
        };
        let checkpoint = self.checkpoint.clone();
        let scope = self.scope.to_owned();
        let on_submission = {
            let journal = Rc::clone(&journal);
            let pending = Rc::clone(&pending);
            let lock = Rc::clone(&lock);
            Closure::<dyn FnMut(JsValue) -> Promise>::new(move |value: JsValue| {
                let journal = Rc::clone(&journal);
                let lock = Rc::clone(&lock);
                let checkpoint = checkpoint.clone();
                let scope = scope.clone();
                let promise = wasm_bindgen_futures::future_to_promise(async move {
                    let _guard = lock.lock().await;
                    let value: serde_json::Value = serde_wasm_bindgen::from_value(value)
                        .map_err(|e| JsValue::from_str(&e.to_string()))?;
                    {
                        let mut state = journal.borrow_mut();
                        let attempt = state
                            .pending_payment
                            .as_mut()
                            .ok_or_else(|| JsValue::from_str("no pending payment"))?;
                        if attempt.submissions.len() >= 256 {
                            return Err(JsValue::from_str("too many payment submissions"));
                        }
                        attempt.submissions.push(value);
                    }
                    let snapshot = journal.borrow().clone();
                    checkpoint
                        .save(&scope, &snapshot)
                        .await
                        .map_err(|e| JsValue::from_str(&e))?;
                    Ok(JsValue::UNDEFINED)
                });
                pending.borrow_mut().push(promise.clone());
                promise
            })
        };
        // JS owns the callback while a cancelled wallet may still finish broadcasting.
        let on_submission = on_submission.into_js_value();
        let network = serde_wasm_bindgen::to_value(self.payment_network)
            .map_err(|e| Error::Payment(e.to_string()))?;
        let attempt = journal
            .borrow()
            .pending_payment
            .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
            .map_err(|e| Error::Payment(e.to_string()))?;
        // Persistence above can yield to close(). No new wallet request may
        // start afterwards, but observation of an existing submission is safe.
        if !self.recovering.get() && self.network.inner.pool.availability.closed.get() {
            journal.borrow_mut().pending_payment = None;
            let snapshot = journal.borrow().clone();
            self.payment_state.replace(Some(snapshot.clone()));
            self.checkpoint
                .save(self.scope, &snapshot)
                .await
                .map_err(Error::Payment)?;
            return Err(Error::Payment(
                "browser client is closed; payment was not submitted".into(),
            ));
        }
        let returned = callback
            .call4(&JsValue::NULL, &network, &input, &on_submission, &attempt)
            .map_err(|e| Error::Payment(js_error_message(e)));
        let result = match returned {
            Ok(value) => JsFuture::from(Promise::resolve(&value))
                .await
                .map_err(|e| Error::Payment(js_error_message(e))),
            Err(error) => Err(error),
        };
        let writes = pending.borrow().clone();
        let mut write_error = None;
        for write in writes {
            if let Err(error) = JsFuture::from(write).await {
                write_error.get_or_insert_with(|| Error::Payment(js_error_message(error)));
            }
        }
        self.payment_state.replace(Some(journal.borrow().clone()));
        if let Some(error) = write_error {
            return Err(error);
        }
        let value = result?;
        let raw: serde_json::Value = serde_wasm_bindgen::from_value(value.clone())
            .map_err(|e| Error::Payment(e.to_string()))?;
        if let Some(attempt) = journal.borrow_mut().pending_payment.as_mut() {
            attempt.receipt = Some(raw);
        }
        let snapshot = journal.borrow().clone();
        self.payment_state.replace(Some(snapshot.clone()));
        self.checkpoint
            .save(self.scope, &snapshot)
            .await
            .map_err(Error::Payment)?;
        Ok(value)
    }

    /// Report a record position within the whole file rather than this batch.
    fn report_record(&self, label: &str, position: usize, total: usize) {
        let position = self.placement.position(position);
        let total = self.placement.total(total);
        self.progress.report(&format!("{label} {position}/{total}"));
    }

    fn update_journal(&self, state: &mut UploadState) {
        if let Some(journal) = self.payment_state.take() {
            *state = journal;
        }
    }
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
        let input =
            serde_wasm_bindgen::to_value(&verified).map_err(|e| Error::Payment(e.to_string()))?;
        let value = self.invoke(self.wallet, input).await?;
        let payment: BrowserPaymentSubmission =
            serde_wasm_bindgen::from_value(value).map_err(|e| Error::Payment(e.to_string()))?;
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
        let value =
            serde_wasm_bindgen::to_value(&request).map_err(|e| Error::Payment(e.to_string()))?;
        let returned = self.invoke(callback, value).await?;
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
    async fn submit_payment(
        &self,
        plans: &[ChunkPaymentPlan],
        state: &mut UploadState,
    ) -> DataResult<UploadPayment> {
        self.payment_state.replace(Some(state.clone()));
        self.recovering.set(false);
        let result = self.pay(plans).await;
        self.update_journal(state);
        result
    }
    async fn reconcile_payment(
        &self,
        plans: &[ChunkPaymentPlan],
        state: &mut UploadState,
    ) -> DataResult<UploadPayment> {
        self.payment_state.replace(Some(state.clone()));
        self.recovering.set(true);
        let result = self.pay(plans).await;
        self.update_journal(state);
        result
    }
    async fn submit_merkle_payment(
        &self,
        batch: &crate::data::client::merkle::PreparedMerkleBatch,
        state: &mut UploadState,
    ) -> DataResult<crate::data::client::upload::MerkleUploadPayment> {
        self.payment_state.replace(Some(state.clone()));
        self.recovering.set(false);
        let result = self.pay_merkle(batch).await;
        self.update_journal(state);
        result
    }
    async fn reconcile_merkle_payment(
        &self,
        batch: &crate::data::client::merkle::PreparedMerkleBatch,
        state: &mut UploadState,
    ) -> DataResult<crate::data::client::upload::MerkleUploadPayment> {
        self.payment_state.replace(Some(state.clone()));
        self.recovering.set(true);
        let result = self.pay_merkle(batch).await;
        self.update_journal(state);
        result
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
        self.report_record("Confirmed available record", stored, total);
    }
    fn quoted(&self, quoted: usize, total: usize) {
        self.report_record("Quoted record", quoted, total);
    }
    fn record_stored(&self, index: usize, total: usize) {
        self.report_record("Stored new record", index, total);
    }
    fn checked(&self, checked: usize, total: usize) {
        self.progress
            .report(&format!("Checked existing storage {checked}/{total}"));
    }
    fn already_stored(&self, index: usize, total: usize) {
        self.report_record("Already present record", index, total);
    }
    fn payment_quotes(&self, completed: usize, total: usize) {
        self.progress.report(&format!(
            "Collecting payment quote pools {completed}/{total}"
        ));
    }
    fn preparing(&self, message: &str) {
        self.progress.report(message);
    }
}
