//! Benchmark the quoting phase of both single-node and merkle uploads against
//! the live Autonomi network. Measures per-stage latency so we can tell
//! whether the bottleneck is `find_closest_peers`, per-peer quote RPCs, or
//! something else.
//!
//! No payment, no wallet, no EVM. Only sends `ChunkQuoteRequest` /
//! `MerkleCandidateQuoteRequest` to real peers.
//!
//! # Usage
//!
//! ```bash
//! # Bench both modes, 10 reps each, against bootstrap peers in
//! # resources/bootstrap_peers.toml.
//! cargo run --release --example bench-quoting -- --reps 10
//!
//! # Only single-node quoting:
//! cargo run --release --example bench-quoting -- --mode normal --reps 10
//!
//! # Only merkle, and stress-test N concurrent midpoint lookups per rep:
//! cargo run --release --example bench-quoting -- --mode merkle --reps 5 --concurrency 16
//! ```
//!
//! Output: a table to stdout plus (optional) JSON when `--json-out <path>` is given.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use ant_core::data::{Client, ClientConfig};
use ant_protocol::evm::{MerklePaymentCandidateNode, PaymentQuote, CANDIDATES_PER_POOL};
use ant_protocol::transport::PeerId;
use ant_protocol::{
    compute_address, send_and_await_chunk_response, ChunkMessage, ChunkMessageBody,
    ChunkQuoteRequest, ChunkQuoteResponse, MerkleCandidateQuoteRequest,
    MerkleCandidateQuoteResponse, CLOSE_GROUP_SIZE,
};
use futures::stream::{FuturesUnordered, StreamExt};
use rand::Rng;
use serde::Serialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Normal,
    Merkle,
    /// Drives `client.prepare_chunk_payment` over many random chunks
    /// to exercise the adaptive controller's `quote` channel.
    /// Dumps the per-rep cap progression so we can see slow-start.
    Batch,
    /// Drives `client.chunk_get` over many random addresses through
    /// the same buffer_unordered shape `data_download` uses, so we
    /// can compare main's static `quote_concurrency` cap against
    /// feat's adaptive controller on the download hot path. No
    /// payment needed; "not found" responses still exercise the
    /// full DHT lookup and per-peer GET RPC pipeline.
    Download,
    Both,
}

impl Mode {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" | "single" => Some(Self::Normal),
            "merkle" => Some(Self::Merkle),
            "batch" => Some(Self::Batch),
            "download" => Some(Self::Download),
            "both" => Some(Self::Both),
            _ => None,
        }
    }
}

struct Args {
    mode: Mode,
    reps: usize,
    /// For merkle stress test: how many midpoint lookups to fire concurrently
    /// per rep (mirrors the 16-pool real upload path).
    concurrency: usize,
    /// Number of chunks per rep in `Batch` mode. Drives the
    /// adaptive controller's `quote` channel through `prepare_chunk_payment`.
    chunks_per_rep: usize,
    /// Override the buffer_unordered cap used in `Batch` mode. If
    /// unset, falls back to whatever the runtime cap is (controller
    /// on adaptive branch; `ClientConfig::quote_concurrency` static
    /// elsewhere). Set this to compare both branches at identical
    /// fan-out.
    batch_cap_override: Option<usize>,
    /// Hard per-rep cap. A rep that exceeds this is recorded as `ok=false`
    /// with `(rep timeout)` noted. Prevents a single hung Kademlia lookup
    /// from blocking the bench for hours.
    rep_timeout_secs: u64,
    json_out: Option<PathBuf>,
    bootstrap: Vec<SocketAddr>,
    tag: String,
}

