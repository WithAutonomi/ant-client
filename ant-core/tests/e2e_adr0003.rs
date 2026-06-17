//! ADR-0003 end-to-end: commitment-bound quote pricing.
//!
//! Proves, against a real in-process QUIC testnet + Anvil EVM, that:
//!
//! 1. A node carrying a live storage commitment emits a COMMITMENT-BOUND quote:
//!    `committed_key_count == N`, `commitment_pin == Some`, the price is exactly
//!    `calculate_price(N)`, and the signed commitment is SHIPPED in the quote
//!    response (ADR-0003 "the commitment arrived with the quote").
//! 2. The client's forced-price gate ACCEPTS those bound quotes (they are
//!    self-consistent) and a full pay → store → retrieve round-trip succeeds —
//!    so the storer accepts the bound quotes and the forwarded sidecars too.
//! 3. A node with NO commitment emits a valid BASELINE quote `(0, None)` priced
//!    at `calculate_price(0)`, and the full flow still works.
//!
//! The OFF-CURVE rejection (a node charging above its committed count is dropped
//! before payment) is proven directly on the gate in the `quote.rs` unit tests
//! (`classifier_drops_off_curve_quote_with_typed_error` + the
//! `quote_commitment_binding_is_valid` suite) — honest nodes never emit an
//! off-curve quote, so it cannot be surfaced through a live happy-path network.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{compute_address, Client};
use ant_protocol::payment::calculate_price;
use ant_protocol::payment::commitment::{
    commitment_hash, verify_commitment_signature, StorageCommitment,
};
use bytes::Bytes;
use serial_test::serial;
use std::sync::Arc;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};

/// The committed key count every node attests in these tests. Picked above the
/// pricing baseline so `calculate_price(N)` is strictly greater than the
/// empty-node baseline — a bound quote is visibly distinct from a baseline one.
const COMMITTED_KEYS: u32 = 9_000;

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr0003_bound_quotes_are_shipped_priced_and_resolve() {
    let testnet = MiniTestnet::start_with_commitments(DEFAULT_NODE_COUNT, COMMITTED_KEYS).await;
    let node = testnet.node(3).expect("node 3 exists");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    let content = Bytes::from("adr-0003 bound-quote payload");
    let address = compute_address(&content);

    // Collect quotes directly so we can inspect the ADR-0003 binding the client
    // verified before it would pay.
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("quote collection should reach quorum");

    assert!(
        quotes.len() >= ant_protocol::CLOSE_GROUP_SIZE,
        "must collect a full close group of bound quotes, got {}",
        quotes.len()
    );

    let expected_price = calculate_price(COMMITTED_KEYS as usize);
    for (peer_id, _addrs, quote, price, commitment) in &quotes {
        // Bound shape: the count is what the node committed, with a pin.
        assert_eq!(
            quote.committed_key_count, COMMITTED_KEYS,
            "quote from {peer_id} must carry the committed key count"
        );
        let pin = quote
            .commitment_pin
            .expect("a bound quote must carry a commitment pin");

        // Forced price: exactly calculate_price(N), by recomputation. This is
        // the ceiling — a node cannot charge above what it can prove it stores.
        assert_eq!(
            quote.price, expected_price,
            "quote from {peer_id} must be priced at calculate_price(committed_key_count)"
        );
        assert_eq!(*price, expected_price);

        // The commitment arrived WITH the quote (no separate fetch needed).
        let sidecar = commitment
            .as_ref()
            .expect("a bound quote response must ship its signed commitment");

        // FULLY resolve the shipped commitment exactly as the client gate does
        // before paying — this is the real anti-cheat invariant, not just
        // "non-empty bytes". The commitment must deserialize, carry a valid
        // signature, be bound to the quoting peer, hash to the quote's pin, and
        // attest exactly the claimed count.
        let resolved: StorageCommitment =
            rmp_serde::from_slice(sidecar).expect("shipped commitment must deserialize");
        assert!(
            verify_commitment_signature(&resolved),
            "shipped commitment from {peer_id} must be validly signed"
        );
        assert_eq!(
            resolved.sender_peer_id,
            *peer_id.as_bytes(),
            "shipped commitment must be bound to the quoting peer"
        );
        assert_eq!(
            compute_address(&resolved.sender_public_key),
            *peer_id.as_bytes(),
            "BLAKE3(sender_public_key) must equal the quoting peer (full peer binding)"
        );
        assert_eq!(
            commitment_hash(&resolved),
            Some(pin),
            "shipped commitment must hash to the quote's pin"
        );
        assert_eq!(
            resolved.key_count, COMMITTED_KEYS,
            "shipped commitment must attest exactly the committed key count"
        );
    }

    // Full round-trip: the client paid (accepting the bound quotes + forwarding
    // the sidecars), the storers accepted the bundle, and the chunk is back.
    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("paid put with commitment-bound quotes must succeed");
    assert_eq!(stored, address);

    let retrieved = client
        .chunk_get(&address)
        .await
        .expect("chunk_get should succeed")
        .expect("chunk must be found after storing");
    assert_eq!(retrieved.content.as_ref(), content.as_ref());

    drop(client);
    testnet.teardown().await;
}

/// The other half: a node with NO commitment emits a valid BASELINE quote
/// `(0, None)` priced at `calculate_price(0)`, and the full flow still works.
/// This guards the baseline branch of the same forced-price gate.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr0003_baseline_quotes_still_work() {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let node = testnet.node(3).expect("node 3 exists");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());

    let content = Bytes::from("adr-0003 baseline payload");
    let address = compute_address(&content);

    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("quote collection should reach quorum");

    let baseline = calculate_price(0);
    for (peer_id, _addrs, quote, _price, commitment) in &quotes {
        assert_eq!(
            quote.committed_key_count, 0,
            "no-commitment node must quote count 0 ({peer_id})"
        );
        assert!(
            quote.commitment_pin.is_none(),
            "baseline quote pins nothing ({peer_id})"
        );
        assert_eq!(
            quote.price, baseline,
            "baseline quote must price at calculate_price(0) ({peer_id})"
        );
        assert!(
            commitment.is_none(),
            "baseline quote ships no commitment ({peer_id})"
        );
    }

    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("paid put with baseline quotes must succeed");
    assert_eq!(stored, address);

    drop(client);
    testnet.teardown().await;
}
