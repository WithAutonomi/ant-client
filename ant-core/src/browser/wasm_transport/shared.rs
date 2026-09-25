// Copyright 2026 Saorsa Labs Limited
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Browser I/O adapter for the ordinary ant-core data client.

use super::*;
use crate::data::error::{Error as DataError, Result as DataResult};
use crate::data::network::BrowserNetwork;
use ant_protocol::transport::{DHTNode, MultiAddr, PeerId, WitnessedCloseGroup};
use ant_protocol::{ChunkMessage, ChunkMessageBody};
use futures::future::LocalBoxFuture;

pub(super) struct SharedNetworkAdapter {
    pub(super) inner: Rc<BrowserNetworkCore>,
    local_peer: PeerId,
    pub(super) sources: RefCell<lru::LruCache<LookupKey, BrowserNode>>,
    pub(super) payment_network: Option<BrowserPaymentNetwork>,
}

impl SharedNetworkAdapter {
    pub(super) fn new(inner: Rc<BrowserNetworkCore>) -> Self {
        Self {
            inner,
            local_peer: PeerId::random(),
            sources: RefCell::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(256).unwrap_or(std::num::NonZeroUsize::MIN),
            )),
            payment_network: None,
        }
    }
}

pub(super) use super::super::peer_records::{browser_record, peer_record};

impl BrowserNetwork for SharedNetworkAdapter {
    fn peer_id(&self) -> &PeerId {
        &self.local_peer
    }

