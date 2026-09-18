//! Client operations for pointers (ADR-0015 in `ant-node`).
//!
//! A pointer is a mutable, owner-signed reference stored at
//! `BLAKE3("autonomi.pointer.address.v1" || owner_key)`. Public-key addressed
//! and self-verifying: the key is inside the record, so anything that parses is
//! checkable on the spot, with no fetch.
//!
//! # What the client owns
//!
//! - **The counter.** Creation is counter 0; each update is exactly one past
//!   the current value, so one payment buys one increment. [`Client::pointer_update`]
//!   reads the current record and steps it, because guessing the counter is the
//!   one way to write an update the network refuses.
//! - **Paying at the state, not the address.** The quote names `state_id`, while
//!   the close group that issues it is the one around the address. Paying at the
//!   address would buy every future update at once.
//! - **Following targets.** A node never interprets a pointer's target — it
//!   stores 33 opaque bytes. Chain resolution, its depth limit and its cycle
//!   detection are entirely here: see [`Client::pointer_resolve`].

use std::collections::HashSet;

use ant_protocol::chunk::{
    ChunkMessage, ChunkMessageBody, PointerGetRequest, PointerGetResponse, PointerPutRequest,
    PointerPutResponse,
};
use ant_protocol::pointer::{
    Pointer, PointerTarget, PointerTargetKind, DATA_TYPE_POINTER, POINTER_WIRE_LEN,
};
use ant_protocol::pqc::api::{MlDsaPublicKey, MlDsaSecretKey};
use ant_protocol::send_and_await_chunk_response;
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::XorName;
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};

use crate::data::client::Client;
use crate::data::error::{Error, Result};

/// The majority of `peers` — the threshold both a store and a read must meet.
///
/// Derived from the group actually returned, not from a fixed constant: the
/// close-group width is configurable, and a fixed four against a width of
/// twenty would let a write land on four peers and a *disjoint* four answer the
/// read. Quorums only intersect if both are majorities of the same set.
fn majority_of(peers: usize) -> usize {
    (peers / 2) + 1
}

/// How many pointer hops [`Client::pointer_resolve`] will follow.
///
/// A pointer may target another pointer — that is how handover works — so a
/// chain can be arbitrarily long, and a malicious or careless owner can make it
/// a cycle. The node cannot stop either: it never reads the target. So the
/// client bounds it.
pub const MAX_POINTER_RESOLVE_DEPTH: usize = 16;

