//! Security attack tests for ant-core.
//!
//! Each test sets up a `MiniTestnet`, collects real quotes, tampers with the
//! proof in a specific way, and asserts the attack is REJECTED by the network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client, ClientConfig};
use ant_evm::ProofOfPayment;
use ant_node::client::hex_node_id_to_encoded_peer_id;
use ant_node::core::PeerId;
use ant_node::payment::{serialize_single_node_proof, PaymentProof, SingleNodePayment};
use bytes::Bytes;
use evmlib::common::TxHash;
use serial_test::serial;
use std::sync::Arc;
use support::MiniTestnet;

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default())
        .with_wallet(testnet.wallet().clone());
    (client, testnet)
}

/// Helper: collect quotes, pay on-chain, and return the raw `PaymentProof` struct,
/// the serialized proof bytes, and the target peer for PUT.
async fn collect_and_pay(client: &Client, content: &Bytes) -> (PaymentProof, Vec<u8>, PeerId) {
    let address = compute_address(content);
    let data_size = content.len() as u64;

    // Collect quotes
    let quotes = client
        .get_store_quotes(&address, data_size, 0)
        .await
        .expect("quotes should succeed");

    // Pin the first peer as the PUT target (closest from DHT)
    let target_peer = quotes.first().expect("should have quotes").0;

    // Build peer_quotes and payment
    let mut peer_quotes = Vec::with_capacity(quotes.len());
    let mut quotes_for_payment = Vec::with_capacity(quotes.len());
    for (peer_id, _addrs, quote, price) in quotes {
        let encoded =
            hex_node_id_to_encoded_peer_id(&peer_id.to_hex()).expect("peer ID conversion");
        peer_quotes.push((encoded, quote.clone()));
        quotes_for_payment.push((quote, price));
    }

    // Pay on-chain
    let payment = SingleNodePayment::from_quotes(quotes_for_payment).expect("payment creation");
    let wallet = client.wallet().expect("wallet should be set");
    let tx_hashes = payment.pay(wallet).await.expect("on-chain payment");

    // Build proof
    let proof = PaymentProof {
        proof_of_payment: ProofOfPayment { peer_quotes },
        tx_hashes,
    };
    let proof_bytes = serialize_single_node_proof(&proof).expect("serialize proof");

    (proof, proof_bytes, target_peer)
}

