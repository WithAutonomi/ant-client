//! ADR-0005 E2E: earned reward eligibility over real QUIC + Anvil.
//!
//! MiniTestnet nodes run no replication engine, so no real audits fire; each
//! node instead carries a real `AuditTally` (wired as its quote generator's
//! report source) that the tests SEED with controlled facts. That makes the
//! full wire path — tally → signed report on the quote response → client
//! verification → eligibility gate at collection — deterministic: what a
//! quorum of observers testifies is exactly what the test wrote.
//!
//! Timing note: histories are seeded with real past timestamps (1 day =
//! 86 400 s, the production bucket), so no time compression is involved here.
//! The emergent audit-driven timing ("honest nodes qualify in ≈ a week") is
//! Layer 3's job — the process-level local testnet with compressed days.

mod support;

use ant_core::data::client::merkle::PaymentMode;
use ant_core::data::{compute_address, Client};
use bytes::Bytes;
use serial_test::serial;
use std::sync::Arc;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};

const COMMITTED_KEYS: u32 = 700;
const WEEK_DAYS: u64 = 7;

/// Sets the ADR-0005 enforcement env for one test and clears it on drop
/// (tests are `#[serial]`, so no cross-test interleaving).
struct EnforceGuard;
impl EnforceGuard {
    fn set() -> Self {
        std::env::set_var("ADR5_ENFORCE", "1");
        Self
    }
}
impl Drop for EnforceGuard {
    fn drop(&mut self) {
        std::env::remove_var("ADR5_ENFORCE");
    }
}

/// Seed a full clean week for every (observer, subject) pair except subjects
/// listed in `skip` — those get NO testimony anywhere (fresh/unproven nodes).
fn seed_all_except(testnet: &MiniTestnet, skip: &[usize]) {
    for subject in 0..testnet.nodes.len() {
        if skip.contains(&subject) {
            continue;
        }
        testnet.seed_history_for_all_observers(subject, COMMITTED_KEYS, WEEK_DAYS);
    }
}

/// XOR distance between a peer id and an address, comparable byte-wise.
fn xor_distance(peer: &[u8; 32], addr: &[u8; 32]) -> [u8; 32] {
    let mut d = [0u8; 32];
    for i in 0..32 {
        d[i] = peer[i] ^ addr[i];
    }
    d
}

/// Mine chunk content whose address puts `target` inside the closest
/// `within` peers of the testnet (excluding `client_index`, which never
/// quotes to itself). With ~14 nodes this converges in a handful of tries.
fn mine_content_with_target_close(
    testnet: &MiniTestnet,
    target_index: usize,
    client_index: usize,
    within: usize,
) -> Bytes {
    let peer_ids: Vec<(usize, [u8; 32])> = (0..testnet.nodes.len())
        .filter(|i| *i != client_index)
        .map(|i| (i, testnet.peer_id_bytes(i)))
        .collect();
    for salt in 0u64..100_000 {
        let content = Bytes::from(format!("adr-0005 mined payload {salt}"));
        let address = compute_address(&content);
        let mut by_distance = peer_ids.clone();
        by_distance.sort_by_key(|(_, peer)| xor_distance(peer, &address));
        if by_distance
            .iter()
            .take(within)
            .any(|(i, _)| *i == target_index)
        {
            return content;
        }
    }
    panic!("could not mine an address near the target peer");
}

/// Scenario 1 + 5 (gate-on liveness both ways): with EVERY quoter eligible,
/// enforcement changes nothing observable — a full put/get round-trip works
/// and a close group of quotes is collected. With NOBODY eligible (empty
/// tallies), enforcement degrades to today's rules and the round-trip STILL
/// works: the gate never stalls the network.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_enforced_gate_keeps_liveness_all_eligible_and_none_eligible() {
    let testnet =
        MiniTestnet::start_with_commitments_and_tallies(DEFAULT_NODE_COUNT, COMMITTED_KEYS).await;
    let node = testnet.node(3).expect("node 3 exists");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let _guard = EnforceGuard::set();

    // Phase A: nobody has testimony yet (all tallies empty) -> degraded mode,
    // uploads must still succeed.
    let content = Bytes::from("adr-0005 degraded-mode payload");
    let address = compute_address(&content);
    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("degraded-mode put must succeed (the gate never stalls)");
    assert_eq!(stored, address);

    // Phase B: seed a full week for everyone -> gate-on path, everyone
    // eligible, round-trip still green.
    seed_all_except(&testnet, &[]);
    let content = Bytes::from("adr-0005 all-eligible payload");
    let address = compute_address(&content);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("quote collection under enforcement");
    assert!(
        quotes.len() >= ant_protocol::CLOSE_GROUP_SIZE,
        "full close group under enforcement, got {}",
        quotes.len()
    );
    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("all-eligible enforced put must succeed");
    assert_eq!(stored, address);
    let retrieved = client.chunk_get(&address).await.expect("get back");
    assert!(retrieved.is_some(), "stored chunk must be retrievable");

    testnet.teardown().await;
}

