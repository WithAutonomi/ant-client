//! Native payment journal recovery against a real Anvil chain.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use ant_core::data::client::{
    batch::ChunkPaymentPlan,
    upload::{UploadAdapter, UploadPayment, UploadRecord},
    upload_state::UploadState,
};
use ant_core::data::{
    error::{Error, Result},
    Client,
};
use ant_protocol::evm::journal::PaymentRequest;
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
    async fn load(&self, _: UploadRecord) -> Result<bytes::Bytes> {
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
    let client = client(wallet.clone()).await;
    let request =
        PaymentRequest::Quotes(vec![([17; 32].into(), funder.address(), Amount::from(100))]);
    let adapter = Checkpoints::default();
    let mut state = UploadState::default();
    assert!(client
        .test_execute_native_payment(&adapter, &request, &mut state, true)
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
    let mut state = UploadState::default();
    let receipt = client
        .test_execute_native_payment(&adapter, &request, &mut state, true)
        .await
        .unwrap();
    assert_eq!(receipt.amount, Amount::from(100));
    assert_eq!(wallet.balance_of_tokens().await.unwrap(), Amount::from(900));
}
#[tokio::test]
async fn signed_checkpoint_recovers_before_and_after_broadcast_without_paying_twice() {
    let chain = Testnet::new().await.unwrap();
    let network = chain.to_network();
    let wallet =
        Wallet::new_from_private_key(network, &chain.default_wallet_private_key().unwrap())
            .unwrap();
    let before = wallet.balance_of_tokens().await.unwrap();
    let client = client(wallet.clone()).await;
    let request =
        PaymentRequest::Quotes(vec![([18; 32].into(), [19; 20].into(), Amount::from(100))]);
    let interrupted = Checkpoints {
        stop_after_signed: true,
        ..Default::default()
    };
    assert!(client
        .test_execute_native_payment(&interrupted, &request, &mut UploadState::default(), true)
        .await
        .is_err());
    assert_eq!(wallet.balance_of_tokens().await.unwrap(), before);
    let mut restored = interrupted.last.lock().unwrap().clone().unwrap();
    let adapter = Checkpoints::default();
    let receipt = client
        .test_execute_native_payment(&adapter, &request, &mut restored, true)
        .await
        .unwrap();
    // Simulate losing proof construction after the transaction already mined.
    let mut restored = interrupted.last.lock().unwrap().clone().unwrap();
    let recovered = client
        .test_execute_native_payment(&adapter, &request, &mut restored, false)
        .await
        .unwrap();
    assert_eq!(receipt.transaction_hash, recovered.transaction_hash);
    assert_eq!(
        wallet.balance_of_tokens().await.unwrap(),
        before - Amount::from(100)
    );
}