// ─── Test 1: Forged ML-DSA-65 Signatures ────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_forged_signature() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("forged signature attack test data");
    let (proof, _, target_peer) = collect_and_pay(&client, &content).await;

    // Corrupt all ML-DSA-65 signatures by flipping bytes
    let mut tampered_quotes: Vec<_> = proof.proof_of_payment.peer_quotes.clone();
    for (_encoded_peer, quote) in &mut tampered_quotes {
        for byte in &mut quote.signature {
            *byte = byte.wrapping_add(1);
        }
    }

    let tampered_proof = PaymentProof {
        proof_of_payment: ProofOfPayment {
            peer_quotes: tampered_quotes,
        },
        tx_hashes: proof.tx_hashes.clone(),
    };
    let tampered_bytes = rmp_serde::to_vec(&tampered_proof).expect("serialize tampered proof");

    let result = client
        .chunk_put_with_proof(content, tampered_bytes, &target_peer, &[])
        .await;

    assert!(
        result.is_err(),
        "PUT with forged signatures should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 2: Wrong Chunk Address ─────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_wrong_chunk_address() {
    let (client, testnet) = setup().await;

    // Get quotes and pay for chunk A
    let content_a = Bytes::from("chunk A - the one we paid for");
    let (_, proof_bytes, target_peer) = collect_and_pay(&client, &content_a).await;

    // Try to store chunk B using A's proof
    let content_b = Bytes::from("chunk B - the interloper");
    let result = client
        .chunk_put_with_proof(content_b, proof_bytes, &target_peer, &[])
        .await;

    assert!(
        result.is_err(),
        "PUT with wrong chunk address should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 3: Replay Proof for Different Chunk ────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_replay_different_chunk() {
    let (client, testnet) = setup().await;

    // Legitimately store chunk A
    let content_a = Bytes::from("replay attack - legitimate chunk A");
    let (proof_bytes_a, peers_a) = client
        .pay_for_storage(&compute_address(&content_a), content_a.len() as u64, 0)
        .await
        .expect("payment for chunk A should succeed");
    let (target_peer_a, target_addrs_a) =
        peers_a.first().expect("should have quoted peers").clone();

    let addr_a = client
        .chunk_put_with_proof(
            content_a,
            proof_bytes_a.clone(),
            &target_peer_a,
            &target_addrs_a,
        )
        .await
        .expect("storing chunk A should succeed");

    // Verify chunk A is actually stored
    let retrieved = client.chunk_get(&addr_a).await.expect("get should succeed");
    assert!(retrieved.is_some(), "Chunk A should be stored");

    // Try to store chunk B using A's proof
    let content_b = Bytes::from("replay attack - sneaky chunk B");
    let result = client
        .chunk_put_with_proof(content_b, proof_bytes_a, &target_peer_a, &target_addrs_a)
        .await;

    assert!(
        result.is_err(),
        "Replaying chunk A's proof for chunk B should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 4: Zero-Amount Payment (Empty TX Hashes) ──────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_zero_amount_payment() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("zero amount attack test data");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    // Collect real quotes but DON'T pay on-chain
    let quotes = client
        .get_store_quotes(&address, data_size, 0)
        .await
        .expect("quotes should succeed");

    let target_peer = quotes.first().expect("should have quotes").0;
    let mut peer_quotes = Vec::with_capacity(quotes.len());
    for (peer_id, _addrs, quote, _price) in quotes {
        let encoded =
            hex_node_id_to_encoded_peer_id(&peer_id.to_hex()).expect("peer ID conversion");
        peer_quotes.push((encoded, quote));
    }

    // Build a proof with valid structure but empty tx_hashes
    let fake_proof = PaymentProof {
        proof_of_payment: ProofOfPayment { peer_quotes },
        tx_hashes: vec![],
    };
    let fake_bytes = rmp_serde::to_vec(&fake_proof).expect("serialize fake proof");

    let result = client
        .chunk_put_with_proof(content, fake_bytes, &target_peer, &[])
        .await;

    assert!(
        result.is_err(),
        "PUT with zero-amount (empty tx_hashes) should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 5: Fabricated Transaction Hash ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_fabricated_tx_hash() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("fabricated tx hash attack test data");
    let address = compute_address(&content);
    let data_size = content.len() as u64;

    // Collect real quotes
    let quotes = client
        .get_store_quotes(&address, data_size, 0)
        .await
        .expect("quotes should succeed");

    let target_peer = quotes.first().expect("should have quotes").0;
    let mut peer_quotes = Vec::with_capacity(quotes.len());
    for (peer_id, _addrs, quote, _price) in quotes {
        let encoded =
            hex_node_id_to_encoded_peer_id(&peer_id.to_hex()).expect("peer ID conversion");
        peer_quotes.push((encoded, quote));
    }

    // Build proof with a fabricated tx hash (never happened on-chain)
    let fake_tx_hash = TxHash::from([0xDE; 32]);
    let fake_proof = PaymentProof {
        proof_of_payment: ProofOfPayment { peer_quotes },
        tx_hashes: vec![fake_tx_hash],
    };
    let fake_bytes = rmp_serde::to_vec(&fake_proof).expect("serialize fake proof");

    let result = client
        .chunk_put_with_proof(content, fake_bytes, &target_peer, &[])
        .await;

    assert!(
        result.is_err(),
        "PUT with fabricated tx hash should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 6: Double-Spend Same Proof (Idempotent) ───────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_double_spend_same_proof() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("double spend idempotency test data");
    let (_, proof_bytes, target_peer) = collect_and_pay(&client, &content).await;

    // Store chunk A successfully
    let addr1 = client
        .chunk_put_with_proof(content.clone(), proof_bytes.clone(), &target_peer, &[])
        .await
        .expect("first PUT should succeed");

    // Try to store the SAME chunk with the SAME proof again
    let addr2 = client
        .chunk_put_with_proof(content, proof_bytes, &target_peer, &[])
        .await
        .expect("second PUT (idempotent AlreadyExists) should succeed");

    assert_eq!(
        addr1, addr2,
        "Double-spend with same proof should return same address (idempotent)"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 7: Corrupted Public Key ───────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_corrupted_public_key() {
    let (client, testnet) = setup().await;

    let content = Bytes::from("corrupted public key attack test data");
    let (proof, _, target_peer) = collect_and_pay(&client, &content).await;

    // Replace all pub_key fields with random bytes
    let mut tampered_quotes: Vec<_> = proof.proof_of_payment.peer_quotes.clone();
    for (_encoded_peer, quote) in &mut tampered_quotes {
        // Replace with random garbage of the same length
        for byte in &mut quote.pub_key {
            *byte = byte.wrapping_add(0x42);
        }
    }

    let tampered_proof = PaymentProof {
        proof_of_payment: ProofOfPayment {
            peer_quotes: tampered_quotes,
        },
        tx_hashes: proof.tx_hashes.clone(),
    };
    let tampered_bytes = rmp_serde::to_vec(&tampered_proof).expect("serialize tampered proof");

    let result = client
        .chunk_put_with_proof(content, tampered_bytes, &target_peer, &[])
        .await;

    assert!(
        result.is_err(),
        "PUT with corrupted public keys should be rejected"
    );

    drop(client);
    testnet.teardown().await;
}

// ─── Test 8: Client Without Wallet ──────────────────────────────────────────

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn test_attack_client_without_wallet() {
    let testnet = MiniTestnet::start(6).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    // Create client WITHOUT wallet
    let client = Client::from_node(Arc::clone(&node), ClientConfig::default());

    let content = Bytes::from("no wallet attack test data");
    let result = client.chunk_put(content).await;

    assert!(result.is_err(), "chunk_put without wallet should fail");
    let err_msg = format!("{}", result.expect_err("should have error"));
    let err_lower = err_msg.to_lowercase();
    assert!(
        err_lower.contains("wallet"),
        "Error should mention wallet, got: {err_msg}"
    );

    drop(client);
    testnet.teardown().await;
}