/// Scenario 3b (forced single-node substitution): one node has NO testimony
/// anywhere and sits inside the closest-7 of a mined address. Enforcement
/// must widen collection, drop it from the payable set, and still assemble a
/// full close group of eligible quotes. Observe-only (default) must keep it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_unproven_quoter_is_substituted_under_enforcement() {
    let testnet =
        MiniTestnet::start_with_commitments_and_tallies(DEFAULT_NODE_COUNT, COMMITTED_KEYS).await;
    let client_index = 0usize;
    let fresh_index = 5usize;
    seed_all_except(&testnet, &[fresh_index]);

    let node = testnet.node(client_index).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let content = mine_content_with_target_close(
        &testnet,
        fresh_index,
        client_index,
        ant_protocol::CLOSE_GROUP_SIZE,
    );
    let address = compute_address(&content);
    let fresh_peer = testnet.peer_id_bytes(fresh_index);

    // Observe-only first: the unproven quoter is closest-7, so it appears.
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("observe-only quote collection");
    assert!(
        quotes
            .iter()
            .any(|(peer, _, _, _, _)| *peer.as_bytes() == fresh_peer),
        "observe-only keeps the unproven closest-7 quoter"
    );

    // Enforcement: substituted away, full close group still assembled.
    let _guard = EnforceGuard::set();
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("enforced quote collection must not degrade with substitutes available");
    assert_eq!(
        quotes.len(),
        ant_protocol::CLOSE_GROUP_SIZE,
        "a full close group of eligible quoters"
    );
    assert!(
        quotes
            .iter()
            .all(|(peer, _, _, _, _)| *peer.as_bytes() != fresh_peer),
        "the unproven quoter must be substituted out of the payable set"
    );

    // And the gated payment path completes end to end.
    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("enforced put with substitution must succeed");
    assert_eq!(stored, address);

    testnet.teardown().await;
}

/// Scenario 2 (conviction resets the dues): a convicted node is excluded even
/// though it had a full week of history; once it re-earns a fresh week of
/// passes (which also clears the outstanding-conviction marker), it is
/// payable again.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_convicted_node_excluded_until_it_reearns() {
    let testnet =
        MiniTestnet::start_with_commitments_and_tallies(DEFAULT_NODE_COUNT, COMMITTED_KEYS).await;
    let client_index = 0usize;
    let subject_index = 6usize;
    seed_all_except(&testnet, &[]);

    let node = testnet.node(client_index).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let content = mine_content_with_target_close(
        &testnet,
        subject_index,
        client_index,
        ant_protocol::CLOSE_GROUP_SIZE,
    );
    let address = compute_address(&content);
    let subject_peer = testnet.peer_id_bytes(subject_index);
    let _guard = EnforceGuard::set();

    // Convicted at every observer: the week of history is gone.
    testnet.convict_at_all_observers(subject_index, 0);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("enforced collection with a convicted closest-7 quoter");
    assert!(
        quotes
            .iter()
            .all(|(peer, _, _, _, _)| *peer.as_bytes() != subject_peer),
        "a convicted node must not be payable"
    );

    // v4 sticky convictions: a fresh week of passes alone does NOT restore
    // eligibility while the marker is outstanding.
    testnet.seed_history_for_all_observers(subject_index, COMMITTED_KEYS, WEEK_DAYS);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("collection inside the sticky period");
    assert!(
        quotes
            .iter()
            .all(|(peer, _, _, _, _)| *peer.as_bytes() != subject_peer),
        "the sticky conviction must hold through fresh passes for a dues period"
    );

    // Model "one dues period has since elapsed": re-record the conviction 8
    // days in the past (aging it beyond CONVICTION_STICKY_DAYS zeroes the
    // rows again), then re-earn a fresh week on top -> payable again.
    testnet.convict_at_all_observers(subject_index, 8);
    testnet.seed_history_for_all_observers(subject_index, COMMITTED_KEYS, WEEK_DAYS);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("collection after the sticky period + re-earned dues");
    assert!(
        quotes
            .iter()
            .any(|(peer, _, _, _, _)| *peer.as_bytes() == subject_peer),
        "a re-earned node must be payable again (conviction is one dues period, not a ban)"
    );

    testnet.teardown().await;
}

