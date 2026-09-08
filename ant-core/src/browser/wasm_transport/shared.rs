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
                client.hello().await?;
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
            let client = self
                .inner
                .pool
                .client(&endpoint)
                .await
                .map_err(DataError::Network)?;
            let hello = client.hello().await.map_err(DataError::Network)?;
            if !hello.capabilities.iter().any(|cap| cap == "chunk_protocol") {
                return Err(DataError::Network(
                    "node does not support shared ant-protocol RPC".into(),
                ));
            }
            let payment_network = self.payment_network.clone();
            if matches!(
                request.body,
                ChunkMessageBody::QuoteRequest(_) | ChunkMessageBody::PutRequest(_)
            ) {
                if let Some(network) = payment_network {
                    assert_upload_node(&hello, &network).map_err(DataError::Network)?;
                }
            }
            let bytes = request
                .encode()
                .map_err(|e| DataError::Protocol(e.to_string()))?;
            let response = crate::runtime::timeout(
                timeout,
                client.request_typed(BrowserRequestBody::ChunkProtocol, &bytes),
            )
            .await
            .map_err(|_| DataError::Timeout("chunk protocol response deadline expired".into()))?
            .map_err(|error| match error {
                RpcError::Timeout(message) => DataError::Timeout(message),
                other => DataError::Network(other.to_string()),
            })?;
            if !matches!(response.header.body, BrowserResponseBody::ChunkProtocol) {
                return Err(DataError::Protocol(
                    "expected chunk_protocol response".into(),
                ));
            }
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