impl Args {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let mut mode = Mode::Both;
        let mut reps = 5usize;
        let mut concurrency = 1usize;
        let mut chunks_per_rep = 64usize;
        let mut batch_cap_override: Option<usize> = None;
        let mut rep_timeout_secs = 180u64;
        let mut json_out = None;
        let mut bootstrap_cli: Vec<SocketAddr> = Vec::new();
        let mut tag = "unknown".to_string();
        while let Some(a) = args.next() {
            match a.as_str() {
                "--mode" => {
                    let v = args.next().expect("--mode needs a value");
                    mode = Mode::parse(&v).expect("--mode must be normal|merkle|batch|both");
                }
                "--reps" => {
                    reps = args
                        .next()
                        .expect("--reps needs a value")
                        .parse()
                        .expect("int");
                }
                "--concurrency" => {
                    concurrency = args
                        .next()
                        .expect("--concurrency needs a value")
                        .parse()
                        .expect("int");
                }
                "--chunks-per-rep" => {
                    chunks_per_rep = args
                        .next()
                        .expect("--chunks-per-rep needs a value")
                        .parse()
                        .expect("int");
                }
                "--batch-cap" => {
                    let v: usize = args
                        .next()
                        .expect("--batch-cap needs a value")
                        .parse()
                        .expect("int");
                    batch_cap_override = Some(v);
                }
                "--rep-timeout-secs" => {
                    rep_timeout_secs = args
                        .next()
                        .expect("--rep-timeout-secs needs a value")
                        .parse()
                        .expect("int");
                }
                "--json-out" => {
                    json_out = Some(PathBuf::from(
                        args.next().expect("--json-out needs a value"),
                    ));
                }
                "--bootstrap" => {
                    let v = args.next().expect("--bootstrap needs a value");
                    for part in v.split(',') {
                        bootstrap_cli.push(part.parse().expect("bootstrap must be ip:port"));
                    }
                }
                "--tag" => {
                    tag = args.next().expect("--tag needs a value");
                }
                other => panic!("unknown arg: {other}"),
            }
        }

        let bootstrap = if !bootstrap_cli.is_empty() {
            bootstrap_cli
        } else {
            load_default_bootstrap()
        };

        Self {
            mode,
            reps,
            concurrency,
            chunks_per_rep,
            batch_cap_override,
            rep_timeout_secs,
            json_out,
            bootstrap,
            tag,
        }
    }
}

fn load_default_bootstrap() -> Vec<SocketAddr> {
    // Prefer the file shipped with the repo so the bench is self-contained.
    let repo_file = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(|p| p.join("resources/bootstrap_peers.toml"))
        .expect("parent");
    if let Ok(text) = std::fs::read_to_string(&repo_file) {
        let parsed: toml::Value = toml::from_str(&text).expect("valid toml");
        if let Some(list) = parsed.get("peers").and_then(toml::Value::as_array) {
            return list
                .iter()
                .filter_map(toml::Value::as_str)
                .filter_map(|s: &str| s.parse::<SocketAddr>().ok())
                .collect();
        }
    }
    // Fall back to the platform config dir.
    match ant_core::config::load_bootstrap_peers() {
        Ok(Some(peers)) if !peers.is_empty() => peers,
        _ => panic!(
            "no bootstrap peers: pass --bootstrap ip:port[,ip:port...] or ensure \
             resources/bootstrap_peers.toml is readable."
        ),
    }
}

// --------------------------------------------------------------------------
// Timing record + stats
// --------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize)]
struct Rep {
    rep: usize,
    stages_ms: Vec<(String, u128)>,
    total_ms: u128,
    ok: bool,
    note: String,
}

#[derive(Serialize)]
struct ModeReport {
    mode: String,
    reps: Vec<Rep>,
    summary: Vec<StageSummary>,
}

#[derive(Serialize, Clone)]
struct StageSummary {
    stage: String,
    p50_ms: u128,
    p95_ms: u128,
    max_ms: u128,
    mean_ms: u128,
    count: usize,
}