/// Silent-hopper fencing: a fenced node (unanswered challenge on a monetized
/// pin, no pass since) is excluded exactly like a convicted one, but keeps
/// its history — a single fresh pass restores it.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_fenced_node_excluded_until_next_pass() {
    let testnet =
        MiniTestnet::start_with_commitments_and_tallies(DEFAULT_NODE_COUNT, COMMITTED_KEYS).await;
    let client_index = 0usize;
    let subject_index = 4usize;
    seed_all_except(&testnet, &[]);

    let node = testnet.node(client_index).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let content = mine_content_with_target_close(
        &testnet,
        subject_index,
        client_index,
        ant_protocol::CLOSE_GROUP_SIZE,
    );
    let address = compute_address(&content);
    let subject_peer = testnet.peer_id_bytes(subject_index);
    let _guard = EnforceGuard::set();

    testnet.fence_at_all_observers(subject_index);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("enforced collection with a fenced closest-7 quoter");
    assert!(
        quotes
            .iter()
            .all(|(peer, _, _, _, _)| *peer.as_bytes() != subject_peer),
        "a fenced node must not be payable while the challenge is outstanding"
    );

    // One fresh pass per observer clears the fence; history was kept, so the
    // node is immediately payable again (no re-grind — that is conviction's
    // job).
    testnet.seed_history_for_all_observers(subject_index, COMMITTED_KEYS, 1);
    let quotes = client
        .get_store_quotes(&address, content.len() as u64, 0)
        .await
        .expect("collection after the fence clears");
    assert!(
        quotes
            .iter()
            .any(|(peer, _, _, _, _)| *peer.as_bytes() == subject_peer),
        "a fence clears on the next pass, restoring the kept history"
    );

    testnet.teardown().await;
}

/// A tracing writer that appends into a shared buffer, so a test can assert
/// on the client's structured `adr5::*` gate decisions.
#[derive(Clone)]
struct CaptureWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for CaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CaptureWriter {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Scenario 3c (merkle mixed-eligibility pool): with one unproven node in the
/// candidate field and 16+ eligible candidates available, an enforced merkle
/// upload completes end-to-end — the pool is composed eligible-first and the
/// storers' closeness/payment verification still passes. Non-vacuous by
/// construction: the client's own gate decisions must show the unproven node
/// was EVALUATED in a merkle candidate field and NEVER judged eligible, and
/// at least one merkle-pool gate dropped an ineligible candidate.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_merkle_mixed_pool_uploads_under_enforcement() {
    // Merkle pools need 16 candidates; 35 nodes matches the merkle e2e suite.
    let testnet = MiniTestnet::start_with_commitments_and_tallies(35, COMMITTED_KEYS).await;
    let fresh_index = 7usize;
    seed_all_except(&testnet, &[fresh_index]);
    let fresh_peer_hex = hex::encode(testnet.peer_id_bytes(fresh_index));

    let node = testnet.node(0).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let _guard = EnforceGuard::set();

    // Capture the gate's structured decisions for the duration of the upload.
    let capture = CaptureWriter(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    // Upload enough data for several pools so the unproven node lands in at
    // least one 32-wide candidate query with overwhelming probability.
    let data = Bytes::from(vec![0x5Au8; 2 * 1024 * 1024]);
    let result = client
        .data_upload_with_mode(data.clone(), PaymentMode::Merkle)
        .await
        .expect("enforced merkle upload with a mixed-eligibility field must succeed");
    let retrieved = client
        .data_download(&result.data_map)
        .await
        .expect("download back");
    assert_eq!(retrieved, data);

    let captured = String::from_utf8_lossy(&capture.0.lock().expect("capture lock")).to_string();
    let fresh_decisions: Vec<&str> = captured
        .lines()
        .filter(|l| l.contains("eligibility decision") && l.contains(fresh_peer_hex.as_str()))
        .collect();
    assert!(
        !fresh_decisions.is_empty(),
        "the unproven node must have been evaluated in at least one candidate field"
    );
    assert!(
        fresh_decisions.iter().all(|l| l.contains("eligible=false")),
        "the unproven node must never be judged eligible"
    );
    assert!(
        captured
            .lines()
            .any(|l| l.contains("ADR-0005 gate [merkle-pool]")
                && !l.contains("dropped 0 ineligible")),
        "at least one merkle pool must have dropped an ineligible candidate"
    );

    testnet.teardown().await;
}

/// v5 fast-growth (tier-2 dues fallback): the whole network grew fast, so
/// nobody has a week of history AT THE CURRENT (grown) size — size-eligibility
/// is empty network-wide. The two-tier gate must then fall back to
/// DUES-eligibility (a clean audited week at any size) and keep paying, WHILE
/// still excluding a fresh no-history node and a convicted node. This is the
/// scenario that motivated the whole two-tier change.
#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn adr5_fast_growth_falls_back_to_dues_but_excludes_fresh_and_convicted() {
    // Nodes commit to (and quote at) the LARGE grown size...
    const GROWN_SIZE: u32 = COMMITTED_KEYS; // 700, the current commitment
    const SMALL_HISTORY: u32 = 1; // ...but only ever passed audits when small
    let testnet =
        MiniTestnet::start_with_commitments_and_tallies(DEFAULT_NODE_COUNT, GROWN_SIZE).await;

