//! Chunk storage operations.
//!
//! Chunks are immutable, content-addressed data blocks where the address
//! is the BLAKE3 hash of the content.

use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_node::ant_protocol::{
    ChunkGetRequest, ChunkGetResponse, ChunkMessage, ChunkMessageBody, ChunkPutRequest,
    ChunkPutResponse,
};
use ant_node::client::{compute_address, send_and_await_chunk_response, DataChunk, XorName};
use ant_node::core::{MultiAddr, PeerId};
use ant_node::CLOSE_GROUP_MAJORITY;
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use std::future::Future;
use std::time::Duration;
use tracing::{debug, info, warn};

/// Data type identifier for chunks (used in quote requests).
const CHUNK_DATA_TYPE: u32 = 0;

impl Client {
    /// Store a chunk on the Autonomi network with payment.
    ///
    /// Checks if the chunk already exists before paying. If it does,
    /// returns the address immediately without incurring on-chain costs.
    /// Otherwise collects quotes, pays on-chain, then stores with proof
    /// to `CLOSE_GROUP_MAJORITY` peers.
    ///
    /// # Errors
    ///
    /// Returns an error if payment or the network operation fails.
    pub async fn chunk_put(&self, content: Bytes) -> Result<XorName> {
        let address = compute_address(&content);
        let data_size = u64::try_from(content.len())
            .map_err(|e| Error::InvalidData(format!("content size too large: {e}")))?;

        match self
            .pay_for_storage(&address, data_size, CHUNK_DATA_TYPE)
            .await
        {
            Ok((proof, peers)) => self.chunk_put_to_close_group(content, proof, &peers).await,
            Err(Error::AlreadyStored) => {
                debug!(
                    "Chunk {} already stored on network, skipping payment",
                    hex::encode(address)
                );
                Ok(address)
            }
            Err(e) => Err(e),
        }
    }

    /// Store a chunk to `CLOSE_GROUP_MAJORITY` peers from the quoted set.
    ///
    /// Initially sends the PUT concurrently to the first
    /// `CLOSE_GROUP_MAJORITY` peers. If any fail, falls back to the
    /// remaining peers in the quoted set until majority is reached or
    /// all peers are exhausted.
    ///
    /// # Errors
    ///
    /// Returns an error if fewer than `CLOSE_GROUP_MAJORITY` peers accept
    /// the chunk.
    pub(crate) async fn chunk_put_to_close_group(
        &self,
        content: Bytes,
        proof: Vec<u8>,
        peers: &[(PeerId, Vec<MultiAddr>)],
    ) -> Result<XorName> {
        let address = compute_address(&content);

        let initial_count = peers.len().min(CLOSE_GROUP_MAJORITY);
        let (initial_peers, fallback_peers) = peers.split_at(initial_count);

        let mut put_futures = FuturesUnordered::new();
        for (peer_id, addrs) in initial_peers {
            put_futures.push(self.spawn_chunk_put(content.clone(), proof.clone(), peer_id, addrs));
        }

        let mut success_count = 0usize;
        let mut failures: Vec<String> = Vec::new();
        let mut fallback_iter = fallback_peers.iter();

        while let Some((peer_id, result)) = put_futures.next().await {
            match result {
                Ok(_) => {
                    success_count += 1;
                    if success_count >= CLOSE_GROUP_MAJORITY {
                        info!(
                            "Chunk {} stored on {success_count} peers (majority reached)",
                            hex::encode(address)
                        );
                        return Ok(address);
                    }
                }
                Err(e) => {
                    warn!("Failed to store chunk on {peer_id}: {e}");
                    failures.push(format!("{peer_id}: {e}"));

                    if let Some((fb_peer, fb_addrs)) = fallback_iter.next() {
                        debug!(
                            "Falling back to peer {fb_peer} for chunk {}",
                            hex::encode(address)
                        );
                        put_futures.push(self.spawn_chunk_put(
                            content.clone(),
                            proof.clone(),
                            fb_peer,
                            fb_addrs,
                        ));
                    }
                }
            }
        }

        Err(Error::InsufficientPeers(format!(
            "Stored on {success_count} peers, need {CLOSE_GROUP_MAJORITY}. Failures: [{}]",
            failures.join("; ")
        )))
    }