fn percentile(sorted: &[u128], p: f64) -> u128 {
    if sorted.is_empty() {
        return 0;
    }
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn summarise(reps: &[Rep]) -> Vec<StageSummary> {
    use std::collections::BTreeMap;
    let mut by_stage: BTreeMap<String, Vec<u128>> = BTreeMap::new();
    for rep in reps.iter().filter(|r| r.ok) {
        for (stage, ms) in &rep.stages_ms {
            by_stage.entry(stage.clone()).or_default().push(*ms);
        }
        by_stage
            .entry("total".to_string())
            .or_default()
            .push(rep.total_ms);
    }
    let mut out = Vec::new();
    for (stage, mut vals) in by_stage {
        vals.sort_unstable();
        let count = vals.len();
        let mean_ms = if count == 0 {
            0
        } else {
            #[allow(clippy::cast_possible_truncation)]
            let m = (vals.iter().sum::<u128>()) / (count as u128);
            m
        };
        out.push(StageSummary {
            stage,
            p50_ms: percentile(&vals, 0.5),
            p95_ms: percentile(&vals, 0.95),
            max_ms: *vals.last().unwrap_or(&0),
            mean_ms,
            count,
        });
    }
    out
}

// --------------------------------------------------------------------------
// Single-rep runners
// --------------------------------------------------------------------------

static REQ_ID: AtomicU64 = AtomicU64::new(1);
fn next_request_id() -> u64 {
    REQ_ID.fetch_add(1, Ordering::Relaxed)
}

async fn bench_normal_once(client: &Client, rep: usize) -> Rep {
    let mut stages: Vec<(String, u128)> = Vec::new();
    let total_t0 = Instant::now();

    // 1. Random target address.
    let mut content = [0u8; 64];
    rand::thread_rng().fill(&mut content[..]);
    let address = compute_address(&content);

    // 2. find_closest_peers (same call single-node quoting uses).
    let t0 = Instant::now();
    let peers = match client
        .network()
        .find_closest_peers(&address, CLOSE_GROUP_SIZE * 2)
        .await
    {
        Ok(p) => p,
        Err(e) => {
            return Rep {
                rep,
                stages_ms: stages,
                total_ms: total_t0.elapsed().as_millis(),
                ok: false,
                note: format!("find_closest_peers failed: {e}"),
            };
        }
    };
    stages.push(("find_closest_peers".into(), t0.elapsed().as_millis()));
    stages.push((
        "find_closest_peers_returned_count".into(),
        peers.len() as u128,
    ));

    if peers.len() < CLOSE_GROUP_SIZE {
        return Rep {
            rep,
            stages_ms: stages,
            total_ms: total_t0.elapsed().as_millis(),
            ok: false,
            note: format!("only {} peers returned (< {CLOSE_GROUP_SIZE})", peers.len()),
        };
    }

    // 3. Concurrent quote RPCs.
    let t1 = Instant::now();
    let per_peer_timeout = Duration::from_secs(client.config().quote_timeout_secs);
    let node = client.network().node();
    let mut futs = FuturesUnordered::new();
    for (peer_id, addrs) in &peers {
        let request_id = next_request_id();
        let msg = ChunkMessage {
            request_id,
            body: ChunkMessageBody::QuoteRequest(ChunkQuoteRequest {
                address,
                data_size: 1024,
                data_type: 0,
            }),
        };
        let bytes = match msg.encode() {
            Ok(b) => b,
            Err(_) => continue,
        };
        let peer_id_clone = *peer_id;
        let addrs_clone = addrs.clone();
        let node_clone = node.clone();
        futs.push(async move {
            let t = Instant::now();
            let res: Result<PaymentQuote, String> = send_and_await_chunk_response(
                &node_clone,
                &peer_id_clone,
                bytes,
                request_id,
                per_peer_timeout,
                &addrs_clone,
                |body| match body {
                    ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Success {
                        quote, ..
                    }) => match rmp_serde::from_slice::<PaymentQuote>(&quote) {
                        Ok(q) => Some(Ok(q)),
                        Err(e) => Some(Err(format!("deser: {e}"))),
                    },
                    ChunkMessageBody::QuoteResponse(ChunkQuoteResponse::Error(e)) => {
                        Some(Err(format!("err: {e}")))
                    }
                    _ => None,
                },
                |e| format!("send: {e}"),
                || "timeout".to_string(),
            )
            .await;
            (peer_id_clone, t.elapsed().as_millis(), res.is_ok())
        });
    }

    // Collect with a generous overall cap (any one slow peer shouldn't hang the bench).
    let mut successes = 0usize;
    let mut per_peer_ms: Vec<u128> = Vec::new();
    let overall_cap = Duration::from_secs(90);
    let collect_res = tokio::time::timeout(overall_cap, async {
        while let Some((_pid, ms, ok)) = futs.next().await {
            per_peer_ms.push(ms);
            if ok {
                successes += 1;
            }
        }
    })
    .await;

    stages.push(("quote_rpcs_total".into(), t1.elapsed().as_millis()));
    if !per_peer_ms.is_empty() {
        let mut s = per_peer_ms.clone();
        s.sort_unstable();
        stages.push(("quote_rpc_p50".into(), percentile(&s, 0.5)));
        stages.push(("quote_rpc_p95".into(), percentile(&s, 0.95)));
        stages.push(("quote_rpc_max".into(), *s.last().unwrap_or(&0)));
    }

    let ok = collect_res.is_ok() && successes >= CLOSE_GROUP_SIZE;
    Rep {
        rep,
        stages_ms: stages,
        total_ms: total_t0.elapsed().as_millis(),
        ok,
        note: format!(
            "{successes}/{} quotes{}",
            peers.len(),
            if collect_res.is_err() {
                " (overall timeout)"
            } else {
                ""
            }
        ),
    }
}

