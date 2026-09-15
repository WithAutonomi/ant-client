// Copyright 2026 Saorsa Labs Limited
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Native wallet journal: persist a signed payment before any storage broadcast.
use super::{
    batch::ChunkPaymentPlan,
    merkle::PreparedMerkleBatch,
    upload::{MerkleUploadPayment, UploadAdapter, UploadPayment},
    upload_state::{PaymentAttempt, UploadState},
    Client,
};
use crate::data::error::{Error, Result};
use ant_protocol::evm::journal::{PaymentReceipt, PaymentRequest, PaymentStatus, SignedPayment};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(
    tag = "native_payment_v1",
    content = "transaction",
    rename_all = "snake_case"
)]
enum Journal {
    Preparing,
    Signed(SignedPayment),
}

pub(super) fn initialize(attempt: &mut PaymentAttempt) {
    // This synchronous marker is persisted with the attempt, before wallet I/O.
    attempt.submissions = vec![serde_json::json!({ "native_payment_v1": "preparing" })];
}

pub(super) async fn pay<A: UploadAdapter>(
    client: &Client,
    adapter: &A,
    plans: &[ChunkPaymentPlan],
    state: &mut UploadState,
) -> Result<UploadPayment> {
    let quotes = plans
        .iter()
        .flat_map(|p| p.payment.quotes.iter())
        .filter(|q| q.amount != ant_protocol::evm::Amount::ZERO)
        .map(|q| (q.quote_hash, q.rewards_address, q.amount))
        .collect::<Vec<_>>();
    let hashes = quotes.iter().map(|q| q.0).collect::<Vec<_>>();
    let fresh = |state: &UploadState| {
        plans.iter().all(|p| {
            state
                .retained_plan(&p.address, p.data_size, crate::runtime::system_time())
                .is_some()
        })
    };
    let receipt = execute(
        client,
        adapter,
        &PaymentRequest::Quotes(quotes),
        state,
        fresh,
    )
    .await?;
    Ok(UploadPayment {
        transactions: hashes
            .into_iter()
            .map(|h| (h, receipt.transaction_hash))
            .collect(),
        amount: receipt.amount,
        gas: receipt.gas_cost_wei,
    })
}

pub(super) async fn pay_merkle<A: UploadAdapter>(
    client: &Client,
    adapter: &A,
    batch: &PreparedMerkleBatch,
    state: &mut UploadState,
) -> Result<MerkleUploadPayment> {
    let request = PaymentRequest::Merkle {
        depth: batch.depth,
        pools: batch.pool_commitments.clone(),
        timestamp: batch.merkle_payment_timestamp,
    };
    let fresh = |_: &UploadState| {
        super::upload_state::merkle_fresh(
            batch.merkle_payment_timestamp,
            crate::runtime::system_time(),
        )
    };
    let receipt = execute(client, adapter, &request, state, fresh).await?;
    Ok(MerkleUploadPayment {
        winner_pool: receipt
            .winner_pool
            .ok_or_else(|| Error::Payment("missing confirmed Merkle winner".into()))?,
        amount: receipt.amount,
        gas: receipt.gas_cost_wei,
    })
}

async fn execute<A: UploadAdapter, F: Fn(&UploadState) -> bool + Send + Sync>(
    client: &Client,
    adapter: &A,
    request: &PaymentRequest,
    state: &mut UploadState,
    fresh: F,
) -> Result<PaymentReceipt> {
    let attempt = state
        .pending_payment
        .as_ref()
        .ok_or_else(|| Error::Payment("missing native payment attempt".into()))?;
    let journal: Journal =
        match attempt.submissions.as_slice() {
            [value] => serde_json::from_value(value.clone()).map_err(|_| {
                Error::Payment(
                    "unknown native payment journal; reconciliation evidence is required".into(),
                )
            })?,
            _ => return Err(Error::Payment(
                "legacy pending payment has no transaction identity; reconcile it before retrying"
                    .into(),
            )),
        };
    let wallet = client.require_wallet()?;
    let _wallet_lock = wallet.lock().await;
    let signed = match journal {
        Journal::Signed(signed) => signed,
        Journal::Preparing => {
            // This marker proves no storage payment was broadcast by this implementation.
            let prepared = if let Some(refusal) = client.corroborated_settlement_refusal() {
                Err(Error::ClientUpdateRequired(refusal))
            } else if !fresh(state) {
                Err(Error::Payment(
                    "unsubmitted payment quotes expired; retry to prepare fresh quotes".into(),
                ))
            } else {
                wallet
                    .prepare_payment(request)
                    .await
                    .map_err(Error::Payment)
            };
            let signed = match prepared {
                Ok(signed) => signed,
                Err(error) => {
                    state.pending_payment = None;
                    adapter.checkpoint(state, None).await?;
                    return Err(error);
                }
            };
            let value = serde_json::to_value(Journal::Signed(signed.clone()))
                .map_err(|e| Error::Serialization(e.to_string()))?;
            if let Some(attempt) = &mut state.pending_payment {
                attempt.submissions = vec![value];
            }
            adapter.checkpoint(state, None).await?;
            signed
        }
    };
    let mut broadcast = false;
    let observation = async {
        loop {
            match wallet
                .observe_payment(&signed, request)
                .await
                .map_err(Error::Payment)?
            {
                PaymentStatus::Confirmed(receipt) => {
                    if let Some(attempt) = &mut state.pending_payment {
                        attempt.receipt = Some(
                            serde_json::to_value(&receipt)
                                .map_err(|e| Error::Serialization(e.to_string()))?,
                        );
                    }
                    adapter.checkpoint(state, None).await?;
                    return Ok(receipt);
                }
                PaymentStatus::Reverted => {
                    state.pending_payment = None;
                    adapter.checkpoint(state, None).await?;
                    return Err(Error::Payment(
                        "payment reverted on-chain; retry is safe".into(),
                    ));
                }
                PaymentStatus::Pending => {
                    if !broadcast {
                        if let Some(refusal) = client.corroborated_settlement_refusal() {
                            return Err(Error::ClientUpdateRequired(refusal));
                        }
                        if !fresh(state) {
                            return Err(Error::Payment("pending payment has expired quotes; retain its transaction journal for reconciliation".into()));
                        }
                        // An ambiguous send only ever retries these exact signed bytes.
                        wallet
                            .broadcast_payment(&signed, request)
                            .await
                            .map_err(Error::Payment)?;
                        broadcast = true;
                    }
                    crate::runtime::sleep(std::time::Duration::from_secs(1)).await;
                }
            }
        }
    };
    crate::runtime::timeout(std::time::Duration::from_secs(30), observation)
        .await
        .map_err(|_| {
            Error::Payment(
                "payment outcome pending; retry will reconcile the journaled transaction".into(),
            )
        })?
}

#[cfg(feature = "test-utils")]
impl Client {
    /// Exercise the native payment journal with an injected persistence adapter.
    /// Available only to integration tests built with `test-utils`.
    pub async fn test_execute_native_payment<A: UploadAdapter>(
        &self,
        adapter: &A,
        request: &PaymentRequest,
        state: &mut UploadState,
        fresh: bool,
    ) -> Result<PaymentReceipt> {
        if state.pending_payment.is_none() {
            state.start_payment(false, Vec::new())?;
            initialize(state.pending_payment.as_mut().expect("just initialized"));
            adapter.checkpoint(state, None).await?;
        }
        execute(self, adapter, request, state, |_| fresh).await
    }
}