impl Client {
    /// Create a pointer at counter 0, owned by `owner`.
    ///
    /// # Errors
    ///
    /// Returns an error if signing fails, payment fails, or no storer accepts
    /// the record.
    pub async fn pointer_create(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<XorName> {
        let record = Pointer::create(secret_key, owner, target)
            .map_err(|e| Error::InvalidData(format!("cannot sign pointer: {e}")))?;
        self.pointer_put(&record).await
    }

    /// Update the pointer owned by `owner` to `target`, at `counter + 1`.
    ///
    /// Reads the current record first: the counter must be exactly one past
    /// what the network holds, so it cannot be guessed. A pointer that does not
    /// exist yet is created at 0.
    ///
    /// # Errors
    ///
    /// Returns an error if the current record cannot be read, the counter is
    /// exhausted, signing fails, payment fails, or no storer accepts it.
    pub async fn pointer_update(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<XorName> {
        let address = ant_protocol::pointer::pointer_address(owner);
        let record = match self.pointer_get(&address).await? {
            Some(current) => current
                .update(secret_key, target)
                .map_err(|e| Error::InvalidData(format!("cannot sign pointer update: {e}")))?,
            None => Pointer::create(secret_key, owner, target)
                .map_err(|e| Error::InvalidData(format!("cannot sign pointer: {e}")))?,
        };
        self.pointer_put(&record).await
    }

    /// Pay for and store an already-signed pointer.
    ///
    /// # Errors
    ///
    /// Returns an error if payment fails or no storer accepts the record.
    pub async fn pointer_put(&self, record: &Pointer) -> Result<XorName> {
        let address = record.address();
        let state_id = record.state_id();

        // The quote names the state; the peers that may issue it are the close
        // group around the address. One payment, one increment.
        let (proof, _) = self
            .pay_for_storage_split(
                &address,
                &state_id,
                POINTER_WIRE_LEN as u64,
                DATA_TYPE_POINTER,
            )
            .await?;

        // Store on exactly the peers a read will ask.
        //
        // The payment plan can return far more put-targets than the close
        // group, and counting those towards success would let a write be
        // acknowledged entirely outside the set `pointer_get` queries — stored,
        // paid for, and immediately unreadable. So the write targets the same
        // strict closest-K set the read does.
        let targets = self
            .network()
            .find_closest_peers(&address, self.config().close_group_size)
            .await?;
        let wanted = majority_of(targets.len());

        let request = PointerPutRequest::with_payment(Bytes::from(record.to_bytes()), proof);
        let mut in_flight = FuturesUnordered::new();
        for (peer_id, addrs) in &targets {
            let request = request.clone();
            in_flight.push(self.send_pointer_put(
                request,
                *peer_id,
                addrs.clone(),
                address,
                state_id,
            ));
        }

        let mut stored = 0usize;
        let mut last_error = None;
        while let Some(result) = in_flight.next().await {
            match result {
                Ok(()) => {
                    stored += 1;
                    if stored >= wanted {
                        return Ok(address);
                    }
                }
                Err(e) => last_error = Some(e),
            }
        }

        if stored > 0 {
            return Err(Error::CloseGroupShortfall(format!(
                "pointer {} stored on {stored} of {wanted} close-group peers",
                hex::encode(address)
            )));
        }
        Err(last_error.unwrap_or_else(|| {
            Error::Protocol("no close-group peer accepted the pointer".to_string())
        }))
    }

    /// Read the pointer at `address`, verifying it before returning it.
    ///
    /// The record is checked against the address it was asked for, so a storer
    /// cannot answer with a different owner's pointer.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer answers.
    pub async fn pointer_get(&self, address: &XorName) -> Result<Option<Pointer>> {
        let peers = self
            .network()
            .find_closest_peers(address, self.config().close_group_size)
            .await?;

        // Ask the whole close group at once and keep the best answer.
        //
        // Every reply is verified independently, so a dishonest peer can only
        // offer a record that is genuinely signed and genuinely belongs here.
        // What it must not be able to do is decide the answer alone: any peer
        // may legitimately hold a stale record, so taking the first reply would
        // let one pin a reader to it. Asking concurrently also means one slow
        // peer cannot stall the read behind the store timeout.
        let mut in_flight = FuturesUnordered::new();
        for (peer_id, addrs) in &peers {
            in_flight.push(self.send_pointer_get(*address, *peer_id, addrs.clone()));
        }

        let wanted = majority_of(peers.len());
        let mut best: Option<Pointer> = None;
        let mut answered = 0usize;
        let mut last_error = None;
        while let Some(result) = in_flight.next().await {
            match result {
                Ok(found) => {
                    answered += 1;
                    if let Some(record) = found {
                        best = Some(match best {
                            Some(held) if held.replaces(&record) => held,
                            _ => record,
                        });
                    }
                    // A majority has spoken. Waiting for the rest would let one
                    // unreachable peer hold the read open for its whole timeout
                    // after the answer is already known.
                    if answered >= wanted {
                        return Ok(best);
                    }
                }
                Err(e) => last_error = Some(e),
            }
        }

        if answered == 0 {
            return Err(last_error.unwrap_or_else(|| {
                Error::Protocol("no close-group peer answered for the pointer".to_string())
            }));
        }
        // A single answer is not a network verdict: with no replication, one
        // reachable peer holding a stale record looks exactly like the whole
        // group agreeing. Say so rather than presenting it as the value.
        if answered < wanted {
            return Err(Error::CloseGroupShortfall(format!(
                "only {answered} of the close group answered for pointer {}",
                hex::encode(address)
            )));
        }
        Ok(best)
    }

    /// Follow a pointer chain to the chunk it ends at.
    ///
    /// A pointer may target another pointer, so this walks until it reaches a
    /// non-pointer target, refusing to loop or to walk further than
    /// [`MAX_POINTER_RESOLVE_DEPTH`]. Both guards live here because the node
    /// never reads a target and so cannot enforce either.
    ///
    /// A multi-hop read is not atomic: a target can move while the walk is in
    /// flight, so the result is what the chain said, not a snapshot of it.
    ///
    /// # Errors
    ///
    /// Returns an error if a hop is missing, the chain cycles, the depth limit
    /// is reached, or the final target is a kind this build does not know.
    pub async fn pointer_resolve(&self, address: &XorName) -> Result<PointerTarget> {
        let mut seen = HashSet::new();
        let mut at = *address;

        for _ in 0..MAX_POINTER_RESOLVE_DEPTH {
            if !seen.insert(at) {
                return Err(Error::InvalidData(format!(
                    "pointer chain cycles back to {}",
                    hex::encode(at)
                )));
            }
            let record = self.pointer_get(&at).await?.ok_or_else(|| {
                Error::InvalidData(format!("pointer chain breaks at {}", hex::encode(at)))
            })?;
            let target = record.target();
            match target.kind() {
                Some(PointerTargetKind::Pointer) => at = target.address,
                Some(PointerTargetKind::Chunk) => return Ok(target),
                // A tag this build does not know. The bytes are authentic — the
                // record is signed — so report the target rather than pretend
                // the chain is broken; the caller decides what to do with it.
                None => return Ok(target),
            }
        }
        Err(Error::InvalidData(format!(
            "pointer chain from {} is longer than {MAX_POINTER_RESOLVE_DEPTH} hops",
            hex::encode(address)
        )))
    }

    /// Send one pointer PUT and check the acknowledgement against what was sent.
    ///
    /// A peer that answers `Success` for a different address or a different
    /// state is claiming to hold something the client never submitted. Taking
    /// that at face value would let one peer end the write — after payment —
    /// while storing nothing, so the reply has to name the record it was given.
    async fn send_pointer_put(
        &self,
        request: PointerPutRequest,
        target_peer: PeerId,
        peer_addrs: Vec<MultiAddr>,
        expected_address: XorName,
        expected_state: XorName,
    ) -> Result<()> {
        let request_id = self.next_request_id();
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::PointerPutRequest(request),
        };
        let bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("cannot encode pointer PUT: {e}")))?;