async fn bench_merkle_once(client: &Client, rep: usize, concurrency: usize) -> Rep {
    let mut stages: Vec<(String, u128)> = Vec::new();
    let total_t0 = Instant::now();

    // 1. Fabricate `concurrency` random pool-midpoint addresses. Honest
    //    uploads derive these from the tree but the network-facing lookup
    //    doesn't care about provenance — any 32-byte target is valid.
    let mut targets: Vec<[u8; 32]> = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        targets.push(rand::thread_rng().gen());
    }
    let merkle_payment_timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // 2. Fire all `find_closest_peers` calls concurrently (mirrors real
    //    build_candidate_pools which runs one per midpoint in parallel).
    let t0 = Instant::now();
    let mut find_futs = FuturesUnordered::new();
    for target in targets {
        let net = client.network();
        find_futs.push(async move {
            let t = Instant::now();
            let res = net
                .find_closest_peers(&target, CANDIDATES_PER_POOL * 2)
                .await;
            (target, t.elapsed().as_millis(), res)
        });
    }

    let mut find_ms: Vec<u128> = Vec::new();
    type PoolPeers = Vec<(PeerId, Vec<ant_protocol::transport::MultiAddr>)>;
    let mut pool_peer_sets: Vec<([u8; 32], PoolPeers)> = Vec::new();
    while let Some((target, ms, res)) = find_futs.next().await {
        find_ms.push(ms);
        match res {
            Ok(peers) => pool_peer_sets.push((target, peers)),
            Err(e) => {
                return Rep {
                    rep,
                    stages_ms: stages,
                    total_ms: total_t0.elapsed().as_millis(),
                    ok: false,
                    note: format!("find_closest_peers failed: {e}"),
                };
            }
        }
    }
    stages.push(("find_closest_peers_total".into(), t0.elapsed().as_millis()));
    if !find_ms.is_empty() {
        let mut s = find_ms.clone();
        s.sort_unstable();
        stages.push(("find_closest_peers_p50".into(), percentile(&s, 0.5)));
        stages.push(("find_closest_peers_p95".into(), percentile(&s, 0.95)));
        stages.push(("find_closest_peers_max".into(), *s.last().unwrap_or(&0)));
    }

    // 3. Concurrent MerkleCandidateQuoteRequest per peer, per pool.
    let t1 = Instant::now();
    let per_peer_timeout = Duration::from_secs(client.config().quote_timeout_secs);
    let node = client.network().node();
    let mut quote_futs = FuturesUnordered::new();
    let mut expected = 0usize;
    for (target, peers) in &pool_peer_sets {
        for (peer_id, addrs) in peers {
            let request_id = next_request_id();
            let msg = ChunkMessage {
                request_id,
                body: ChunkMessageBody::MerkleCandidateQuoteRequest(MerkleCandidateQuoteRequest {
                    address: *target,
                    data_type: 0,
                    data_size: 1024,
                    merkle_payment_timestamp,
                }),
            };
            let bytes = match msg.encode() {
                Ok(b) => b,
                Err(_) => continue,
            };
            let peer_id_clone = *peer_id;
            let addrs_clone = addrs.clone();
            let node_clone = node.clone();
            expected += 1;
            quote_futs.push(async move {
                let t = Instant::now();
                let res: Result<MerklePaymentCandidateNode, String> =
                    send_and_await_chunk_response(
                        &node_clone,
                        &peer_id_clone,
                        bytes,
                        request_id,
                        per_peer_timeout,
                        &addrs_clone,
                        |body| match body {
                            ChunkMessageBody::MerkleCandidateQuoteResponse(
                                MerkleCandidateQuoteResponse::Success { candidate_node },
                            ) => match rmp_serde::from_slice::<MerklePaymentCandidateNode>(
                                &candidate_node,
                            ) {
                                Ok(n) => Some(Ok(n)),
                                Err(e) => Some(Err(format!("deser: {e}"))),
                            },
                            ChunkMessageBody::MerkleCandidateQuoteResponse(
                                MerkleCandidateQuoteResponse::Error(e),
                            ) => Some(Err(format!("err: {e}"))),
                            _ => None,
                        },
                        |e| format!("send: {e}"),
                        || "timeout".to_string(),
                    )
                    .await;
                (peer_id_clone, t.elapsed().as_millis(), res.is_ok())
            });
        }
    }

    let mut quote_ms: Vec<u128> = Vec::new();
    let mut successes = 0usize;
    let overall_cap = Duration::from_secs(120);
    let collect_res = tokio::time::timeout(overall_cap, async {
        while let Some((_pid, ms, ok)) = quote_futs.next().await {
            quote_ms.push(ms);
            if ok {
                successes += 1;
            }
        }
    })
    .await;

    stages.push(("quote_rpcs_total".into(), t1.elapsed().as_millis()));
    if !quote_ms.is_empty() {
        let mut s = quote_ms.clone();
        s.sort_unstable();
        stages.push(("quote_rpc_p50".into(), percentile(&s, 0.5)));
        stages.push(("quote_rpc_p95".into(), percentile(&s, 0.95)));
        stages.push(("quote_rpc_max".into(), *s.last().unwrap_or(&0)));
    }

    let min_per_pool = CANDIDATES_PER_POOL.min(
        pool_peer_sets
            .iter()
            .map(|(_, p)| p.len())
            .min()
            .unwrap_or(0),
    );
    let need = min_per_pool * pool_peer_sets.len();
    let ok = collect_res.is_ok() && successes >= need.max(1);
    Rep {
        rep,
        stages_ms: stages,
        total_ms: total_t0.elapsed().as_millis(),
        ok,
        note: format!(
            "{successes}/{expected} candidate quotes across {} pools{}",
            pool_peer_sets.len(),
            if collect_res.is_err() {
                " (overall timeout)"
            } else {
                ""
            }
        ),
    }
}

