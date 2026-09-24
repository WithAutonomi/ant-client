//! End-to-end tests for pointers against a local testnet with real EVM payments.
//!
//! Everything else about pointers is tested against a function. This is the
//! test that a pointer works: real nodes, real QUIC, real Anvil settlement, and
//! the client's own create / update / get / resolve path over the top.
//!
//! The fork and catch-up tests also need what no client call can produce on
//! purpose: a state that reached only part of the close group. They pay for a
//! state once, as the client does, and deliver it to chosen close-group nodes
//! through each node's own request handler, so payment verification, the
//! closeness gate and the merge all run exactly as they do for a PUT that came
//! over the network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, XorName};
use ant_node::storage::AntProtocol;
use ant_protocol::chunk::{
    ChunkMessage, ChunkMessageBody, PointerGetRequest, PointerGetResponse, PointerPutRequest,
    PointerPutResponse,
};
use ant_protocol::pointer::{
    pointer_address, Pointer, PointerTarget, PointerTargetKind, DATA_TYPE_POINTER, POINTER_WIRE_LEN,
};
use ant_protocol::pqc::api::{ml_dsa_65, MlDsaPublicKey, MlDsaSecretKey};
use ant_protocol::CLOSE_GROUP_SIZE;
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

/// The close group the client writes `address` to, found by the lookup the
/// client itself uses, as each node's own request handler.
async fn close_group(
    client: &Client,
    testnet: &MiniTestnet,
    address: &XorName,
) -> Vec<Arc<AntProtocol>> {
    let peers = client
        .network()
        .find_closest_peers(address, CLOSE_GROUP_SIZE)
        .await
        .expect("close group lookup");
    let group: Vec<Arc<AntProtocol>> = peers
        .iter()
        .filter_map(|(peer_id, _)| {
            testnet.nodes.iter().find_map(|node| {
                let p2p = node.p2p_node.as_ref()?;
                if p2p.peer_id() == peer_id {
                    node.protocol.clone()
                } else {
                    None
                }
            })
        })
        .collect();
    assert_eq!(
        group.len(),
        CLOSE_GROUP_SIZE,
        "every close-group peer is a testnet node"
    );
    group
}

/// Pay for `record`'s state once, the way the client does, and build the PUT
/// that carries the proof.
async fn paid(client: &Client, record: &Pointer) -> PointerPutRequest {
    let (proof, _) = client
        .pay_for_storage_split(
            &record.address(),
            &record.state_id(),
            POINTER_WIRE_LEN as u64,
            DATA_TYPE_POINTER,
        )
        .await
        .expect("pay for the pointer state");
    PointerPutRequest::with_payment(Bytes::from(record.to_bytes()), proof)
}

/// Hand `request` to one node's request handler and return its answer.
async fn put_to(node: &AntProtocol, request: &PointerPutRequest) -> PointerPutResponse {
    let message = ChunkMessage {
        request_id: 1,
        body: ChunkMessageBody::PointerPutRequest(request.clone()),
    };
    let reply = node
        .try_handle_request(&message.encode().expect("encode"))
        .await
        .expect("handle")
        .expect("a pointer PUT is answered");
    match ChunkMessage::decode(&reply).expect("decode").body {
        ChunkMessageBody::PointerPutResponse(response) => response,
        other => panic!("expected a pointer PUT response, got {other:?}"),
    }
}

/// What one node serves at `address`.
async fn held_by(node: &AntProtocol, address: &XorName) -> Option<Pointer> {
    let message = ChunkMessage {
        request_id: 2,
        body: ChunkMessageBody::PointerGetRequest(PointerGetRequest::new(*address)),
    };
    let reply = node
        .try_handle_request(&message.encode().expect("encode"))
        .await
        .expect("handle")
        .expect("a pointer GET is answered");
    match ChunkMessage::decode(&reply).expect("decode").body {
        ChunkMessageBody::PointerGetResponse(PointerGetResponse::Success { record }) => {
            Some(Pointer::from_bytes(&record).expect("a served record verifies"))
        }
        ChunkMessageBody::PointerGetResponse(PointerGetResponse::NotFound { .. }) => None,
        other => panic!("expected a pointer GET response, got {other:?}"),
    }
}

/// Deliver a paid `record` to each of `nodes` and require every one to store it.
async fn store_on(client: &Client, nodes: &[Arc<AntProtocol>], record: &Pointer) {
    let request = paid(client, record).await;
    for (index, node) in nodes.iter().enumerate() {
        match put_to(node, &request).await {
            PointerPutResponse::Success { address, state_id } => {
                assert_eq!(address, record.address());
                assert_eq!(state_id, record.state_id());
            }
            other => panic!(
                "node {index} refused a paid state at counter {}: {other:?}",
                record.counter()
            ),
        }
    }
}

/// Require each of `nodes` to serve exactly `record`'s state.
async fn assert_held(nodes: &[Arc<AntProtocol>], record: &Pointer) {
    for (index, node) in nodes.iter().enumerate() {
        let held = held_by(node, &record.address())
            .await
            .unwrap_or_else(|| panic!("node {index} holds nothing"));
        assert_eq!(
            held.state_id(),
            record.state_id(),
            "node {index} holds counter {} instead of counter {}",
            held.counter(),
            record.counter()
        );
    }
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

/// Pay to update: each update is its own paid state, and the network serves
/// the new one.
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
            "pointer_update signs one past what the network serves"
        );
        assert_eq!(record.target(), chunk_target(expected_counter as u8 + 1));
    }

    drop(client);
    testnet.teardown().await;
}

