//! V2-1358 read-strategy bench on the browser lookup. Test-utils builds only.
//!
//! The browser baseline already turns lookup progress into early GETs, so the
//! native `Progress` strategy is the browser baseline. `Eager` additionally
//! offers each peer as the lookup queries it.

use super::{shared, BrowserNetworkClient, ProgressReporter};
use crate::browser::protocol::BrowserNode;
use crate::data::client::read_bench::{
    elapsed_ms, LookupEvent, PeerAddrs, ReadStrategy, ReadTrace, Recorder, DUPLICATE_DRAIN_TIMEOUT,
};
use crate::data::error::Error;
use tokio::sync::mpsc;
use wasm_bindgen::prelude::*;
use web_time::Instant;

/// Reports the browser lookup's queries and answers to a bench read.
#[derive(Clone)]
pub(super) struct BenchHook {
    started: Instant,
    events: mpsc::UnboundedSender<LookupEvent>,
}

fn peer_addrs(node: &BrowserNode) -> Option<PeerAddrs> {
    shared::peer_record(node)
        .ok()
        .map(|record| (record.peer_id, record.addresses_by_priority()))
}

impl BenchHook {
    pub(super) fn query(&self, round: usize, node: &BrowserNode) {
        if let Some(peer) = peer_addrs(node) {
            let _ = self.events.send(LookupEvent::Query {
                round,
                peer,
                at_ms: elapsed_ms(self.started),
            });
        }
    }

    pub(super) fn response(&self, round: usize, responder: &BrowserNode, nodes: &[BrowserNode]) {
        if let Some(responder) = peer_addrs(responder) {
            let _ = self.events.send(LookupEvent::Response {
                round,
                responder,
                candidates: nodes.iter().filter_map(peer_addrs).collect(),
                at_ms: elapsed_ms(self.started),
            });
        }
    }
}

#[wasm_bindgen(js_class = BrowserNetworkClient)]
impl BrowserNetworkClient {
    /// Read one chunk with `strategy` (`baseline` or `combined`), bypassing
    /// the chunk cache, and return its trace as JSON.
    #[wasm_bindgen(js_name = benchChunkRead)]
    pub async fn bench_chunk_read(&self, address: &str, strategy: &str) -> Result<String, JsValue> {
        let strategy = ReadStrategy::parse(strategy)
            .filter(|strategy| *strategy != ReadStrategy::Progress)
            .ok_or_else(|| JsValue::from_str("the browser has no separate progress strategy"))?;
        let address: [u8; 32] = hex::decode(address)
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .ok_or_else(|| JsValue::from_str("address must be 32 hex bytes"))?;
        let trace = match strategy {
            ReadStrategy::Baseline => self.bench_baseline(&address).await,
            ReadStrategy::Eager => self.bench_eager(&address).await,
            _ => self.bench_lookup_driven(&address, strategy).await,
        };
        serde_json::to_string(&trace).map_err(|error| JsValue::from_str(&error.to_string()))
    }
}

impl BrowserNetworkClient {
    async fn bench_baseline(&self, address: &[u8; 32]) -> ReadTrace {
        let recorder = Recorder::new();
        let count = self.shared.config().close_group_size;
        let result = self
            .shared
            .bench_progressive_read(address, &recorder, |progress| {
                self.shared
                    .network()
                    .find_read_peers(address, count, progress)
            })
            .await;
        let total_ms = recorder.now_ms();
        recorder.finish(ReadStrategy::Baseline, address, &result, total_ms, None)
    }

    /// The baseline, also offering each peer as the lookup queries it.
    async fn bench_eager(&self, address: &[u8; 32]) -> ReadTrace {
        let recorder = Recorder::new();
        let count = self.shared.config().close_group_size;
        let result = self
            .shared
            .bench_progressive_read(address, &recorder, |progress| async move {
                let (events, mut received) = mpsc::unbounded_channel();
                *self.inner.bench_hook.borrow_mut() = Some(BenchHook {
                    started: recorder.started,
                    events,
                });
                let lookup =
                    self.shared
                        .network()
                        .find_read_peers(address, count, progress.clone());
                tokio::pin!(lookup);
                let result = loop {
                    tokio::select! {
                        biased;
                        Some(event) = received.recv() => {
                            if let LookupEvent::Query { peer, .. } = event {
                                progress.offer(vec![peer]);
                            }
                        }
                        result = &mut lookup => break result,
                    }
                };
                *self.inner.bench_hook.borrow_mut() = None;
                result
            })
            .await;
        let total_ms = recorder.now_ms();
        recorder.finish(ReadStrategy::Eager, address, &result, total_ms, None)
    }

    async fn bench_lookup_driven(&self, address: &[u8; 32], strategy: ReadStrategy) -> ReadTrace {
        let recorder = Recorder::new();
        let count = self.shared.config().close_group_size;
        let (events, received) = mpsc::unbounded_channel();
        *self.inner.bench_hook.borrow_mut() = Some(BenchHook {
            started: recorder.started,
            events,
        });
        let target = hex::encode(address);
        let lookup = async {
            let result = self
                .inner
                .find_closest_with_progress(&target, &ProgressReporter::default(), count, None)
                .await
                .map_err(Error::Network)?;
            result
                .nodes
                .iter()
                .map(|node| shared::peer_record(node).map(|node| (node.peer_id, node.addresses)))
                .collect()
        };
        let trace = self
            .shared
            .bench_lookup_read(
                address,
                strategy,
                &recorder,
                lookup,
                received,
                DUPLICATE_DRAIN_TIMEOUT,
            )
            .await;
        *self.inner.bench_hook.borrow_mut() = None;
        trace
    }
}