// --------------------------------------------------------------------------
// Batch quote bench — drives the adaptive controller's `quote` channel.
// --------------------------------------------------------------------------

/// Times preparing payment for `chunks_per_rep` random chunks via
/// `client.prepare_chunk_payment`. This goes through the controller's
/// `quote` channel (sized by `controller.quote.current()` in
/// `Client::batch_upload_chunks`), so the cap actually moves. We
/// record the final cap for quote/store/fetch on each rep so the
/// JSON dump shows the AIMD trajectory.
async fn bench_batch_once(
    client: &Client,
    rep: usize,
    chunks_per_rep: usize,
    cap_override: Option<usize>,
) -> Rep {
    use bytes::Bytes;
    use futures::stream::StreamExt;

    let mut stages: Vec<(String, u128)> = Vec::new();
    let total_t0 = Instant::now();

    // Generate `chunks_per_rep` random chunks. Random content means
    // each address is fresh so we always go through the full quoting
    // path (no AlreadyStored shortcuts).
    let chunks: Vec<Bytes> = (0..chunks_per_rep)
        .map(|_| {
            let mut buf = vec![0u8; 4096];
            rand::thread_rng().fill(&mut buf[..]);
            Bytes::from(buf)
        })
        .collect();

    // Pipeline cap: explicit override takes precedence (so both
    // branches can be benched at identical fan-out for fair
    // comparison). Otherwise read from the controller.
    let cap_for_pipeline = cap_override.unwrap_or_else(|| quote_cap(client));
    stages.push(("quote_cap_before".into(), cap_for_pipeline as u128));

    let t_quote = Instant::now();
    let results: Vec<Result<_, _>> = futures::stream::iter(chunks)
        .map(|content| {
            let c = client;
            async move { c.prepare_chunk_payment(content).await }
        })
        .buffer_unordered(cap_for_pipeline)
        .collect()
        .await;
    let quote_ms = t_quote.elapsed().as_millis();
    stages.push(("quote_total_ms".into(), quote_ms));

    let mut ok_count = 0usize;
    let mut already_stored = 0usize;
    let mut err_count = 0usize;
    for r in &results {
        match r {
            Ok(Some(_)) => ok_count += 1,
            Ok(None) => already_stored += 1,
            Err(_) => err_count += 1,
        }
    }
    stages.push(("ok_count".into(), ok_count as u128));
    stages.push(("already_stored".into(), already_stored as u128));
    stages.push(("err_count".into(), err_count as u128));

    let cap_after = quote_cap(client);
    stages.push(("quote_cap_after".into(), cap_after as u128));

    // Per-chunk wall time; for buffer_unordered with fan-out N this
    // is dominated by the slowest peer in any one batch, so it's a
    // proxy for "how often did we hit the slow tail".
    let per_chunk_ms = if chunks_per_rep > 0 {
        quote_ms / chunks_per_rep as u128
    } else {
        0
    };
    stages.push(("quote_per_chunk_ms".into(), per_chunk_ms));

    Rep {
        rep,
        stages_ms: stages,
        total_ms: total_t0.elapsed().as_millis(),
        ok: err_count == 0 && ok_count + already_stored == chunks_per_rep,
        note: format!(
            "{ok_count} quoted, {already_stored} already-stored, {err_count} err; cap {cap_for_pipeline}->{cap_after}",
        ),
    }
}

