//! E2E tests for the swarm: clients reading the same file meet through the
//! nodes closest to it, share its records over real WebRTC data channels,
//! and the nodes serve only what no other client holds.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use ant_core::data::{Client, DataMap};
use rand::RngCore;
use serial_test::serial;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use support::{test_client_config, MiniTestnet, DEFAULT_NODE_COUNT};
use tempfile::{NamedTempFile, TempDir};

/// How long two clients may take to meet and open a link.
const MEET_TIMEOUT: Duration = Duration::from_secs(60);
/// About the size of each record of a file three times as long: several 16
/// KiB pieces on a link.
const RECORD_BYTES: usize = 64 * 1024;

fn reader(testnet: &MiniTestnet, node: usize) -> Client {
    Client::from_node(
        Arc::clone(&testnet.node(node).expect("node exists")),
        test_client_config(),
    )
}

/// Uploads `size` varied bytes and returns them with their DataMap.
async fn upload(testnet: &MiniTestnet, size: usize) -> (Vec<u8>, DataMap) {
    let uploader = reader(testnet, 3).with_wallet(testnet.wallet().clone());
    // Incompressible, so its records are full size.
    let mut data = vec![0u8; size];
    rand::thread_rng().fill_bytes(&mut data);
    let mut file = NamedTempFile::new().expect("temp file");
    file.write_all(&data).expect("write");
    file.flush().expect("flush");
    let result = uploader.file_upload(file.path()).await.expect("upload");
    (data, result.data_map)
}

async fn download(client: &Client, data_map: &DataMap, dir: &Path, name: &str) -> Vec<u8> {
    let path = dir.join(name);
    client
        .file_download(data_map, &path)
        .await
        .expect("download");
    std::fs::read(&path).expect("read download")
}

