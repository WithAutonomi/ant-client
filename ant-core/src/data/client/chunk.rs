//! Chunk storage operations.
//!
//! Chunks are immutable, content-addressed data blocks where the address
//! is the BLAKE3 hash of the content.

use crate::data::client::peer_cache::record_peer_outcome;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::{
    compute_address, send_and_await_chunk_response, ChunkGetRequest, ChunkGetResponse,
    ChunkMessage, ChunkMessageBody, ChunkPutRequest, ChunkPutResponse, DataChunk, XorName,
    CLOSE_GROUP_MAJORITY,
};
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use std::future::Future;
use std::time::{Duration, Instant};
use tracing::{debug, warn};

/// Data type identifier for chunks (used in quote requests).
const CHUNK_DATA_TYPE: u32 = 0;

/// A reusable set of peers built from a single DHT lookup, suitable
/// for fetching many chunks from overlapping close groups without
/// paying the lookup cost per chunk.
///
/// Build via [`Client::build_peer_pool_for`]. Use via
/// [`Client::chunk_get_with_pool`].
///
/// On live mainnet today, where `find_closest_peers` dominates
/// per-chunk download wall-clock (~70 s/lookup), a pool built once
/// and reused across a 32-chunk batch reduces total wall-clock by
/// ~34% vs the per-chunk lookup baseline. The advantage grows with
/// batch size.
#[derive(Debug, Clone)]
pub struct PeerPool {
    pub(crate) peers: Vec<(PeerId, Vec<MultiAddr>)>,
}

impl PeerPool {
    /// Number of peers in the pool.
    #[must_use]
    pub fn len(&self) -> usize {
        self.peers.len()
    }

    /// Whether the pool is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.peers.is_empty()
    }
}

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
                        debug!(
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

        let timeout = Duration::from_secs(self.config().store_timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().store_timeout_secs;

        let result = send_and_await_chunk_response(
            node,
            target_peer,
            message_bytes,
            request_id,
            timeout,
            peer_addrs,
            |body| match body {
                ChunkMessageBody::PutResponse(ChunkPutResponse::Success { address: addr }) => {
                    debug!("Chunk stored at {}", hex::encode(addr));
                    Some(Ok(addr))
                }
                ChunkMessageBody::PutResponse(ChunkPutResponse::AlreadyExists {
                    address: addr,
                }) => {
                    debug!("Chunk already exists at {}", hex::encode(addr));
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
        .await;

        // No RTT recorded on the PUT path: the wall-clock is dominated by
        // the ~4 MB payload upload, which reflects the uploader's uplink
        // rather than the peer's responsiveness. Quote-path and GET-path
        // RTTs still feed quality scoring.
        record_peer_outcome(node, *target_peer, peer_addrs, result.is_ok(), None).await;

        result
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

    /// Build a reusable pool of peers suitable for fetching any chunk
    /// in `addresses`.
    ///
    /// Performs ONE close-group lookup at the median address of the
    /// batch and returns the resulting peer set. On a network where
    /// `find_closest_peers` dominates per-chunk wall-clock (e.g. live
    /// Autonomi mainnet today), reusing this pool across many GETs
    /// via `chunk_get_with_pool` gives a measured ~1.5x speedup on
    /// 32-chunk downloads and grows with batch size.
    ///
    /// The pool is "best-effort": peers are the close group of the
    /// median XorName, which closely overlaps the close group of any
    /// nearby address. Chunks whose actual close group has rotated
    /// can still be fetched via the per-chunk fallback in
    /// `chunk_get_with_pool`.
    ///
    /// # Errors
    ///
    /// Returns an error if `addresses` is empty or if the DHT lookup
    /// fails.
    pub async fn build_peer_pool_for(&self, addresses: &[XorName]) -> Result<PeerPool> {
        if addresses.is_empty() {
            return Err(Error::InvalidData(
                "build_peer_pool_for requires at least one address".to_string(),
            ));
        }
        let mut sorted = addresses.to_vec();
        sorted.sort_unstable();
        let median_idx = sorted.len() / 2;
        let median = sorted.get(median_idx).copied().unwrap_or_else(|| sorted[0]);
        let peers = self.close_group_peers(&median).await?;
        debug!(
            "Built peer pool of {} peers from median address {} (batch size {})",
            peers.len(),
            hex::encode(median),
            addresses.len()
        );
        Ok(PeerPool { peers })
    }

    /// Fetch a chunk, trying the supplied pool's peers first.
    ///
    /// Falls back to a per-chunk close-group lookup if none of the
    /// pool peers have the chunk (or all pool peers are unreachable).
    /// Pair with `build_peer_pool_for` to amortize a single DHT walk
    /// across many GETs.
    ///
    /// Cache, integrity verification, and per-peer outcome reporting
    /// behave identically to `chunk_get`.
    ///
    /// # Errors
    ///
    /// Returns an error if all peers (pool + fallback close group)
    /// fail with non-recoverable errors.
    pub async fn chunk_get_with_pool(
        &self,
        address: &XorName,
        pool: &PeerPool,
    ) -> Result<Option<DataChunk>> {
        // Cache lookup with integrity verification — same as chunk_get.
        if let Some(cached) = self.chunk_cache().get(address) {
            let computed = compute_address(&cached);
            if computed == *address {
                debug!("Cache hit for chunk {} (pooled GET)", hex::encode(address));
                return Ok(Some(DataChunk::new(*address, cached)));
            }
            self.chunk_cache().remove(address);
        }

        let addr_hex = hex::encode(address);

        // Try the pool first.
        for (peer, addrs) in &pool.peers {
            match self.chunk_get_from_peer(address, peer, addrs).await {
                Ok(Some(chunk)) => {
                    self.chunk_cache().put(chunk.address, chunk.content.clone());
                    return Ok(Some(chunk));
                }
                Ok(None) => {
                    debug!("Chunk {addr_hex} not found on pool peer {peer}, trying next");
                }
                Err(Error::Timeout(_) | Error::Network(_)) => {
                    debug!("Pool peer {peer} unreachable for chunk {addr_hex}");
                }
                Err(e) => return Err(e),
            }
        }

        // Pool miss: fall back to a per-chunk close-group lookup so
        // we don't return a false NotFound when the pool's peers
        // simply weren't the right close group for this address.
        debug!("Pool exhausted for {addr_hex}; falling back to per-chunk close group");
        self.chunk_get(address).await
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

        let timeout = Duration::from_secs(self.config().store_timeout_secs);
        let addr_hex = hex::encode(address);
        let timeout_secs = self.config().store_timeout_secs;

        let start = Instant::now();
        let result = send_and_await_chunk_response(
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
        .await;

        let success = result.is_ok();
        let rtt_ms = success.then(|| start.elapsed().as_millis() as u64);
        record_peer_outcome(node, *peer, peer_addrs, success, rtt_ms).await;

        result
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_pool_empty_reports_zero_len() {
        let pool = PeerPool { peers: Vec::new() };
        assert_eq!(pool.len(), 0);
        assert!(pool.is_empty());
    }

    #[test]
    fn peer_pool_with_entries_reports_len() {
        use ant_protocol::transport::PeerId;
        let peer = PeerId::random();
        let pool = PeerPool {
            peers: vec![(peer, vec![])],
        };
        assert_eq!(pool.len(), 1);
        assert!(!pool.is_empty());
    }
}
