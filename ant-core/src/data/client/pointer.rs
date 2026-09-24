//! Client operations for pointers (ADR-0016 in `ant-node`).
//!
//! A pointer is a mutable, owner-signed reference stored at
//! `BLAKE3::derive_key("autonomi.pointer.address.v1", owner_key)`. Public-key
//! addressed and self-verifying: the key is inside the record, so anything that
//! parses is checkable on the spot, with no fetch.
//!
//! # What the client owns
//!
//! - **The counter.** Creation is counter 0 and [`Client::pointer_update`]
//!   signs one past the counter the network serves, because the update has to
//!   beat it. A node takes any paid state that beats what it holds, so a
//!   counter may skip, a peer that missed updates takes the next one, and two
//!   states at one counter converge on the smaller target until a later
//!   counter replaces both.
//! - **Paying at the state, not the address.** The quote names `state_id`, while
//!   the close group that issues it is the one around the address. Paying at the
//!   address would buy every future update at once.
//! - **Following targets.** A node never interprets a pointer's target — it
//!   stores 33 opaque bytes. Chain resolution, its depth limit and its cycle
//!   detection are entirely here: see [`Client::pointer_resolve`].

use std::collections::HashSet;
use std::future::Future;

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

use crate::data::client::chunk::STORE_RESPONSE_TIMEOUT;
use crate::data::client::Client;
use crate::data::error::{Error, Result};

/// How many of `peers` must answer a read.
///
/// Derived from the group actually returned, not from a fixed constant: the
/// close-group width is configurable, and a fixed four against a width of
/// twenty would let a write land on four peers and a *disjoint* four answer the
/// read. Quorums only intersect if both are taken from the same set.
fn read_quorum(peers: usize) -> usize {
    (peers / 2) + 1
}

/// How many of `peers` a write must reach.
///
/// One wider than a bare majority, which is what a chunk needs. A chunk is
/// self-proving — one copy is *the* chunk for that address — but a pointer read
/// has to decide which of several signed states is current, and a state that
/// only one peer reports is a state one peer decided. So a read requires two
/// peers to report what it returns ([`corroboration`]), and the write is one
/// wider so that two of them always answer: with `|W| + |R| >= K + 2` the two
/// sets overlap in at least two peers.
fn write_quorum(peers: usize) -> usize {
    (read_quorum(peers) + 1).min(peers)
}

/// How many peers must report a state before a read will return it.
///
/// Two, so that no single peer decides what a pointer says. A peer that is in
/// the close group and willing to serve what the owner never paid to store
/// could otherwise answer first with an owner-signed state no honest node
/// holds, and every reader would take it: the record verifies, it belongs at
/// the address, and it wins the merge. What it cannot do is make a second peer
/// agree.
///
/// A group of one can only ever be corroborated by itself.
fn corroboration(peers: usize) -> usize {
    peers.min(2)
}

/// The outcome of asking a close group.
struct Answered {
    /// How many peers gave a usable answer.
    count: usize,
    /// The last failure, kept only to explain a total failure.
    last_error: Option<Error>,
}

/// Ask a whole close group at once and stop as soon as there is an answer.
///
/// The shape a pointer write and a pointer read share: every peer is asked
/// concurrently and each answer is handed to `keep`, which says whether what it
/// has is settled. It ends at the first answer that is both settled and the
/// `wanted`-th, or when the group is exhausted. Stopping early is what keeps one
/// unreachable peer from holding an operation open for its whole timeout after
/// the answer is already decided; the requests still in flight are dropped,
/// which cancels them.
async fn ask_the_group<T>(
    mut in_flight: FuturesUnordered<impl Future<Output = Result<T>>>,
    wanted: usize,
    mut keep: impl FnMut(T) -> bool,
) -> Answered {
    let mut answered = Answered {
        count: 0,
        last_error: None,
    };
    while let Some(result) = in_flight.next().await {
        match result {
            Ok(answer) => {
                answered.count += 1;
                let settled = keep(answer);
                if settled && answered.count >= wanted {
                    break;
                }
            }
            Err(e) => answered.last_error = Some(e),
        }
    }
    answered
}