async fn until_linked(clients: &[&Client]) {
    let deadline = tokio::time::Instant::now() + MEET_TIMEOUT;
    while clients.iter().any(|client| client.swarm_stats().links == 0) {
        assert!(
            tokio::time::Instant::now() < deadline,
            "clients did not meet: {:?}",
            clients
                .iter()
                .map(|client| client.swarm_stats())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The first messages on a new link say what each side holds.
    tokio::time::sleep(Duration::from_millis(500)).await;
}

/// Waits until `client` has heard that a linked reader holds every record
/// of the file, as the first messages on a link say: a read that starts
/// before then goes to the nodes, as it should.
async fn until_offered(client: &Client, data_map: &DataMap) {
    let deadline = tokio::time::Instant::now() + MEET_TIMEOUT;
    while !data_map
        .infos()
        .iter()
        .all(|info| client.swarm_could_serve(&info.dst_hash.0))
    {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never heard what the other reader holds: {:?}",
            client.swarm_stats()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// GETs the nodes have received for the file's records.
async fn node_gets(testnet: &MiniTestnet, client: &Client, data_map: &DataMap) -> usize {
    testnet.flush_chunk_gets(client.network().node()).await;
    data_map
        .infos()
        .iter()
        .map(|info| {
            testnet
                .chunk_gets_received(&info.dst_hash.0)
                .iter()
                .sum::<usize>()
        })
        .sum()
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_second_reader_is_served_by_the_first_instead_of_the_nodes() {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    // Records small enough to cross the link well inside a read's head start
    // on a slow runner. In a debug build the link's encryption is
    // unoptimised Rust and the nodes' is native code: a 2.7 MiB record took
    // the link 1.3 s of the 2 s a read gives it on a laptop, and longer than
    // that on CI runners, where the nodes then served every record.
    let (data, data_map) = upload(&testnet, 3 * RECORD_BYTES).await;
    let records = data_map.infos().len() as u64;
    let out = TempDir::new().expect("temp dir");

    // The first reader finds nobody else in the room: the nodes serve it.
    let first = reader(&testnet, 4);
    assert_eq!(download(&first, &data_map, out.path(), "first").await, data);
    assert_eq!(first.swarm_stats().records_from_peers, 0);

    // The second meets the first, which serves the records it is asked for:
    // a link takes one ask and one more read waiting for its turn, and
    // further reads at once go to the nodes rather than queue behind it.
    let second = reader(&testnet, 5);
    second.join_swarm(&data_map).await.expect("join");
    until_linked(&[&first, &second]).await;
    until_offered(&second, &data_map).await;
    let before = node_gets(&testnet, &second, &data_map).await;
    assert_eq!(
        download(&second, &data_map, out.path(), "second").await,
        data
    );
    let after = node_gets(&testnet, &second, &data_map).await;

    let second_stats = second.swarm_stats();
    let from_peers = second_stats.records_from_peers;
    // Records come from the first; a busy machine may let the nodes win the
    // race for some, as a read allows (the race itself is timed in the unit
    // tests, on a paused clock).
    assert!(from_peers >= 1, "{second_stats:?}");
    assert_eq!(second_stats.records_rejected, 0);
    // Every record the first did not send was asked of the nodes.
    let from_nodes = usize::try_from(records - from_peers).unwrap_or(usize::MAX);
    assert!(
        after - before >= from_nodes,
        "{second_stats:?}, {} node GETs",
        after - before
    );
    // A record sent whole on a link closed as a duplicate is served but not
    // read.
    assert!(first.swarm_stats().records_served >= from_peers);
    assert!(first.swarm_stats().bytes_served >= second_stats.bytes_from_peers);
    // Whole records, each sent in several pieces.
    assert!(second_stats.bytes_from_peers >= from_peers * RECORD_BYTES as u64);

    // A record a peer sent says nothing of whether the network stores it:
    // checking each record asks the nodes for every one the first sent,
    // while those the nodes sent are known.
    let before_checks = node_gets(&testnet, &second, &data_map).await;
    for info in data_map.infos() {
        let exists = second.chunk_exists(&info.dst_hash.0).await;
        assert!(matches!(exists, Ok(true)), "{exists:?}");
    }
    let checks = node_gets(&testnet, &second, &data_map).await - before_checks;
    assert!(
        checks >= usize::try_from(from_peers).unwrap_or(usize::MAX),
        "{checks} node GETs to check {from_peers} records from a peer"
    );

    drop((first, second));
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn a_reader_whose_peers_left_is_served_by_the_nodes() {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let (data, data_map) = upload(&testnet, 2 * 1024 * 1024).await;
    let out = TempDir::new().expect("temp dir");

    let first = reader(&testnet, 4);
    assert_eq!(download(&first, &data_map, out.path(), "first").await, data);
    let second = reader(&testnet, 5);
    second.join_swarm(&data_map).await.expect("join");
    until_linked(&[&first, &second]).await;

    // The first reader goes away; the nodes are the last seed.
    drop(first);
    let deadline = tokio::time::Instant::now() + MEET_TIMEOUT;
    while second.swarm_stats().links > 0 {
        assert!(tokio::time::Instant::now() < deadline, "the link stayed up");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        download(&second, &data_map, out.path(), "second").await,
        data
    );
    assert_eq!(second.swarm_stats().records_from_peers, 0);

    drop(second);
    testnet.teardown().await;
}

#[tokio::test(flavor = "multi_thread")]
#[serial]
async fn readers_of_different_files_do_not_meet() {
    let testnet = MiniTestnet::start(DEFAULT_NODE_COUNT).await;
    let (_, one) = upload(&testnet, 1024 * 1024).await;
    let (_, other) = upload(&testnet, 1024 * 1024 + 1).await;

    let first = reader(&testnet, 4);
    let second = reader(&testnet, 5);
    first.join_swarm(&one).await.expect("join");
    second.join_swarm(&other).await.expect("join");
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(first.swarm_stats().links, 0);
    assert_eq!(second.swarm_stats().links, 0);
    assert_eq!(first.swarm_stats().rooms, 1);

    drop((first, second));
    testnet.teardown().await;
}