/// Resolve the runtime quote concurrency cap from the adaptive
/// controller. Lives on the feat branch only.
fn quote_cap(client: &Client) -> usize {
    client.controller().quote.current()
}

/// Resolve the runtime fetch concurrency cap. On the adaptive
/// branch this reads the controller's fetch channel; on baseline
/// main it falls back to `ClientConfig::quote_concurrency` since
/// `data_download` historically used the quote knob for downloads.
fn fetch_cap(client: &Client) -> usize {
    client.controller().fetch.current()
}

// --------------------------------------------------------------------------
// Download bench — drives `chunk_get` over many random addresses to
// time the DHT lookup + GET RPC pipeline that `data_download` uses.
// No payment needed; "not found" responses still walk the same path.
// --------------------------------------------------------------------------

async fn bench_download_once(
    client: &Client,
    rep: usize,
    chunks_per_rep: usize,
    cap_override: Option<usize>,
) -> Rep {
    use futures::stream::StreamExt;

    let mut stages: Vec<(String, u128)> = Vec::new();
    let total_t0 = Instant::now();

    let addrs: Vec<[u8; 32]> = (0..chunks_per_rep)
        .map(|_| rand::thread_rng().gen())
        .collect();

    let cap = cap_override.unwrap_or_else(|| fetch_cap(client));
    stages.push(("fetch_cap_before".into(), cap as u128));

    let t = Instant::now();
    let results: Vec<Result<_, _>> = futures::stream::iter(addrs)
        .map(|addr| {
            let c = client;
            async move { c.chunk_get(&addr).await }
        })
        .buffer_unordered(cap)
        .collect()
        .await;
    let total_ms = t.elapsed().as_millis();
    stages.push(("get_total_ms".into(), total_ms));

    let mut found = 0usize;
    let mut not_found = 0usize;
    let mut err = 0usize;
    for r in &results {
        match r {
            Ok(Some(_)) => found += 1,
            Ok(None) => not_found += 1,
            Err(_) => err += 1,
        }
    }
    stages.push(("found".into(), found as u128));
    stages.push(("not_found".into(), not_found as u128));
    stages.push(("err".into(), err as u128));

    let cap_after = fetch_cap(client);
    stages.push(("fetch_cap_after".into(), cap_after as u128));

    let per_chunk_ms = if chunks_per_rep > 0 {
        total_ms / chunks_per_rep as u128
    } else {
        0
    };
    stages.push(("get_per_chunk_ms".into(), per_chunk_ms));

    Rep {
        rep,
        stages_ms: stages,
        total_ms: total_t0.elapsed().as_millis(),
        // Random addresses: ok=true means every call returned (with
        // not-found or content), no transport errors. We don't
        // require found>0 because the addresses are random.
        ok: err == 0,
        note: format!("{found} found, {not_found} not-found, {err} err; cap {cap}->{cap_after}",),
    }
}