/// What the peers answering a read have said.
///
/// A tally of every state offered, not a running winner. Keeping only the best
/// state and its count would let one peer erase the agreement behind another:
/// a single peer naming a higher state — which it cannot make anyone else
/// confirm — would discard the count behind the state the rest of the group
/// holds, and a healthy pointer would read back as uncorroborated. That is not
/// only an attack; it is what an ordinary read during an update looks like.
///
/// At most one entry per peer, so this is never longer than the close group.
#[derive(Default)]
struct Replies {
    /// Each distinct state offered, and how many peers offered it.
    seen: Vec<(Pointer, usize)>,
}

impl Replies {
    /// Record one peer's answer. `None` is an answer that names no state.
    fn add(&mut self, reply: Option<Pointer>) {
        let Some(reply) = reply else { return };
        for (held, count) in &mut self.seen {
            if held.state_id() == reply.state_id() {
                *count += 1;
                return;
            }
        }
        self.seen.push((reply, 1));
    }

    /// The best state that at least `needed` peers named.
    ///
    /// Best by the merge rule, among the corroborated only: a stale state the
    /// group agrees on beats a newer one only one peer has heard of, because
    /// the second is one peer's word and the first is the network's.
    fn corroborated(&self, needed: usize) -> Option<&Pointer> {
        self.seen
            .iter()
            .filter(|(_, count)| *count >= needed)
            .map(|(record, _)| record)
            .reduce(|best, next| if next.replaces(best) { next } else { best })
    }

    /// Whether any peer named a state at all.
    fn any(&self) -> bool {
        !self.seen.is_empty()
    }
}

/// What one peer's reply means for a write.
///
/// `None` means the message was not a reply to this request at all, and the
/// caller keeps waiting.
///
/// A storer that acknowledges some *other* record is claiming to hold something
/// this client never sent. Taking that at face value would let one peer end the
/// write — after payment — while storing nothing, so an acknowledgement only
/// counts when it names the record that was sent.
fn read_put_reply(
    body: ChunkMessageBody,
    expected_address: XorName,
    expected_state: XorName,
) -> Option<Result<()>> {
    let ChunkMessageBody::PointerPutResponse(response) = body else {
        return None;
    };
    Some(match response {
        // `Unchanged` is success for a retry: the state the client signed is
        // exactly what the node holds.
        PointerPutResponse::Success { address, state_id }
        | PointerPutResponse::Unchanged { address, state_id } => {
            if address == expected_address && state_id == expected_state {
                Ok(())
            } else {
                Err(Error::InvalidData(format!(
                    "peer acknowledged a pointer this client did not send: address {} state {}",
                    hex::encode(address),
                    hex::encode(state_id)
                )))
            }
        }
        // Every reply names the address it is about, and every one of them is
        // checked against the address that was sent. A refusal is not exempt:
        // one about some other pointer says nothing about this write.
        PointerPutResponse::Stale { address, state_id } => {
            Err(Error::InvalidData(if address == expected_address {
                format!(
                    "the pointer moved while this update was in flight; the network \
                     now holds state {}",
                    hex::encode(state_id)
                )
            } else {
                format!(
                    "peer refused a pointer this client did not send: address {}",
                    hex::encode(address)
                )
            }))
        }
        PointerPutResponse::PaymentRequired { message } => Err(Error::Payment(message)),
        PointerPutResponse::Error(e) => Err(Error::Protocol(format!("pointer PUT refused: {e}"))),
    })
}