        send_and_await_chunk_response(
            self.network().node(),
            &target_peer,
            bytes,
            request_id,
            std::time::Duration::from_secs(self.config().merkle_store_timeout_secs),
            &peer_addrs,
            move |body| match body {
                // `Unchanged` is success for a retry: the state the client
                // signed is exactly what the node holds.
                ChunkMessageBody::PointerPutResponse(
                    PointerPutResponse::Success { address, state_id }
                    | PointerPutResponse::Unchanged { address, state_id },
                ) => Some(
                    if address == expected_address && state_id == expected_state {
                        Ok(())
                    } else {
                        Err(Error::InvalidData(format!(
                            "peer acknowledged a pointer this client did not send: \
                         address {} state {}",
                            hex::encode(address),
                            hex::encode(state_id)
                        )))
                    },
                ),
                ChunkMessageBody::PointerPutResponse(PointerPutResponse::Stale {
                    state_id,
                    ..
                }) => Some(Err(Error::InvalidData(format!(
                    "the pointer moved while this update was in flight; the network \
                     now holds state {}",
                    hex::encode(state_id)
                )))),
                ChunkMessageBody::PointerPutResponse(PointerPutResponse::PaymentRequired {
                    message,
                }) => Some(Err(Error::Payment(message))),
                ChunkMessageBody::PointerPutResponse(PointerPutResponse::Error(e)) => {
                    Some(Err(Error::Protocol(format!("pointer PUT refused: {e}"))))
                }
                _ => None,
            },
            |e| Error::Network(format!("pointer PUT send failed: {e}")),
            || Error::Timeout("pointer PUT timed out".to_string()),
        )
        .await
    }

    /// Send one pointer GET and validate whatever comes back.
    async fn send_pointer_get(
        &self,
        address: XorName,
        target_peer: PeerId,
        peer_addrs: Vec<MultiAddr>,
    ) -> Result<Option<Pointer>> {
        let request_id = self.next_request_id();
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::PointerGetRequest(PointerGetRequest::new(address)),
        };
        let bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("cannot encode pointer GET: {e}")))?;

        send_and_await_chunk_response(
            self.network().node(),
            &target_peer,
            bytes,
            request_id,
            std::time::Duration::from_secs(self.config().chunk_get_timeout_secs),
            &peer_addrs,
            move |body| match body {
                ChunkMessageBody::PointerGetResponse(PointerGetResponse::Success { record }) => {
                    // Verify before trusting: the signature must check out and
                    // the record must belong at the address that was asked for,
                    // or a storer could answer with someone else's pointer.
                    Some(match Pointer::from_bytes(&record) {
                        Ok(record) if record.address() == address => Ok(Some(record)),
                        Ok(_) => Err(Error::InvalidData(
                            "peer answered with a pointer for a different address".to_string(),
                        )),
                        Err(e) => Err(Error::InvalidData(format!("invalid pointer: {e}"))),
                    })
                }
                ChunkMessageBody::PointerGetResponse(PointerGetResponse::NotFound { .. }) => {
                    Some(Ok(None))
                }
                ChunkMessageBody::PointerGetResponse(PointerGetResponse::Error(e)) => {
                    Some(Err(Error::Protocol(format!("pointer GET refused: {e}"))))
                }
                _ => None,
            },
            |e| Error::Network(format!("pointer GET send failed: {e}")),
            || Error::Timeout("pointer GET timed out".to_string()),
        )
        .await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use ant_protocol::pointer::pointer_address;
    use ant_protocol::pqc::api::ml_dsa_65;

    fn keypair(seed: u8) -> (MlDsaPublicKey, MlDsaSecretKey) {
        ml_dsa_65().generate_keypair_from_seed(&[seed; 32])
    }

    fn chunk_target(byte: u8) -> PointerTarget {
        PointerTarget::new(PointerTargetKind::Chunk, [byte; 32])
    }

    /// The client's half of "pay to create, pay to update": a create is counter
    /// 0 and every update is exactly one more, so one payment buys one
    /// increment and the node never has to refuse a guessed counter.
    #[test]
    fn the_client_creates_at_zero_and_steps_by_one() {
        let (pk, sk) = keypair(1);
        let created = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        assert_eq!(created.counter(), 0);

        let mut current = created;
        for expected in 1..=5u64 {
            current = current.update(&sk, chunk_target(2)).expect("update");
            assert_eq!(current.counter(), expected);
        }
    }

    /// A pointer is public-key addressed: the address follows the owner key and
    /// nothing else, so a client can compute where its own pointer lives
    /// without asking the network.
    #[test]
    fn the_address_is_derivable_offline_from_the_owner_key() {
        let (pk, sk) = keypair(2);
        let record = Pointer::create(&sk, &pk, chunk_target(3)).expect("create");
        assert_eq!(record.address(), pointer_address(&pk));

        let (other, _) = keypair(3);
        assert_ne!(record.address(), pointer_address(&other));
    }

    /// Every update is a distinct paid state, so a receipt for one cannot fund
    /// another.
    #[test]
    fn each_update_is_paid_under_its_own_identifier() {
        let (pk, sk) = keypair(4);
        let created = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let next = created.update(&sk, chunk_target(2)).expect("update");
        let again = next.update(&sk, chunk_target(3)).expect("update");

        let states: std::collections::BTreeSet<_> = [&created, &next, &again]
            .iter()
            .map(|r| r.state_id())
            .collect();
        assert_eq!(states.len(), 3, "three updates, three paid identifiers");

        // And they all live at one address, which is what routing uses.
        assert_eq!(created.address(), next.address());
        assert_eq!(next.address(), again.address());
    }

    /// A record that does not verify, or verifies but belongs elsewhere, must
    /// never be accepted as the answer for an address.
    #[test]
    fn a_record_for_another_address_is_not_an_answer() {
        let (mine, my_sk) = keypair(5);
        let (theirs, their_sk) = keypair(6);
        let asked_for = pointer_address(&mine);

        let mine_record = Pointer::create(&my_sk, &mine, chunk_target(1)).expect("create");
        let theirs_record = Pointer::create(&their_sk, &theirs, chunk_target(1)).expect("create");

        assert_eq!(mine_record.address(), asked_for);
        assert_ne!(
            theirs_record.address(),
            asked_for,
            "a storer answering with this must be refused"
        );

        // Tampering with any byte breaks the signature, so it never parses.
        let mut tampered = mine_record.to_bytes().to_vec();
        tampered[100] ^= 0xff;
        assert!(Pointer::from_bytes(&tampered).is_err());
    }

    /// A peer that answers with a record the client never sent must not end the
    /// write. Otherwise one peer takes the payment, stores nothing, and says
    /// "stored".
    #[test]
    fn an_acknowledgement_must_name_the_record_that_was_sent() {
        let (pk, sk) = keypair(9);
        let sent = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let other = Pointer::create(&sk, &pk, chunk_target(2)).expect("create");

        // Same address — same owner — but a different state.
        assert_eq!(sent.address(), other.address());
        assert_ne!(sent.state_id(), other.state_id());

        // The check the response handler makes.
        let accepts = |address, state| address == sent.address() && state == sent.state_id();
        assert!(accepts(sent.address(), sent.state_id()));
        assert!(
            !accepts(sent.address(), other.state_id()),
            "a different state at the right address must be refused"
        );
        assert!(
            !accepts([0u8; 32], sent.state_id()),
            "a different address must be refused"
        );
    }

    /// Reads take the best answer, not the first: any single peer is entitled
    /// to serve a stale record, and must not be able to pin a reader to it.
    #[test]
    fn a_read_keeps_the_winner_not_the_first_reply() {
        let (pk, sk) = keypair(10);
        let old = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let new = old.update(&sk, chunk_target(2)).expect("update");

        // Whichever order the replies arrive in, the merge rule picks the same.
        let fold = |replies: &[&Pointer]| {
            let mut best: Option<Pointer> = None;
            for r in replies {
                best = Some(match best {
                    Some(held) if held.replaces(r) => held,
                    _ => (*r).clone(),
                });
            }
            best.expect("non-empty")
        };
        assert_eq!(fold(&[&old, &new]).state_id(), new.state_id());
        assert_eq!(fold(&[&new, &old]).state_id(), new.state_id());
    }

    /// A write quorum and a read quorum must intersect at every group width.
    ///
    /// This is the property a fixed four-of-K silently broke: at K=20 a write
    /// on four peers and a read of a disjoint four would never meet, so an
    /// acknowledged pointer could read back as missing.
    #[test]
    fn write_and_read_quorums_always_intersect() {
        for k in 1usize..=64 {
            let write = majority_of(k);
            let read = majority_of(k);
            assert!(
                write + read > k,
                "at width {k} a {write}-write and a {read}-read can be disjoint"
            );
            assert!(write <= k, "a quorum cannot need more peers than exist");
        }
    }

    /// The default close group is seven, where a majority is four.
    #[test]
    fn the_default_group_needs_four() {
        assert_eq!(majority_of(7), 4);
        assert_eq!(majority_of(20), 11);
        assert_eq!(majority_of(1), 1);
    }

    /// A chain that points at itself must be caught by the seen-set, not by
    /// running out of hops: the node never reads a target, so a cycle is the
    /// client's to detect.
    #[test]
    fn a_self_referencing_chain_is_a_cycle() {
        let (pk, sk) = keypair(8);
        let address = pointer_address(&pk);
        let loops = Pointer::sign(
            &sk,
            &pk,
            0,
            PointerTarget::new(PointerTargetKind::Pointer, address),
        )
        .expect("sign");

        // The walk `pointer_resolve` performs, without the network: the first
        // hop lands back on the address already visited.
        let mut seen = HashSet::new();
        assert!(seen.insert(loops.address()));
        assert!(
            !seen.insert(loops.target().address),
            "a pointer targeting its own address is a cycle the client must refuse"
        );
    }

    /// An unknown target kind is carried, not rejected: the record is signed, so
    /// the bytes are authentic even when this build cannot follow them.
    #[test]
    fn an_unknown_target_kind_is_carried_not_rejected() {
        let (pk, sk) = keypair(7);
        let exotic = PointerTarget::from_raw_tag(200, [9; 32]);
        let record = Pointer::sign(&sk, &pk, 0, exotic).expect("sign");
        let parsed = Pointer::from_bytes(&record.to_bytes()).expect("parse");
        assert_eq!(parsed.target().kind_tag(), 200);
        assert_eq!(parsed.target().kind(), None);
    }
}