    fn find_closest_peers<'a>(
        &'a self,
        target: &'a LookupKey,
        count: usize,
    ) -> LocalBoxFuture<'a, DataResult<Vec<(PeerId, Vec<MultiAddr>)>>> {
        Box::pin(async move {
            let result = self
                .inner
                .find_closest_with_count(&hex::encode(target), &ProgressReporter(None), count)
                .await
                .map_err(DataError::Network)?;
            result
                .nodes
                .iter()
                .map(|node| peer_record(node).map(|node| (node.peer_id, node.addresses)))
                .collect()
        })
    }

    fn find_read_peers<'a>(
        &'a self,
        target: &'a LookupKey,
        count: usize,
        progress: crate::data::network::ReadProgress,
    ) -> LocalBoxFuture<'a, DataResult<Vec<(PeerId, Vec<MultiAddr>)>>> {
        Box::pin(async move {
            let result = self
                .inner
                .find_closest_with_progress(
                    &hex::encode(target),
                    &ProgressReporter(None),
                    count,
                    Some(progress),
                )
                .await
                .map_err(DataError::Network)?;
            result
                .nodes
                .iter()
                .map(|node| peer_record(node).map(|node| (node.peer_id, node.addresses)))
                .collect()
        })
    }

    fn find_witnessed_close_group<'a>(
        &'a self,
        target: &'a LookupKey,
        count: usize,
        view_count: usize,
    ) -> LocalBoxFuture<'a, DataResult<WitnessedCloseGroup>> {
        Box::pin(async move {
            let target_hex = hex::encode(target);
            let mut lookup = self
                .inner
                .find_closest_with_count(&target_hex, &ProgressReporter(None), count)
                .await
                .map_err(DataError::Network)?;
            let initial_closest = lookup
                .nodes
                .iter()
                .map(peer_record)
                .collect::<DataResult<Vec<_>>>()?;
            // Native quote policy consumes self-inclusive views. Fetch missing
            // views without treating a failed witness as a successful response.
            let missing = lookup.nodes.iter().filter(|node| {
                parse_lookup_key(&node.peer_id, "peer ID")
                    .is_ok_and(|peer| !lookup.views.contains_key(&peer))
            });
            let responses = join_all(missing.map(|node| async {
                let peer = parse_lookup_key(&node.peer_id, "peer ID")?;
                let endpoint = node
                    .webrtc_direct
                    .as_ref()
                    .ok_or("missing WebRTC endpoint")?;
                let client = self.inner.pool.client(endpoint).await?;
                Ok::<_, String>((peer, client.find_node(&target_hex, view_count).await?))
            }))
            .await;
            for (peer, nodes) in responses.into_iter().flatten() {
                lookup.views.insert(peer, nodes);
            }
            if initial_closest.len() < count {
                return Err(DataError::Network(format!(
                    "witnessed close group initial lookup found {} peers, need {count}",
                    initial_closest.len()
                )));
            }
            let responder_views = initial_closest
                .iter()
                .filter_map(|responder| {
                    let nodes = lookup.views.get(responder.peer_id.as_bytes())?;
                    Some((
                        responder.peer_id,
                        nodes
                            .iter()
                            .filter_map(|node| peer_record(node).ok())
                            .collect(),
                    ))
                })
                .collect();
            Ok(
                ant_protocol::transport::client_routing::build_witnessed_close_group(
                    target,
                    count,
                    view_count,
                    initial_closest,
                    responder_views,
                ),
            )
        })
    }

    fn known_peers(&self) -> Vec<DHTNode> {
        let mut known = self
            .inner
            .routing
            .borrow()
            .values()
            .filter_map(|node| peer_record(&node.wire).ok())
            .collect::<Vec<_>>();
        for endpoint in &self.inner.seeds {
            if let Ok(parsed) = parse_webrtc_direct_multiaddr(&endpoint.multiaddr) {
                if let Ok(node) = peer_record(&BrowserNode {
                    address_record: None,
                    peer_record: None,
                    peer_id: parsed.peer_id,
                    native_addresses: Vec::new(),
                    reliability: 1.0,
                    webrtc_direct: Some(endpoint.clone()),
                }) {
                    if !known
                        .iter()
                        .any(|existing| existing.peer_id == node.peer_id)
                    {
                        known.push(node);
                    }
                }
            }
        }
        known
    }

    fn connected_read_peers(&self) -> Vec<PeerId> {
        self.inner
            .pool
            .clients
            .borrow()
            .values()
            .filter(|entry| entry.client.is_connected() && entry.client.hello.borrow().is_some())
            .filter_map(|entry| PeerId::from_hex(&entry.client.endpoint.peer_id).ok())
            .collect()
    }

    fn request<'a>(
        &'a self,
        peer: &'a PeerId,
        addrs: &'a [MultiAddr],
        request: ChunkMessage,
        timeout: Duration,
    ) -> LocalBoxFuture<'a, DataResult<ChunkMessage>> {
        Box::pin(async move {
            let endpoint = addrs
                .iter()
                .find(|addr| addr.is_webrtc_direct())
                .ok_or_else(|| DataError::Network("peer has no WebRTC endpoint".into()))?;
            let endpoint = BrowserEndpoint {
                multiaddr: endpoint.to_string(),
            };
            let parsed = parse_webrtc_direct_multiaddr(&endpoint.multiaddr)
                .map_err(|e| DataError::Network(e.to_string()))?;
            if parsed.peer_id != peer.to_hex() {
                return Err(DataError::Network("endpoint peer mismatch".into()));
            }
            // Bulk records cannot hold the discovery/quote lane's RPC lock.
            // The node already accepts two independently authenticated channels
            // on one association; application request/witness semantics stay intact.
            let admission = TransferDeadline::new(RPC_ADMISSION_TIMEOUT);
            // A pointer write is a paid write, and waits on its payment being
            // verified, so it takes the data lane as a chunk PUT does. A pointer
            // read returns one small record, like a quote, and stays on the RPC
            // lane.
            let client = if matches!(
                &request.body,
                ChunkMessageBody::GetRequest(_)
                    | ChunkMessageBody::PutRequest(_)
                    | ChunkMessageBody::PointerPutRequest(_)
            ) {
                self.inner
                    .pool
                    .data_client_before(&endpoint, admission)
                    .await
            } else {
                self.inner.pool.client_before(&endpoint, admission).await
            }
            .map_err(rpc_data_error)?;
            let client = client.authenticated().await.map_err(rpc_data_error)?;
            let hello = client
                .hello
                .borrow()
                .clone()
                .ok_or_else(|| DataError::Network("authenticated session required".into()))?;
            if !hello.capabilities.iter().any(|cap| cap == "chunk_protocol") {
                return Err(DataError::Network(
                    "node does not support shared ant-protocol RPC".into(),
                ));
            }
            // A node that predates browser pointers refuses them, so it is
            // not asked: its refusal would read as a failed peer.
            if matches!(
                request.body,
                ChunkMessageBody::PointerGetRequest(_) | ChunkMessageBody::PointerPutRequest(_)
            ) && !hello
                .capabilities
                .iter()
                .any(|cap| cap == POINTER_PROTOCOL_CAPABILITY)
            {
                return Err(DataError::Network(
                    "node does not support browser pointers".into(),
                ));
            }
            let payment_network = self.payment_network.clone();
            if matches!(
                request.body,
                ChunkMessageBody::QuoteRequest(_)
                    | ChunkMessageBody::QuoteRequestV2(_)
                    | ChunkMessageBody::MerkleCandidateQuoteRequest(_)
                    | ChunkMessageBody::MerkleCandidateQuoteRequestV2(_)
                    | ChunkMessageBody::PutRequest(_)
                    | ChunkMessageBody::PointerPutRequest(_)
            ) {
                if let Some(network) = payment_network {
                    assert_upload_node(&hello, &network).map_err(DataError::Network)?;
                }
            }
            let is_read = matches!(&request.body, ChunkMessageBody::GetRequest(_));
            // Reserve after peer RPC admission. The transport owns this permit
            // across cancellation, then returns it through response decoding.
            let read_permit = if is_read {
                Some(
                    crate::runtime::timeout(
                        admission.remaining(),
                        self.inner
                            .pool
                            .read_budget
                            .acquire(|| self.inner.pool.read_limit()),
                    )
                    .await
                    .map_err(|_| DataError::Timeout("read admission timed out".into()))?
                    .map_err(|error| DataError::Network(error.into()))?,
                )
            } else {
                None
            };
            let exclusive = matches!(
                &request.body,
                ChunkMessageBody::PutRequest(_) | ChunkMessageBody::PointerPutRequest(_)
            );
            let bytes = request
                .encode()
                .map_err(|e| DataError::Protocol(e.to_string()))?;
            let (response, _read_permit) = client
                .request_reserved(
                    BrowserRequestBody::ChunkProtocol,
                    &bytes,
                    timeout,
                    read_permit,
                    exclusive,
                )
                .await
                .map_err(rpc_data_error)?;
            if !matches!(response.header.body, BrowserResponseBody::ChunkProtocol) {
                return Err(DataError::Protocol(
                    "expected chunk_protocol response".into(),
                ));
            }
            let transport_processing = client.response_processing.get();
            let decode_started = web_time::Instant::now();
            let response = ChunkMessage::decode(&response.content)
                .map_err(|e| DataError::Protocol(e.to_string()))?;
            if response.request_id != request.request_id {
                return Err(DataError::Protocol("chunk request ID mismatch".into()));
            }
            if let ChunkMessageBody::GetResponse(ant_protocol::ChunkGetResponse::Success {
                address,
                ..
            }) = &response.body
            {
                self.sources.borrow_mut().put(
                    *address,
                    BrowserNode {
                        address_record: None,
                        peer_record: None,
                        peer_id: peer.to_hex(),
                        native_addresses: Vec::new(),
                        reliability: 1.0,
                        webrtc_direct: Some(endpoint),
                    },
                );
            }
            if is_read {
                let processing = transport_processing + decode_started.elapsed();
                drop(client);
                // Yield between bulk responses, then measure local event-loop
                // lateness separately from network/discovery service time.
                let yielded = web_time::Instant::now();
                TimeoutFuture::new(0).await;
                self.inner.pool.read_budget.observe_processing(
                    processing,
                    yielded.elapsed(),
                    self.inner.pool.read_limit(),
                );
            }
            Ok(response)
        })
    }
}