/// A node that already holds a pointer takes a paid update to it. The chunk
/// path answers "already exists" and stops; a pointer must not, or every
/// update after the first is lost. Checked on every node of the close group,
/// not just a quorum, and then through the client's own update path.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn every_close_group_node_accepts_a_paid_update_to_a_pointer_it_holds() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();
    let address = pointer_address(&pk);
    let group = close_group(&client, &testnet, &address).await;

    let created = Pointer::create(&sk, &pk, chunk_target(1)).expect("sign");
    store_on(&client, &group, &created).await;
    assert_held(&group, &created).await;

    // Every node holds the pointer now, and every one must take the update.
    let updated = created.update(&sk, chunk_target(2)).expect("sign");
    store_on(&client, &group, &updated).await;
    assert_held(&group, &updated).await;

    // The same through the client's own path.
    client
        .pointer_update(&sk, &pk, chunk_target(3))
        .await
        .expect("pointer_update with payment must not be refused");
    let record = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(record.counter(), 2);
    assert_eq!(record.target(), chunk_target(3));

    drop(client);
    testnet.teardown().await;
}

/// The counter orders states; it does not meter them. A paid record that skips
/// counters is taken, and the client's next update carries on from it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_paid_update_may_skip_counters() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();

    let address = client
        .pointer_create(&sk, &pk, chunk_target(1))
        .await
        .expect("create");

    let skipped = Pointer::sign(&sk, &pk, 9, chunk_target(2)).expect("sign");
    client
        .pointer_put(&skipped)
        .await
        .expect("a paid record that skips counters must be taken");
    let held = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(held.counter(), 9);
    assert_eq!(held.target(), chunk_target(2));

    client
        .pointer_update(&sk, &pk, chunk_target(3))
        .await
        .expect("an update after a skip");
    let held = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(held.counter(), 10);
    assert_eq!(held.target(), chunk_target(3));

    drop(client);
    testnet.teardown().await;
}

/// A write ends once its quorum has answered, so a node can miss updates. The
/// next update brings it level, however far behind it was.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn nodes_that_missed_updates_take_the_next_one() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();
    let address = pointer_address(&pk);
    let group = close_group(&client, &testnet, &address).await;
    let (current, lagging) = group.split_at(5);

    let created = Pointer::create(&sk, &pk, chunk_target(1)).expect("sign");
    store_on(&client, &group, &created).await;

    // Two updates reach only five nodes; the same two nodes miss both.
    let first = created.update(&sk, chunk_target(2)).expect("sign");
    store_on(&client, current, &first).await;
    let second = first.update(&sk, chunk_target(3)).expect("sign");
    store_on(&client, current, &second).await;
    assert_held(current, &second).await;
    assert_held(lagging, &created).await;

    // Five nodes back the current state, so the network still reads it.
    let read = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(read.state_id(), second.state_id());

    // The next update is three counters ahead of what the laggards hold, and
    // every node takes it, the laggards included.
    let next = second.update(&sk, chunk_target(4)).expect("sign");
    store_on(&client, &group, &next).await;
    assert_held(&group, &next).await;

    drop(client);
    testnet.teardown().await;
}

/// Two paid states at one counter, each reaching a different part of the close
/// group, are a fork. A read settles it the way every node would, by the merge
/// rule, and one update at the next counter heals it on every node.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_fork_reads_as_its_merge_winner_and_the_next_counter_heals_it() {
    let (client, testnet) = setup().await;
    let (pk, sk) = owner();
    let address = pointer_address(&pk);
    let group = close_group(&client, &testnet, &address).await;
    let (winning_side, losing_side) = group.split_at(3);

    let created = Pointer::create(&sk, &pk, chunk_target(1)).expect("sign");
    store_on(&client, &group, &created).await;

    // Both at counter 1. Smaller target bytes win, and the larger side of the
    // group holds the loser, so a read cannot settle it by counting heads.
    let winner = Pointer::sign(&sk, &pk, 1, chunk_target(2)).expect("sign");
    let loser = Pointer::sign(&sk, &pk, 1, chunk_target(9)).expect("sign");
    assert!(winner.replaces(&loser), "smaller target bytes win");
    store_on(&client, winning_side, &winner).await;
    store_on(&client, losing_side, &loser).await;
    assert_held(winning_side, &winner).await;
    assert_held(losing_side, &loser).await;

    // Both sides are backed by at least two nodes; the read takes the winner.
    let read = client
        .pointer_get(&address)
        .await
        .expect("a forked pointer still reads")
        .expect("present");
    assert_eq!(read.state_id(), winner.state_id());

    // The owner updates as usual: one past what the read returned. That lands
    // on both sides of the fork, so it reaches the write quorum.
    client
        .pointer_update(&sk, &pk, chunk_target(5))
        .await
        .expect("an update must heal a fork");
    let healed = client
        .pointer_get(&address)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(healed.counter(), 2);
    assert_eq!(healed.target(), chunk_target(5));

    // The write stopped at its quorum. Delivering the same state to the whole
    // group shows that no node on either side refuses it: each either already
    // holds it or takes it now.
    let request = paid(&client, &healed).await;
    for (index, node) in group.iter().enumerate() {
        match put_to(node, &request).await {
            PointerPutResponse::Success { state_id, .. }
            | PointerPutResponse::Unchanged { state_id, .. } => {
                assert_eq!(state_id, healed.state_id(), "node {index}");
            }
            other => panic!("node {index} refused the healing update: {other:?}"),
        }
    }
    assert_held(&group, &healed).await;

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
