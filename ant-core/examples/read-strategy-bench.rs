//! V2-1358: compare chunk read strategies on the live network.
//!
//! Reads each listed chunk once, with one strategy per chunk, interleaving the
//! strategies in shuffled blocks so all of them see the same network and the
//! same warming client. Each read is written as one JSON line.
//!
//! ```bash
//! cargo run --release --example read-strategy-bench -- \
//!     --addresses chunks.txt --out reads.jsonl \
//!     --strategies baseline,progress,combined --seed 1
//! ```
//!
//! `chunks.txt` holds one hex chunk address per line. No wallet or payment.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::print_stdout)]

use ant_core::data::client::read_bench::{ReadStrategy, ReadTrace};
use ant_core::data::{Client, ClientConfig};
use rand::rngs::StdRng;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Time for the routing table to fill before the first read.
const DEFAULT_WARMUP: Duration = Duration::from_secs(10);
/// A read that runs longer is recorded as abandoned.
const READ_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_STRATEGIES: &str = "baseline,progress,eager,combined";

struct Args {
    addresses: PathBuf,
    out: PathBuf,
    strategies: Vec<ReadStrategy>,
    seed: u64,
    limit: Option<usize>,
    warmup: Duration,
}

impl Args {
    fn parse() -> Self {
        let mut args = std::env::args().skip(1);
        let mut addresses = None;
        let mut out = None;
        let mut strategies = DEFAULT_STRATEGIES.to_string();
        let mut seed = 0;
        let mut limit = None;
        let mut warmup = DEFAULT_WARMUP;
        while let Some(arg) = args.next() {
            let mut value = || args.next().expect("flag needs a value");
            match arg.as_str() {
                "--addresses" => addresses = Some(PathBuf::from(value())),
                "--out" => out = Some(PathBuf::from(value())),
                "--strategies" => strategies = value(),
                "--seed" => seed = value().parse().expect("--seed is an integer"),
                "--limit" => limit = Some(value().parse().expect("--limit is an integer")),
                "--warmup-secs" => {
                    warmup = Duration::from_secs(value().parse().expect("--warmup-secs"));
                }
                other => panic!("unknown flag {other}"),
            }
        }
        Self {
            addresses: addresses.expect("--addresses is required"),
            out: out.expect("--out is required"),
            strategies: strategies
                .split(',')
                .map(|name| ReadStrategy::parse(name).expect("unknown strategy"))
                .collect(),
            seed,
            limit,
            warmup,
        }
    }
}

#[derive(Serialize)]
struct Line<'a> {
    seq: usize,
    seed: u64,
    started_unix_ms: u128,
    timed_out: bool,
    strategy: ReadStrategy,
    address: &'a str,
    trace: Option<&'a ReadTrace>,
}

fn unix_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let args = Args::parse();

    let mut addresses: Vec<String> = std::fs::read_to_string(&args.addresses)?
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect();
    let mut rng = StdRng::seed_from_u64(args.seed);
    addresses.shuffle(&mut rng);
    if let Some(limit) = args.limit {
        addresses.truncate(limit);
    }

    let seeds = ant_core::network_defaults::bundled_bootstrap_seeds()?;
    let config = ClientConfig {
        ipv6: false,
        ..ClientConfig::default()
    };
    let client = Client::connect_multiaddrs(&seeds.quic, config).await?;
    eprintln!("connected; warming up for {:?}", args.warmup);
    tokio::time::sleep(args.warmup).await;

    let mut out = std::fs::File::create(&args.out)?;
    let mut block = args.strategies.clone();
    for (seq, address_hex) in addresses.iter().enumerate() {
        let position = seq % block.len();
        if position == 0 {
            block.shuffle(&mut rng);
        }
        let strategy = block[position];
        let address: [u8; 32] = hex::decode(address_hex)?
            .try_into()
            .map_err(|_| "address is not 32 bytes")?;
        let started_unix_ms = unix_ms();
        let trace = tokio::time::timeout(READ_TIMEOUT, client.bench_chunk_read(&address, strategy))
            .await
            .ok();
        match &trace {
            Some(trace) => eprintln!(
                "{seq:4} {strategy:?} found={} total={}ms lookup={:?} gets={}",
                trace.found,
                trace.total_ms,
                trace.lookup_ms,
                trace.attempts.len()
            ),
            None => eprintln!("{seq:4} {strategy:?} timed out"),
        }
        let line = Line {
            seq,
            seed: args.seed,
            started_unix_ms,
            timed_out: trace.is_none(),
            trace: trace.as_ref(),
            strategy,
            address: address_hex,
        };
        serde_json::to_writer(&mut out, &line)?;
        writeln!(out)?;
        out.flush()?;
    }
    Ok(())
}