    /// Spawn a chunk PUT future for a single peer.
    fn spawn_chunk_put<'a>(
        &'a self,
        content: Bytes,
        proof: Vec<u8>,
        peer_id: &'a PeerId,
        addrs: &'a [MultiAddr],
    ) -> impl Future<Output = (PeerId, Result<XorName>)> + 'a {
        let peer_id_owned = *peer_id;
        async move {
            let result = self
                .chunk_put_with_proof(content, proof, &peer_id_owned, addrs)
                .await;
            (peer_id_owned, result)
        }
    }

    /// Store a chunk on the Autonomi network with a pre-built payment proof.
    ///
    /// Sends to a single peer. Callers that need replication across the
    /// close group should use `chunk_put_to_close_group` instead.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_put_with_proof(
        &self,
        content: Bytes,
        proof: Vec<u8>,
        target_peer: &PeerId,
        peer_addrs: &[MultiAddr],
    ) -> Result<XorName> {
        let address = compute_address(&content);
        let node = self.network().node();

        let request_id = self.next_request_id();
        let request = ChunkPutRequest::with_payment(address, content.to_vec(), proof);
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::PutRequest(request),
        };
        let message_bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("Failed to encode PUT request: {e}")))?;

        let timeout = Duration::from_secs(self.config().timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().timeout_secs;

        send_and_await_chunk_response(
            node,
            target_peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            |body| match body {
                ChunkMessageBody::PutResponse(ChunkPutResponse::Success { address: addr }) => {
                    info!("Chunk stored at {}", hex::encode(addr));
                    Some(Ok(addr))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::AlreadyExists {
                    address: addr,
                }) => {
                    info!("Chunk already exists at {}", hex::encode(addr));
                    Some(Ok(addr))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::PaymentRequired { message }) => {
                    Some(Err(Error::Payment(format!("Payment required: {message}"))))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::Error(e)) => Some(Err(
                    Error::Protocol(format!("Remote PUT error for {addr_hex}: {e}")),
                )),
                _ => None,
            },
            |e| Error::Network(format!("Failed to send PUT to peer: {e}")),
            || {
                Error::Timeout(format!(
                    "Timeout waiting for store response after {timeout_secs}s"
                ))
            },
        )
        .await
    }

    /// Retrieve a chunk from the Autonomi network.
    ///
    /// Queries all peers in the close group for the chunk address,
    /// returning the first successful response. This handles the case
    /// where the storing peer differs from the first peer returned by
    /// DHT routing.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_get(&self, address: &XorName) -> Result<Option<DataChunk>> {
        // Check cache first, with integrity verification.
        if let Some(cached) = self.chunk_cache().get(address) {
            let computed = compute_address(&cached);
            if computed == *address {
                debug!("Cache hit for chunk {}", hex::encode(address));
                return Ok(Some(DataChunk::new(*address, cached)));
            }
            // Cache entry corrupted — evict and fall through to network fetch.
            debug!(
                "Cache corruption detected for {}: evicting",
                hex::encode(address)
            );
            self.chunk_cache().remove(address);
        }

        let peers = self.close_group_peers(address).await?;
        let addr_hex = hex::encode(address);

        for (peer, addrs) in &peers {
            match self.chunk_get_from_peer(address, peer, addrs).await {
                Ok(Some(chunk)) => {
                    self.chunk_cache().put(chunk.address, chunk.content.clone());
                    return Ok(Some(chunk));
                }
                Ok(None) => {
                    debug!("Chunk {addr_hex} not found on peer {peer}, trying next");
                }
                Err(Error::Timeout(_) | Error::Network(_)) => {
                    debug!("Peer {peer} unreachable for chunk {addr_hex}, trying next");
                }
                Err(e) => return Err(e),
            }
        }

        // None of the close group peers had the chunk
        Ok(None)
    }

    /// Fetch a chunk from a specific peer.
    async fn chunk_get_from_peer(
        &self,
        address: &XorName,
        peer: &PeerId,
        peer_addrs: &[MultiAddr],
    ) -> Result<Option<DataChunk>> {
        let node = self.network().node();
        let request_id = self.next_request_id();
        let request = ChunkGetRequest::new(*address);
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::GetRequest(request),
        };
        let message_bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("Failed to encode GET request: {e}")))?;

        let timeout = Duration::from_secs(self.config().timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().timeout_secs;

        send_and_await_chunk_response(
            node,
            peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            |body| match body {
                ChunkMessageBody::GetResponse(ChunkGetResponse::Success {
                    address: addr,
                    content,
                }) => {
                    if addr != *address {
                        return Some(Err(Error::InvalidData(format!(
                            "Mismatched chunk address: expected {addr_hex}, got {}",
                            hex::encode(addr)
                        ))));
                    }

                    let computed = compute_address(&content);
                    if computed != addr {
                        return Some(Err(Error::InvalidData(format!(
                            "Invalid chunk content: expected hash {addr_hex}, got {}",
                            hex::encode(computed)
                        ))));
                    }

                    debug!(
                        "Retrieved chunk {} ({} bytes) from peer {peer}",
                        hex::encode(addr),
                        content.len()
                    );
                    Some(Ok(Some(DataChunk::new(addr, Bytes::from(content)))))
                }
                ChunkMessageBody::GetResponse(ChunkGetResponse::NotFound { .. }) => Some(Ok(None)),
                ChunkMessageBody::GetResponse(ChunkGetResponse::Error(e)) => Some(Err(
                    Error::Protocol(format!("Remote GET error for {addr_hex}: {e}")),
                )),
                _ => None,
            },
            |e| Error::Network(format!("Failed to send GET to peer {peer}: {e}")),
            || {
                Error::Timeout(format!(
                    "Timeout waiting for chunk {addr_hex} from {peer} after {timeout_secs}s"
                ))
            },
        )
        .await
    }

    /// Check if a chunk exists on the network.
    ///
    /// # Errors
    ///
    /// Returns an error if the network operation fails.
    pub async fn chunk_exists(&self, address: &XorName) -> Result<bool> {
        self.chunk_get(address).await.map(|opt| opt.is_some())
    }
}