/// What one peer's reply means for a read.
///
/// Verify before trusting: the signature must check out and the record must
/// belong at the address that was asked for, or a storer could answer with
/// someone else's pointer, or with bytes nobody signed.
fn read_get_reply(body: ChunkMessageBody, address: XorName) -> Option<Result<Option<Pointer>>> {
    let ChunkMessageBody::PointerGetResponse(response) = body else {
        return None;
    };
    Some(match response {
        PointerGetResponse::Success { record } => match Pointer::from_bytes(&record) {
            Ok(record) if record.address() == address => Ok(Some(record)),
            Ok(_) => Err(Error::InvalidData(
                "peer answered with a pointer for a different address".to_string(),
            )),
            Err(e) => Err(Error::InvalidData(format!("invalid pointer: {e}"))),
        },
        // "I do not have it" still has to be about the address that was asked
        // for, exactly as an acknowledgement does: an answer naming something
        // else is not an answer to this request.
        PointerGetResponse::NotFound { address: absent } => {
            if absent == address {
                Ok(None)
            } else {
                Err(Error::InvalidData(format!(
                    "peer reported a different address absent: {}",
                    hex::encode(absent)
                )))
            }
        }
        PointerGetResponse::Error(e) => Err(Error::Protocol(format!("pointer GET refused: {e}"))),
    })
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
    /// Reads the current record first, because the update has to beat what the
    /// network serves: it is signed one past that counter. A pointer that does
    /// not exist yet is created at 0.
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
    /// Each call settles a payment for the state it is given. A state the
    /// network already holds is answered as stored, but the payment for it has
    /// been made either way — so a retry after a timeout costs a second quote.
    /// Read first if that matters; the node cannot tell a retry from a fresh
    /// submission before it has been paid to look.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer is responsible for the address, if payment
    /// fails, or if no storer accepts the record.
    pub async fn pointer_put(&self, record: &Pointer) -> Result<XorName> {
        let address = record.address();
        let state_id = record.state_id();

        // Refuse before paying if nobody is responsible for this address.
        // There is no sense buying storage with nowhere to put it, and a
        // quorum of nothing would otherwise be satisfied by nothing — a paid
        // write reported as stored on zero peers.
        self.pointer_group(&address).await?;

        // The quote names the state; the peers that may issue it are the close
        // group around the address. One payment, one state.
        let (proof, _) = self
            .pay_for_storage_split(
                &address,
                &state_id,
                POINTER_WIRE_LEN as u64,
                DATA_TYPE_POINTER,
            )
            .await?;

        // Ask again rather than keep the set from before the payment. Settling
        // on chain takes time, and the peers responsible for an address can
        // change while it does; writing to the old set would then be refused by
        // peers that are no longer responsible while the ones that are were
        // never asked. The lookup above answered whether there was anywhere to
        // store this — this one answers where.
        let targets = self.pointer_group(&address).await?;
        let wanted = write_quorum(targets.len());

        let request = PointerPutRequest::with_payment(Bytes::from(record.to_bytes()), proof);
        let in_flight = FuturesUnordered::new();
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

        let answered = ask_the_group(in_flight, wanted, |()| true).await;
        if answered.count >= wanted {
            return Ok(address);
        }
        if answered.count > 0 {
            return Err(Error::CloseGroupShortfall(format!(
                "pointer {} stored on {} of {wanted} close-group peers",
                hex::encode(address),
                answered.count
            )));
        }
        Err(answered.last_error.unwrap_or_else(|| {
            Error::Protocol("no close-group peer accepted the pointer".to_string())
        }))
    }

    /// The peers a pointer write must land on and a read must ask.
    ///
    /// One definition serves both. A write acknowledged outside the set the
    /// read queries is a write nobody can read — paid for, stored, and
    /// invisible — so both go through here rather than each deciding for
    /// itself. In particular the payment plan's targets, which can run far
    /// wider than the close group, never count towards a write.
    ///
    /// Same definition, not the same answer: a read does its own lookup, so
    /// membership that changed in between is not covered. That is churn, and
    /// what covers it is replication, which is not built.
    async fn pointer_group(&self, address: &XorName) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        let peers = self
            .network()
            .find_closest_peers(address, self.config().close_group_size)
            .await?;
        if peers.is_empty() {
            // A lookup that succeeds and returns nobody is not somewhere a
            // pointer can live. Saying so here keeps every caller from having
            // to notice that a quorum of an empty group is zero.
            return Err(Error::CloseGroupShortfall(format!(
                "no peer is responsible for pointer {}",
                hex::encode(address)
            )));
        }
        Ok(peers)
    }

    /// Read the pointer at `address`, verifying it before returning it.
    ///
    /// The record is checked against the address it was asked for, so a storer
    /// cannot answer with a different owner's pointer, and the state returned
    /// must be named by more than one of them, so no single peer decides what
    /// the pointer says.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer answers, if too few of the group do, or if
    /// the best state found has too few peers behind it to be the network's
    /// answer rather than one peer's.
    pub async fn pointer_get(&self, address: &XorName) -> Result<Option<Pointer>> {
        let peers = self.pointer_group(address).await?;

        // Ask the whole close group at once and keep the best answer.
        //
        // Every reply is verified independently, so a dishonest peer can only
        // offer a record that is genuinely signed and genuinely belongs here.
        // What it must not be able to do is decide the answer alone: any peer
        // may legitimately hold a stale record, so taking the first reply would
        // let one pin a reader to it. Asking concurrently also means one slow
        // peer cannot stall the read behind the store timeout.
        let in_flight = FuturesUnordered::new();
        for (peer_id, addrs) in &peers {
            in_flight.push(self.send_pointer_get(*address, *peer_id, addrs.clone()));
        }

        let wanted = read_quorum(peers.len());
        let needed = corroboration(peers.len());
        let mut replies = Replies::default();
        let answered = ask_the_group(in_flight, wanted, |found| {
            replies.add(found);
            // Answers alone settle a read that found nothing. A state is
            // settled only once enough peers have named it — until then the
            // read keeps asking, because the peers that would confirm it may
            // simply not have answered yet.
            !replies.any() || replies.corroborated(needed).is_some()
        })
        .await;
        let corroborated = replies.corroborated(needed).cloned();

        if answered.count == 0 {
            return Err(answered.last_error.unwrap_or_else(|| {
                Error::Protocol("no close-group peer answered for the pointer".to_string())
            }));
        }
        // A single answer is not a network verdict: with no replication, one
        // reachable peer holding a stale record looks exactly like the whole
        // group agreeing. Say so rather than presenting it as the value.
        if answered.count < wanted {
            return Err(Error::CloseGroupShortfall(format!(
                "only {} of the close group answered for pointer {}",
                answered.count,
                hex::encode(address)
            )));
        }
        // States were offered, but none by enough peers to call it the
        // network's answer. Either the write is still settling, or what was
        // offered is a state no honest node holds. Neither is a value to hand
        // back as current.
        if corroborated.is_none() && replies.any() {
            return Err(Error::CloseGroupShortfall(format!(
                "no state for pointer {} was named by {needed} of the {} peers that answered",
                hex::encode(address),
                answered.count
            )));
        }
        Ok(corroborated)
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
    /// Returns an error if a hop is missing, the chain cycles, or the depth
    /// limit is reached. A target of a kind this build does not know is *not*
    /// an error: the record is signed, so the bytes are authentic, and the
    /// caller is handed the target to decide about.
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
            // A pointer PUT carries a single-node proof — merkle proofs are
            // refused for pointers — so it does not make the storer do the
            // network closeness lookup the merkle timeout exists to cover.
            // Same budget as a non-merkle chunk PUT.
            STORE_RESPONSE_TIMEOUT,
            &peer_addrs,
            move |body| read_put_reply(body, expected_address, expected_state),
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
            move |body| read_get_reply(body, address),
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
    use futures::future::BoxFuture;

    fn keypair(seed: u8) -> (MlDsaPublicKey, MlDsaSecretKey) {
        ml_dsa_65().generate_keypair_from_seed(&[seed; 32])
    }

    fn chunk_target(byte: u8) -> PointerTarget {
        PointerTarget::new(PointerTargetKind::Chunk, [byte; 32])
    }

    /// A create is counter 0 and each update the client builds is one more,
    /// the smallest counter that beats the record it read.
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

    /// Every reply a peer can give to a read, fed to the code that judges them.
    #[test]
    fn a_read_refuses_every_reply_but_this_address_own_signed_record() {
        let (mine, my_sk) = keypair(5);
        let (theirs, their_sk) = keypair(6);
        let asked_for = pointer_address(&mine);
        let ours = Pointer::create(&my_sk, &mine, chunk_target(1)).expect("create");
        let theirs = Pointer::create(&their_sk, &theirs, chunk_target(1)).expect("create");

        let reply = |record: Bytes| {
            read_get_reply(
                ChunkMessageBody::PointerGetResponse(PointerGetResponse::Success { record }),
                asked_for,
            )
        };

        let accepted = reply(Bytes::from(ours.to_bytes()));
        assert_eq!(
            accepted
                .expect("a reply")
                .expect("valid")
                .expect("present")
                .state_id(),
            ours.state_id()
        );

        // Someone else's pointer: signed, genuine, and not what was asked for.
        assert!(reply(Bytes::from(theirs.to_bytes()))
            .expect("a reply")
            .is_err());

        // Tampering with any byte breaks the signature, so nothing parses.
        let mut tampered = ours.to_bytes();
        tampered[100] ^= 0xff;
        assert!(reply(Bytes::from(tampered)).expect("a reply").is_err());
        assert!(reply(Bytes::new()).expect("a reply").is_err());

        // "I do not have it" is an answer, and counts towards the quorum —
        // but only about the address that was asked for.
        assert!(read_get_reply(
            ChunkMessageBody::PointerGetResponse(PointerGetResponse::NotFound {
                address: asked_for
            }),
            asked_for,
        )
        .expect("a reply")
        .expect("valid")
        .is_none());
        assert!(read_get_reply(
            ChunkMessageBody::PointerGetResponse(PointerGetResponse::NotFound {
                address: [0u8; 32]
            }),
            asked_for,
        )
        .expect("a reply")
        .is_err());

        // A message that is not a reply to this request is not an answer at all.
        assert!(read_get_reply(
            ChunkMessageBody::PointerPutResponse(PointerPutResponse::Success {
                address: asked_for,
                state_id: ours.state_id(),
            }),
            asked_for,
        )
        .is_none());
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

        let judged = |response| {
            read_put_reply(
                ChunkMessageBody::PointerPutResponse(response),
                sent.address(),
                sent.state_id(),
            )
            .expect("a reply")
        };

        for honest in [
            PointerPutResponse::Success {
                address: sent.address(),
                state_id: sent.state_id(),
            },
            // A re-submission of a state the node already holds: the record is
            // stored, which is all the write asked for.
            PointerPutResponse::Unchanged {
                address: sent.address(),
                state_id: sent.state_id(),
            },
        ] {
            assert!(judged(honest).is_ok());
        }

        for lie in [
            // The right address, some other state of the same pointer.
            PointerPutResponse::Success {
                address: sent.address(),
                state_id: other.state_id(),
            },
            // The right state, at an address that was never written.
            PointerPutResponse::Success {
                address: [0u8; 32],
                state_id: sent.state_id(),
            },
            PointerPutResponse::Unchanged {
                address: sent.address(),
                state_id: other.state_id(),
            },
            // Losing a race is not storing.
            PointerPutResponse::Stale {
                address: sent.address(),
                state_id: other.state_id(),
            },
            // Nor is a refusal about somebody else's pointer.
            PointerPutResponse::Stale {
                address: [0u8; 32],
                state_id: other.state_id(),
            },
            PointerPutResponse::PaymentRequired {
                message: "pay up".to_string(),
            },
        ] {
            assert!(judged(lie).is_err(), "this must not count as stored");
        }
    }

    /// Tally some replies and ask what the group's answer is.
    fn tally(replies: Vec<Option<Pointer>>) -> Replies {
        let mut seen = Replies::default();
        for reply in replies {
            seen.add(reply);
        }
        seen
    }

    /// Reads take the best answer, not the first: any single peer is entitled
    /// to serve a stale record, and must not be able to pin a reader to it.
    #[test]
    fn a_read_keeps_the_winner_whatever_order_replies_arrive_in() {
        let (pk, sk) = keypair(10);
        let old = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let new = old.update(&sk, chunk_target(2)).expect("update");
        let old = || Some(old.clone());
        let new = || Some(new.clone());

        for order in [
            vec![old(), old(), new(), new()],
            vec![new(), new(), old(), old()],
            vec![None, old(), new(), None, new(), old()],
            vec![new(), None, old(), new(), old()],
        ] {
            assert_eq!(
                tally(order).corroborated(2).expect("an answer").state_id(),
                new().expect("new").state_id(),
                "the newest corroborated state wins however the replies interleave"
            );
        }
        assert!(
            tally(vec![None, None]).corroborated(2).is_none(),
            "nobody holds it"
        );
        assert!(!tally(vec![None, None]).any(), "and nobody named a state");
    }

    /// One peer naming a higher state must not bury the state the rest of the
    /// group agrees on.
    ///
    /// Two ways to arrive here. A bad peer offers an owner-signed state nobody
    /// paid to store: it can no longer make the client return it, but if it
    /// could discard the count behind the real state it would make the pointer
    /// unreadable instead — a denial of service in place of a forgery. And an
    /// ordinary read taken while an update is in flight looks exactly the same.
    #[test]
    fn a_higher_state_only_one_peer_has_does_not_bury_the_agreed_one() {
        let (pk, sk) = keypair(12);
        let agreed = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let singleton = Pointer::sign(&sk, &pk, 900, chunk_target(9)).expect("sign");
        assert!(singleton.replaces(&agreed), "it does outrank what is held");

        let agreed_reply = || Some(agreed.clone());
        let singleton_reply = || Some(singleton.clone());

        for order in [
            vec![singleton_reply(), agreed_reply(), agreed_reply()],
            vec![agreed_reply(), singleton_reply(), agreed_reply()],
            vec![agreed_reply(), agreed_reply(), singleton_reply()],
            vec![
                singleton_reply(),
                None,
                agreed_reply(),
                agreed_reply(),
                agreed_reply(),
            ],
        ] {
            let seen = tally(order);
            assert_eq!(
                seen.corroborated(2).expect("an answer").state_id(),
                agreed.state_id(),
                "the state the group agrees on is the answer, whenever the \
                 singleton arrives"
            );
        }

        // And once a second peer confirms it, it is the answer — that is the
        // bar, and two colluding peers can clear it. See ADR-0016.
        let seen = tally(vec![
            singleton_reply(),
            singleton_reply(),
            agreed_reply(),
            agreed_reply(),
        ]);
        assert_eq!(
            seen.corroborated(2).expect("an answer").state_id(),
            singleton.state_id()
        );
    }

    /// The fan-out both a write and a read use, driven with synthetic replies.
    ///
    /// A majority ends the operation. The peers that have not answered are
    /// dropped mid-flight, so one unreachable peer cannot hold the operation
    /// open for its whole timeout after the answer is already decided.
    #[tokio::test]
    async fn a_majority_ends_the_operation_and_a_dead_peer_cannot_stall_it() {
        let group = |good: usize, dead: usize| {
            let in_flight: FuturesUnordered<BoxFuture<'static, Result<Option<Pointer>>>> =
                FuturesUnordered::new();
            for _ in 0..good {
                in_flight.push(Box::pin(async { Ok(None) }));
            }
            for _ in 0..dead {
                in_flight.push(Box::pin(futures::future::pending()));
            }
            in_flight
        };

        // Four of seven answer; three never will. Without the early return this
        // would never finish.
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ask_the_group(group(4, 3), read_quorum(7), |_| true),
        )
        .await
        .expect("a majority must end the read without waiting for the rest");
        assert_eq!(answered.count, 4);

        // One short of a majority, and the rest are gone: the caller is told
        // how many answered rather than being handed one peer's word.
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            ask_the_group(group(3, 0), read_quorum(7), |_| true),
        )
        .await
        .expect("nothing is in flight to wait for");
        assert_eq!(answered.count, 3, "three is not a majority of seven");
    }

    /// A peer that errors has not answered, and cannot make up a quorum.
    #[tokio::test]
    async fn an_error_is_not_an_answer() {
        let in_flight: FuturesUnordered<BoxFuture<'static, Result<Option<Pointer>>>> =
            FuturesUnordered::new();
        for _ in 0..6 {
            in_flight.push(Box::pin(async {
                Err(Error::Protocol("peer refused".to_string()))
            }));
        }
        in_flight.push(Box::pin(async { Ok(None) }));

        let answered = ask_the_group(in_flight, read_quorum(7), |_| true).await;
        assert_eq!(answered.count, 1, "six failures are not six answers");
        assert!(
            answered.last_error.is_some(),
            "the failure must be kept to explain the shortfall"
        );
    }

    /// A write quorum and a read quorum must intersect at every group width.
    ///
    /// This is the property a fixed four-of-K silently broke: at K=20 a write
    /// on four peers and a read of a disjoint four would never meet, so an
    /// acknowledged pointer could read back as missing.
    #[test]
    fn write_and_read_quorums_overlap_by_the_corroboration_a_read_demands() {
        // The whole basis of the read rule: a state a write actually landed
        // must be reported by at least as many answering peers as a read
        // insists on, or a legitimate pointer would read back as unconfirmed.
        for k in 1usize..=64 {
            let write = write_quorum(k);
            let read = read_quorum(k);
            let needed = corroboration(k);
            assert!(
                write + read >= k + needed,
                "at width {k} a {write}-write and a {read}-read overlap in fewer \
                 than the {needed} peers a read requires"
            );
            assert!(write <= k, "a quorum cannot need more peers than exist");
            assert!(read <= k);
            assert!(
                needed <= read,
                "a read cannot need more backers than answers"
            );
        }
    }

    /// The default close group is seven: four answers decide a read, five
    /// stores make a write, and two peers must name what a read returns.
    #[test]
    fn the_default_group_reads_on_four_and_writes_on_five() {
        assert_eq!(read_quorum(7), 4);
        assert_eq!(write_quorum(7), 5);
        assert_eq!(corroboration(7), 2);

        assert_eq!(read_quorum(20), 11);
        assert_eq!(write_quorum(20), 12);

        // A group of one cannot corroborate itself with anyone else.
        assert_eq!(read_quorum(1), 1);
        assert_eq!(write_quorum(1), 1);
        assert_eq!(corroboration(1), 1);
    }

    /// A quorum of nothing is satisfied by nothing.
    ///
    /// `write_quorum(0)` is zero, so a write to an empty group would count zero
    /// acknowledgements as enough and report a paid record as stored on no
    /// peers at all. Both paths refuse an empty group before that arithmetic
    /// is ever reached.
    #[test]
    fn an_empty_group_is_not_a_quorum() {
        assert_eq!(
            write_quorum(0),
            0,
            "which is exactly why an empty group must be refused earlier"
        );
        assert_eq!(corroboration(0), 0, "and nothing can corroborate nothing");
        for k in 1usize..=64 {
            assert!(write_quorum(k) >= 1, "a real group always needs a storer");
            assert!(
                corroboration(k) >= 1,
                "and a real answer always needs a peer"
            );
        }
    }

    /// One peer must not be able to decide what a pointer says.
    ///
    /// The attack this closes: an owner signs a state without paying for it and
    /// one close-group peer serves it. Every honest peer says `NotFound`, the
    /// record verifies and belongs at the address, and it wins the merge — so a
    /// read that took the best answer regardless of who backed it would hand
    /// back a state the network never stored.
    #[test]
    fn a_state_only_one_peer_reports_is_not_the_networks_answer() {
        let (pk, sk) = keypair(11);
        let unpaid = Pointer::sign(&sk, &pk, 900, chunk_target(9)).expect("sign");
        let stored = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");

        // One peer offers the unpaid state; the rest of the close group has
        // never heard of it.
        let seen = tally(vec![Some(unpaid.clone()), None, None, None]);
        assert!(seen.any(), "a state was offered");
        assert!(
            seen.corroborated(corroboration(7)).is_none(),
            "but one peer naming it is one peer deciding, not an answer"
        );

        // A state the group really holds clears the bar.
        let seen = tally(vec![Some(stored.clone()), Some(stored.clone()), None]);
        assert_eq!(
            seen.corroborated(corroboration(7))
                .expect("an answer")
                .state_id(),
            stored.state_id()
        );
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