// --------------------------------------------------------------------------
// Reporting
// --------------------------------------------------------------------------

fn print_report(label: &str, reps: &[Rep], summary: &[StageSummary]) {
    println!();
    println!("=== {label} ===");
    println!(
        "reps: {} total, {} ok",
        reps.len(),
        reps.iter().filter(|r| r.ok).count()
    );
    println!(
        "{:<30} {:>10} {:>10} {:>10} {:>10} {:>6}",
        "stage", "p50_ms", "p95_ms", "max_ms", "mean_ms", "n"
    );
    for s in summary {
        println!(
            "{:<30} {:>10} {:>10} {:>10} {:>10} {:>6}",
            s.stage, s.p50_ms, s.p95_ms, s.max_ms, s.mean_ms, s.count
        );
    }
    println!();
    println!("per-rep notes:");
    for r in reps {
        println!(
            "  rep {:>2}: ok={:<5} total={:>6}ms  {}",
            r.rep, r.ok, r.total_ms, r.note
        );
    }
}

// --------------------------------------------------------------------------
// Main
// --------------------------------------------------------------------------

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,bench_quoting=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    eprintln!(
        "bench-quoting: tag={} mode={:?} reps={} concurrency={} bootstrap={}",
        args.tag,
        args.mode,
        args.reps,
        args.concurrency,
        args.bootstrap.len()
    );

    let config = ClientConfig::default();
    let connect_t0 = Instant::now();
    let client = Client::connect(&args.bootstrap, config).await?;
    let connect_ms = connect_t0.elapsed().as_millis();
    eprintln!("connected in {connect_ms}ms");

    // Give the DHT a moment to populate the routing table before we start
    // hammering it. This matches real client behaviour — the CLI spends a
    // couple of seconds bootstrapping before the first upload.
    tokio::time::sleep(Duration::from_secs(3)).await;

    let mut full_report: Vec<(String, Vec<Rep>, Vec<StageSummary>)> = Vec::new();

    let rep_cap = Duration::from_secs(args.rep_timeout_secs);

    if matches!(args.mode, Mode::Normal | Mode::Both) {
        let mut reps = Vec::with_capacity(args.reps);
        for i in 0..args.reps {
            eprintln!("normal rep {}/{}...", i + 1, args.reps);
            let r = match tokio::time::timeout(rep_cap, bench_normal_once(&client, i + 1)).await {
                Ok(rep) => rep,
                Err(_) => Rep {
                    rep: i + 1,
                    stages_ms: vec![],
                    total_ms: rep_cap.as_millis(),
                    ok: false,
                    note: format!("(rep timeout {}s)", args.rep_timeout_secs),
                },
            };
            eprintln!(
                "  => rep {} done ok={} total={}ms  {}",
                r.rep, r.ok, r.total_ms, r.note
            );
            reps.push(r);
        }
        let summary = summarise(&reps);
        let label = format!("normal-quoting ({} reps, 1 address/rep)", args.reps);
        print_report(&label, &reps, &summary);
        full_report.push((label, reps, summary));
    }

    if matches!(args.mode, Mode::Merkle | Mode::Both) {
        let mut reps = Vec::with_capacity(args.reps);
        for i in 0..args.reps {
            eprintln!(
                "merkle rep {}/{} (concurrency={})...",
                i + 1,
                args.reps,
                args.concurrency
            );
            let r = match tokio::time::timeout(
                rep_cap,
                bench_merkle_once(&client, i + 1, args.concurrency),
            )
            .await
            {
                Ok(rep) => rep,
                Err(_) => Rep {
                    rep: i + 1,
                    stages_ms: vec![],
                    total_ms: rep_cap.as_millis(),
                    ok: false,
                    note: format!("(rep timeout {}s)", args.rep_timeout_secs),
                },
            };
            eprintln!(
                "  => rep {} done ok={} total={}ms  {}",
                r.rep, r.ok, r.total_ms, r.note
            );
            reps.push(r);
        }
        let summary = summarise(&reps);
        let label = format!(
            "merkle-quoting ({} reps, {} concurrent midpoint lookups/rep)",
            args.reps, args.concurrency
        );
        print_report(&label, &reps, &summary);
        full_report.push((label, reps, summary));
    }

    if matches!(args.mode, Mode::Batch | Mode::Both) {
        let mut reps = Vec::with_capacity(args.reps);
        for i in 0..args.reps {
            eprintln!(
                "batch rep {}/{} (chunks_per_rep={})...",
                i + 1,
                args.reps,
                args.chunks_per_rep
            );
            let r = match tokio::time::timeout(
                rep_cap,
                bench_batch_once(&client, i + 1, args.chunks_per_rep, args.batch_cap_override),
            )
            .await
            {
                Ok(rep) => rep,
                Err(_) => Rep {
                    rep: i + 1,
                    stages_ms: vec![],
                    total_ms: rep_cap.as_millis(),
                    ok: false,
                    note: format!("(rep timeout {}s)", args.rep_timeout_secs),
                },
            };
            eprintln!(
                "  => rep {} done ok={} total={}ms  {}",
                r.rep, r.ok, r.total_ms, r.note
            );
            reps.push(r);
        }
        let summary = summarise(&reps);
        let label = format!(
            "batch-quoting ({} reps, {} chunks/rep, controller-driven)",
            args.reps, args.chunks_per_rep
        );
        print_report(&label, &reps, &summary);
        full_report.push((label, reps, summary));
    }

    if matches!(args.mode, Mode::Download | Mode::Both) {
        let mut reps = Vec::with_capacity(args.reps);
        for i in 0..args.reps {
            eprintln!(
                "download rep {}/{} (chunks_per_rep={})...",
                i + 1,
                args.reps,
                args.chunks_per_rep
            );
            let r = match tokio::time::timeout(
                rep_cap,
                bench_download_once(&client, i + 1, args.chunks_per_rep, args.batch_cap_override),
            )
            .await
            {
                Ok(rep) => rep,
                Err(_) => Rep {
                    rep: i + 1,
                    stages_ms: vec![],
                    total_ms: rep_cap.as_millis(),
                    ok: false,
                    note: format!("(rep timeout {}s)", args.rep_timeout_secs),
                },
            };
            eprintln!(
                "  => rep {} done ok={} total={}ms  {}",
                r.rep, r.ok, r.total_ms, r.note
            );
            reps.push(r);
        }
        let summary = summarise(&reps);
        let label = format!(
            "download ({} reps, {} chunks/rep, controller-driven fetch)",
            args.reps, args.chunks_per_rep
        );
        print_report(&label, &reps, &summary);
        full_report.push((label, reps, summary));
    }

    if let Some(path) = args.json_out {
        let mode_reports: Vec<ModeReport> = full_report
            .iter()
            .map(|(label, reps, summary)| ModeReport {
                mode: label.clone(),
                reps: reps.clone(),
                summary: summary.clone(),
            })
            .collect();
        #[derive(Serialize)]
        struct FullJson {
            tag: String,
            bootstrap_count: usize,
            connect_ms: u128,
            reports: Vec<ModeReport>,
        }
        let body = FullJson {
            tag: args.tag.clone(),
            bootstrap_count: args.bootstrap.len(),
            connect_ms,
            reports: mode_reports,
        };
        std::fs::write(&path, serde_json::to_vec_pretty(&body)?)?;
        eprintln!("wrote JSON report to {}", path.display());
    }

    Ok(())
}