/// Convert a verified native quote to the existing JavaScript wallet envelope.
pub(super) fn native_quote_artifact(
    quote: &ant_protocol::evm::PaymentQuote,
    sidecars: &[Vec<u8>],
) -> Result<BrowserQuoteArtifact, String> {
    use ant_protocol::payment::commitment::{commitment_hash, StorageCommitment};
    let commitment = sidecars.iter().find_map(|encoded| {
        let value: StorageCommitment = rmp_serde::from_slice(encoded).ok()?;
        if commitment_hash(&value) != quote.commitment_pin {
            return None;
        }
        Some(super::super::protocol::BrowserCommitmentArtifact {
            encoded: hex::encode(encoded),
            root: hex::encode(value.root),
            key_count: value.key_count,
            sender_peer_id: hex::encode(value.sender_peer_id),
            sender_public_key: hex::encode(value.sender_public_key),
            signature: hex::encode(value.signature),
        })
    });
    Ok(BrowserQuoteArtifact {
        peer_id: hex::encode(blake3::hash(&quote.pub_key).as_bytes()),
        content: hex::encode(quote.content.0),
        timestamp_secs: quote
            .timestamp
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_secs(),
        price: quote.price.to_string(),
        rewards_address: format!("0x{}", hex::encode(quote.rewards_address)),
        public_key: hex::encode(&quote.pub_key),
        signature: hex::encode(&quote.signature),
        committed_key_count: quote.committed_key_count,
        commitment_pin: quote.commitment_pin.map(hex::encode),
        quote_hash: hex::encode(quote.hash()),
        commitment,
    })
}

fn rpc_data_error(error: RpcError) -> DataError {
    match error {
        RpcError::Timeout(message) => DataError::Timeout(message),
        other => DataError::Network(other.to_string()),
    }
}
