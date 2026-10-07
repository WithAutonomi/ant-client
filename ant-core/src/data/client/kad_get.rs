//! Native chunk reads through a client-driven Kademlia lookup (ant-node
//! ADR-0020).
//!
//! The policy lives in [`crate::client_engine::kad_read`]; this adapter runs
//! it over saorsa-core's lookup steps and the chunk protocol.

use crate::client_engine::kad_read::{kad_read, KadNetwork, KadReadOutcome, StepAnswer};
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use ant_protocol::transport::{DHTNode, P2PNode};
use ant_protocol::{
    advertises_get_or_closer, send_and_await_chunk_response_with_metadata, ChunkGetOrCloserRequest,
    ChunkGetOrCloserResponse, ChunkMessage, ChunkMessageBody, DataChunk, XorName,
};
use std::time::Duration;

/// One chunk read's view of the native network.
struct NativeKadNetwork<'a> {
    client: &'a Client,
    address: XorName,
}

impl NativeKadNetwork<'_> {
    fn node(&self) -> &P2PNode {
        self.client.network().node()
    }

    fn request_timeout(&self) -> Duration {
        Duration::from_secs(self.client.config().chunk_get_timeout_secs)
    }

    /// Check a chunk a peer sent against the address it was asked for.
    fn verified(&self, address: XorName, content: bytes::Bytes) -> Result<DataChunk> {
        if address != self.address {
            return Err(Error::InvalidData(format!(
                "asked for chunk {} and was sent {}",
                hex::encode(self.address),
                hex::encode(address)
            )));
        }
        crate::record::verify(&address, &content).map_err(Error::InvalidData)?;
        Ok(DataChunk::new(address, content))
    }
}

impl KadNetwork for NativeKadNetwork<'_> {
    type Chunk = DataChunk;
    type Error = Error;

    async fn is_dialable(&self, peer: &DHTNode) -> bool {
        self.node().dht().is_lookup_candidate_dialable(peer).await
    }

    async fn prepare(&self, peer: &DHTNode) -> Result<bool> {
        let node = self.node();
        node.dht()
            .connect_lookup_peer(peer)
            .await
            .map_err(|e| Error::Network(format!("Failed to connect to {}: {e}", peer.peer_id)))?;
        Ok(node
            .transport()
            .peer_user_agent(&peer.peer_id)
            .await
            .is_some_and(|agent| advertises_get_or_closer(&agent)))
    }

    async fn get_or_closer(&self, peer: &DHTNode) -> Result<StepAnswer<DataChunk>> {
        let request_id = self.client.next_request_id();
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::GetOrCloserRequest(ChunkGetOrCloserRequest::new(self.address)),
        };
        let bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("Failed to encode get-or-closer: {e}")))?;
        let addr_hex = hex::encode(self.address);
        let response = send_and_await_chunk_response_with_metadata(
            self.node(),
            &peer.peer_id,
            bytes,
            request_id,
            self.request_timeout(),
            &peer.addresses_by_priority(),
            |body| match body {
                ChunkMessageBody::GetOrCloserResponse(response) => Some(Ok(response)),
                _ => None,
            },
            |e| {
                Error::Network(format!(
                    "Failed to send get-or-closer to {}: {e}",
                    peer.peer_id
                ))
            },
            || {
                Error::Timeout(format!(
                    "Timeout waiting for get-or-closer {addr_hex} from {}",
                    peer.peer_id
                ))
            },
        )
        .await?;
        match response.result? {
            ChunkGetOrCloserResponse::Found { address, content } => {
                Ok(StepAnswer::Found(self.verified(address, content)?))
            }
            ChunkGetOrCloserResponse::Closer { address, peers } if address == self.address => {
                let nodes = self
                    .node()
                    .dht()
                    .decode_closer_peers(
                        &peer.peer_id,
                        &self.address,
                        &peers,
                        response.transport_source.as_ref(),
                    )
                    .await
                    .map_err(|e| {
                        Error::Protocol(format!("Invalid closer peers from {}: {e}", peer.peer_id))
                    })?;
                Ok(StepAnswer::Closer(nodes))
            }
            ChunkGetOrCloserResponse::Error(e) => Err(Error::Protocol(format!(
                "Remote get-or-closer error for {addr_hex}: {e}"
            ))),
            _ => Err(Error::Protocol(format!(
                "Unexpected get-or-closer answer for {addr_hex} from {}",
                peer.peer_id
            ))),
        }
    }

    async fn find_node(&self, peer: &DHTNode) -> Result<Vec<DHTNode>> {
        self.node()
            .dht()
            .find_node_on_peer(peer, &self.address)
            .await
            .map_err(|e| Error::Network(format!("FIND_NODE to {} failed: {e}", peer.peer_id)))
    }

    async fn get(&self, peer: &DHTNode) -> Result<Option<DataChunk>> {
        self.client
            .chunk_get_from_peer(&self.address, &peer.peer_id, &peer.addresses_by_priority())
            .await
    }
}

/// Failures that only take one peer out of a read; anything else, such as
/// content that does not match its address, ends it.
fn is_retryable(error: &Error) -> bool {
    matches!(
        error,
        Error::Timeout(_) | Error::Network(_) | Error::Protocol(_)
    )
}

impl Client {
    /// Read a chunk through a lookup that asks every peer it queries for the
    /// chunk as well as for closer peers (ant-node ADR-0020).
    ///
    /// Returns `None` when the lookup ends without any queried peer sending
    /// the chunk. That is not evidence the chunk is missing: the caller's
    /// ordinary read path, with its absence rule, decides that.
    pub(crate) async fn chunk_get_kad(
        &self,
        address: &XorName,
        peer_count: usize,
    ) -> Result<Option<DataChunk>> {
        let network = NativeKadNetwork {
            client: self,
            address: *address,
        };
        let seeds = self
            .network()
            .node()
            .dht()
            .find_closest_nodes_local(address, peer_count)
            .await;
        Ok(
            match kad_read(&network, *address, peer_count, seeds, is_retryable).await? {
                KadReadOutcome::Found(chunk) => Some(chunk),
                KadReadOutcome::NotFound => None,
            },
        )
    }
}

#[cfg(feature = "test-utils")]
impl Client {
    /// The kad read alone, without the ordinary read path behind it, so a
    /// test can tell what the lookup found by itself.
    ///
    /// # Errors
    ///
    /// Returns an error if a peer sends content that does not match its
    /// address.
    #[doc(hidden)]
    pub async fn chunk_get_kad_only(&self, address: &XorName) -> Result<Option<DataChunk>> {
        self.chunk_get_kad(address, self.config().close_group_size)
            .await
    }
}
