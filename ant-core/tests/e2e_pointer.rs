//! End-to-end tests for pointers against a local testnet with real EVM payments.
//!
//! Everything else about pointers is tested against a function. This is the
//! test that a pointer works: real nodes, real QUIC, real Anvil settlement, and
//! the client's own create / update / get / resolve path over the top.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::Client;
use ant_protocol::pointer::{pointer_address, Pointer, PointerTarget, PointerTargetKind};
use ant_protocol::pqc::api::{ml_dsa_65, MlDsaPublicKey, MlDsaSecretKey};
use bytes::Bytes;
use serial_test::serial;
use std::sync::Arc;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};

async fn setup() -> (Client, MiniTestnet) {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("Node 3 should exist");

    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    (client, testnet)
}

/// A fresh owner. Every test needs its own: the address *is* the key, so two
/// tests sharing one would be writing to one pointer.
fn owner() -> (MlDsaPublicKey, MlDsaSecretKey) {
    ml_dsa_65()
        .generate_keypair()
        .expect("generate ML-DSA-65 keypair")
}

fn chunk_target(byte: u8) -> PointerTarget {
    PointerTarget::new(PointerTargetKind::Chunk, [byte; 32])
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_pointer_is_created_paid_for_and_read_back() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();

    let address = client
        .pointer_create(&sk, &pk, chunk_target(1))
        .await
        .expect("pointer_create should succeed with payment");

    // Public-key addressed: the client knew the address before it asked anyone.
    assert_eq!(address, pointer_address(&pk));

    let record = client
        .pointer_get(&address)
        .await
        .expect("pointer_get should succeed")
        .expect("the pointer must be found after creating it");

    assert_eq!(record.counter(), 0, "a create is counter 0");
    assert_eq!(record.target(), chunk_target(1));
    assert_eq!(record.address(), address);
    assert_eq!(
        record.owner().to_bytes(),
        pk.to_bytes(),
        "the record carries its own owner key"
    );

    drop(client);
    testnet.teardown().await;
}

/// Pay to update: each update is its own paid state at `counter + 1`, and the
/// network serves the new one.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_update_is_paid_for_and_replaces_what_the_network_serves() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();

    let address = client
        .pointer_create(&sk, &pk, chunk_target(1))
        .await
        .expect("create");

    for expected_counter in 1..=2u64 {
        let updated = client
            .pointer_update(&sk, &pk, chunk_target(expected_counter as u8 + 1))
            .await
            .expect("pointer_update should succeed with payment");
        assert_eq!(updated, address, "an update stays at the same address");

        let record = client
            .pointer_get(&address)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(
            record.counter(),
            expected_counter,
            "one payment buys exactly one increment"
        );
        assert_eq!(record.target(), chunk_target(expected_counter as u8 + 1));
    }

    drop(client);
    testnet.teardown().await;
}

/// Re-submitting a state the network already holds is answered as stored
/// without writing anything, so a retry after a timeout cannot lose the update
/// or fork it.
///
/// "Buys nothing" is literal: the write is still paid for, because the node
/// cannot tell a retry from a fresh submission until it has been paid to look.
/// What the retry cannot do is move the pointer.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn resubmitting_a_stored_state_is_accepted_and_changes_nothing() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();

    let address = client
        .pointer_create(&sk, &pk, chunk_target(7))
        .await
        .expect("create");
    let stored = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");

    client
        .pointer_put(&stored)
        .await
        .expect("re-submitting a stored record must not be refused");

    let after = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(after.state_id(), stored.state_id(), "nothing moved");
    assert_eq!(after.counter(), 0);

    drop(client);
    testnet.teardown().await;
}

/// A pointer may target another pointer. Resolution walks the chain client-side
/// — the node never reads a target — and ends at the chunk.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_chain_of_pointers_resolves_to_the_chunk_at_its_end() {
    let (client, testnet) = setup().await;
    let (last_pk, last_sk) = owner();
    let (first_pk, first_sk) = owner();

    let content = Bytes::from("the chunk a pointer chain ends at");
    let chunk = client.chunk_put(content.clone()).await.expect("chunk_put");

    // The end of the chain points at the chunk.
    let last = client
        .pointer_create(
            &last_sk,
            &last_pk,
            PointerTarget::new(PointerTargetKind::Chunk, chunk),
        )
        .await
        .expect("create the last hop");

    // The head points at that pointer.
    let first = client
        .pointer_create(
            &first_sk,
            &first_pk,
            PointerTarget::new(PointerTargetKind::Pointer, last),
        )
        .await
        .expect("create the first hop");

    let resolved = client
        .pointer_resolve(&first)
        .await
        .expect("the chain must resolve");
    assert_eq!(resolved.kind(), Some(PointerTargetKind::Chunk));
    assert_eq!(resolved.address, chunk);

    let fetched = client
        .chunk_get(&resolved.address)
        .await
        .expect("chunk_get")
        .expect("the chunk the chain names must be there");
    assert_eq!(fetched.content.as_ref(), content.as_ref());

    drop(client);
    testnet.teardown().await;
}

/// A pointer that does not exist reads as absent, not as an error.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn an_address_nobody_wrote_reads_as_absent() {
    let (client, testnet) = setup().await;
    let (pk, _sk) = owner();

    assert!(
        client
            .pointer_get(&pointer_address(&pk))
            .await
            .expect("a read of an empty address is not an error")
            .is_none(),
        "nothing was ever written here"
    );

    drop(client);
    testnet.teardown().await;
}

/// A counter that does not follow what the network holds is refused, however
/// well signed. Without this an owner pays once, jumps the counter, and skips
/// every intermediate payment.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_signed_record_that_skips_the_counter_is_refused() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();

    let address = client
        .pointer_create(&sk, &pk, chunk_target(1))
        .await
        .expect("create");

    // Perfectly signed by the owner, and not the successor of counter 0.
    let jumped = Pointer::sign(&sk, &pk, 9, chunk_target(2)).expect("sign");
    assert!(
        client.pointer_put(&jumped).await.is_err(),
        "the network must refuse a counter that skips ahead"
    );

    let held = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(held.counter(), 0, "the refused jump changed nothing");
    assert_eq!(held.target(), chunk_target(1));

    drop(client);
    testnet.teardown().await;
}