    let client_index = 0usize;
    let fresh_index = 5usize;
    let convicted_index = 6usize;

    // Everyone except the fresh node earns a clean week — but at the SMALL
    // size, so with slack 2x nobody covers the grown quote (1*2 < 700). All
    // are dues-eligible; none are size-eligible.
    for subject in 0..testnet.nodes.len() {
        if subject == fresh_index {
            continue;
        }
        testnet.seed_history_for_all_observers(subject, SMALL_HISTORY, WEEK_DAYS);
    }
    // The convicted node has its clean week wiped at every observer.
    testnet.convict_at_all_observers(convicted_index, 0);

    let fresh_peer_hex = hex::encode(testnet.peer_id_bytes(fresh_index));
    let convicted_peer_hex = hex::encode(testnet.peer_id_bytes(convicted_index));

    let node = testnet.node(client_index).expect("client node");
    let client = Client::from_node(Arc::clone(&node), test_client_config())
        .with_wallet(testnet.wallet().clone());
    let _guard = EnforceGuard::set();

    let capture = CaptureWriter(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    let _sub_guard = tracing::subscriber::set_default(subscriber);

    // Uploads still succeed via the dues fallback despite zero size-eligibility.
    let content = Bytes::from("adr-0005 fast-growth payload");
    let address = compute_address(&content);
    let stored = client
        .chunk_put(content.clone())
        .await
        .expect("dues fallback must keep uploads working under fast growth");
    assert_eq!(stored, address);

    let captured = String::from_utf8_lossy(&capture.0.lock().expect("capture lock")).to_string();

    // The gate actually used the dues tier (not size, not ungated).
    assert!(
        captured
            .lines()
            .any(|l| l.contains("DUES-ELIGIBLE (size relaxed)")),
        "the gate must fall back to the dues tier, not stay size or go ungated"
    );

    // Every decision for a well-behaved subject: NOT size-eligible (grown size
    // uncovered) but dues-eligible.
    let sized_lines: Vec<&str> = captured
        .lines()
        .filter(|l| l.contains("eligibility decision"))
        .collect();
    assert!(
        sized_lines
            .iter()
            .any(|l| l.contains("eligible=false") && l.contains("dues_eligible=true")),
        "well-behaved nodes are dues-eligible but not size-eligible"
    );

    // The fresh node and the convicted node are NEITHER size- nor
    // dues-eligible. (A single upload's close group may not include a given
    // node; retry a few uploads so each is evaluated at least once, then
    // assert every decision for it fails both tiers — non-vacuously.)
    for extra in 0..8 {
        let c = Bytes::from(format!("adr-0005 fast-growth probe {extra}"));
        let _ = client.chunk_put(c).await;
    }
    let captured = String::from_utf8_lossy(&capture.0.lock().expect("capture lock")).to_string();
    for (label, hex) in [
        ("fresh", &fresh_peer_hex),
        ("convicted", &convicted_peer_hex),
    ] {
        let decisions: Vec<&str> = captured
            .lines()
            .filter(|l| l.contains("eligibility decision") && l.contains(hex.as_str()))
            .collect();
        assert!(
            !decisions.is_empty(),
            "the {label} node must have been evaluated at least once"
        );
        assert!(
            decisions
                .iter()
                .all(|l| l.contains("eligible=false") && l.contains("dues_eligible=false")),
            "the {label} node must fail BOTH tiers even under the dues fallback"
        );
    }

    testnet.teardown().await;
}
