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

#[cfg(test)]
mod tests {
    use super::*;
    use ant_protocol::evm::{testnet::Testnet, Amount, Wallet};
    use ant_protocol::transport::{CoreNodeConfig, P2PNode};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Checkpoints {
        last: Mutex<Option<UploadState>>,
        stop_after_signed: bool,
    }
    #[async_trait::async_trait]
    impl UploadAdapter for Checkpoints {
        async fn load(&self, _: super::super::upload::UploadRecord) -> Result<bytes::Bytes> {
            Err(Error::Storage("test does not load bytes".into()))
        }
        async fn pay(&self, _: &[ChunkPaymentPlan]) -> Result<UploadPayment> {
            Err(Error::Payment("test uses journaled payment".into()))
        }
        async fn checkpoint(&self, state: &UploadState, _: Option<&UploadPayment>) -> Result<()> {
            *self.last.lock().unwrap() = Some(state.clone());
            if self.stop_after_signed
                && state.pending_payment.as_ref().is_some_and(|p| {
                    p.submissions
                        .first()
                        .is_some_and(|v| v["native_payment_v1"] == "signed")
                })
            {
                return Err(Error::Storage(
                    "simulated interruption after durable signing".into(),
                ));
            }
            Ok(())
        }
    }
    fn attempt() -> UploadState {
        let mut state = UploadState::default();
        state.start_payment(false, vec![]).unwrap();
        initialize(state.pending_payment.as_mut().unwrap());
        state
    }
    async fn client(wallet: Wallet) -> Client {
        let config = CoreNodeConfig::builder()
            .port(0)
            .local(true)
            .build()
            .unwrap();
        let node = P2PNode::new(config).await.unwrap();
        Client::from_node(Arc::new(node), Default::default()).with_wallet(wallet)
    }
    #[tokio::test]
    async fn insufficient_funds_then_funding_unblocks_native_payment() {
        let chain = Testnet::new().await.unwrap();
        let network = chain.to_network();
        let funder = Wallet::new_from_private_key(
            network.clone(),
            &chain.default_wallet_private_key().unwrap(),
        )
        .unwrap();
        let wallet = Wallet::new_with_random_wallet(network);
        let address = wallet.address();
        let client = client(wallet).await;
        let request =
            PaymentRequest::Quotes(vec![([17; 32].into(), funder.address(), Amount::from(100))]);
        let adapter = Checkpoints::default();
        let mut state = attempt();
        assert!(execute(&client, &adapter, &request, &mut state, |_| true)
            .await
            .is_err());
        assert!(state.pending_payment.is_none());
        assert!(adapter
            .last
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .pending_payment
            .is_none());
        funder
            .transfer_tokens(address, Amount::from(1000))
            .await
            .unwrap();
        funder
            .transfer_gas_tokens(address, Amount::from(1_000_000_000_000_000_000u64))
            .await
            .unwrap();
        let mut state = attempt();
        let receipt = execute(&client, &adapter, &request, &mut state, |_| true)
            .await
            .unwrap();
        assert_eq!(receipt.amount, Amount::from(100));
        assert_eq!(
            client
                .require_wallet()
                .unwrap()
                .balance_of_tokens()
                .await
                .unwrap(),
            Amount::from(900)
        );
    }
    #[tokio::test]
    async fn signed_checkpoint_recovers_before_and_after_broadcast_without_paying_twice() {
        let chain = Testnet::new().await.unwrap();
        let network = chain.to_network();
        let wallet =
            Wallet::new_from_private_key(network, &chain.default_wallet_private_key().unwrap())
                .unwrap();
        let before = wallet.balance_of_tokens().await.unwrap();
        let client = client(wallet).await;
        let request =
            PaymentRequest::Quotes(vec![([18; 32].into(), [19; 20].into(), Amount::from(100))]);
        let interrupted = Checkpoints {
            stop_after_signed: true,
            ..Default::default()
        };
        assert!(
            execute(&client, &interrupted, &request, &mut attempt(), |_| true)
                .await
                .is_err()
        );
        assert_eq!(
            client
                .require_wallet()
                .unwrap()
                .balance_of_tokens()
                .await
                .unwrap(),
            before
        );
        let mut restored = interrupted.last.lock().unwrap().clone().unwrap();
        let adapter = Checkpoints::default();
        let receipt = execute(&client, &adapter, &request, &mut restored, |_| true)
            .await
            .unwrap();
        // Simulate losing proof construction after the transaction already mined.
        let mut restored = interrupted.last.lock().unwrap().clone().unwrap();
        let recovered = execute(&client, &adapter, &request, &mut restored, |_| false)
            .await
            .unwrap();
        assert_eq!(receipt.transaction_hash, recovered.transaction_hash);
        assert_eq!(
            client
                .require_wallet()
                .unwrap()
                .balance_of_tokens()
                .await
                .unwrap(),
            before - Amount::from(100)
        );
    }
}
