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
//! - **Deciding between final states.** A state at the final counter is
//!   replaced by nothing, which is what lets an owner hand a pointer over for
//!   good ([`Client::pointer_transfer`], ADR-0018 in `ant-node`). Two different
//!   final states are the one fork left, made only by an owner racing them to
//!   different nodes, and each node keeps the one it took first. A read that
//!   meets a final state returns the one a majority of the close group holds,
//!   and reports a fork when none does; [`Client::pointer_finality`] asks the
//!   whole group, so a recipient can check a transfer before relying on it.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::future::Future;

use ant_protocol::chunk::{
    ChunkMessage, ChunkMessageBody, PointerGetRequest, PointerGetResponse, PointerPutRequest,
    PointerPutResponse,
};
use ant_protocol::pointer::{
    pointer_address, Pointer, PointerError, PointerTarget, PointerTargetKind, DATA_TYPE_POINTER,
    FINAL_COUNTER, POINTER_WIRE_LEN,
};
use ant_protocol::pqc::api::{MlDsaPublicKey, MlDsaSecretKey};
use ant_protocol::transport::{MultiAddr, PeerId};
use ant_protocol::XorName;
use bytes::Bytes;
use futures::stream::{FuturesUnordered, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::warn;
use web_time::Duration;

use crate::client_engine::{store_retry_delay, STORE_MAX_RETRIES};
use crate::data::client::batch::ChunkPaymentPlan;
use crate::data::client::chunk::STORE_RESPONSE_TIMEOUT;
use crate::data::client::Client;
use crate::data::error::{Error, Result};
use crate::data::network::send_and_await_chunk_response;
use crate::runtime::sleep;

/// How many of `peers` must answer a read.
///
/// `peers` is the width from [`quorum_width`]: the configured close group, or
/// more peers than that if a lookup returned more, never fewer. It is not a
/// fixed constant, because the close-group width is configurable, and a fixed
/// four against a width of twenty would let a write land on four peers and a
/// *disjoint* four answer the read. Quorums only intersect if both are taken
/// from the same set.
///
/// It is also the bare majority of the group, which is what decides between
/// two final states: at most one can be held by a majority.
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

/// How many members the quorums of an operation on `found` peers are counted
/// over: the configured close group, however few a lookup returned.
///
/// A lookup can come back short, while a node's view is thin or the network
/// churns. Counting quorums over what it returned would shrink them with it:
/// one returned peer would make a write of one copy complete, and a read that
/// one peer answers would need no second peer to agree. Counted over the whole
/// group, a short lookup is a shortfall instead.
fn quorum_width(found: usize, close_group_size: usize) -> usize {
    found.max(close_group_size)
}

/// The outcome of asking a close group.
struct Answered {
    /// How many peers gave a usable answer.
    count: usize,
    /// The last failure that was not a refusal, kept to explain a total
    /// failure.
    last_error: Option<Error>,
    /// The first failure that was a refusal rather than a missing answer: a
    /// peer that answered and said no. Asking again would get the same.
    refusal: Option<Error>,
    /// How many peers refused.
    refusals: usize,
}

impl Answered {
    /// What to report when too few answered: a refusal if any peer gave one,
    /// since it says why, else the last failure.
    fn failure(self) -> Option<Error> {
        self.refusal.or(self.last_error)
    }
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
        refusal: None,
        refusals: 0,
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
            Err(e) if !worth_retrying(&e) => {
                answered.refusals += 1;
                if answered.refusal.is_none() {
                    answered.refusal = Some(e);
                } else {
                    answered.last_error = Some(e);
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

    /// The distinct final states named, with how many peers named each.
    fn finals(&self) -> impl Iterator<Item = &(Pointer, usize)> {
        self.seen.iter().filter(|(record, _)| record.is_terminal())
    }

    /// The final state at least `majority` peers named. At most one can be,
    /// so long as `majority` is a majority of the group.
    fn final_majority(&self, majority: usize) -> Option<&Pointer> {
        self.finals()
            .find(|(_, count)| *count >= majority)
            .map(|(record, _)| record)
    }

    /// What the tally says the pointer holds.
    ///
    /// Below the final counter, the best corroborated state. Once a final state
    /// has been named the merge rule no longer answers on its own: two
    /// different final states are unordered, and each node keeps whichever it
    /// took first. So the one a majority of the group holds is the answer, as
    /// it is the one most nodes accepted first. Without a majority, two final
    /// states are a fork with no answer, and one final state is taken as any
    /// state is — once corroborated — since nothing contests it.
    fn chosen(&self, needed: usize, majority: usize) -> Chosen<'_> {
        if let Some(held) = self.final_majority(majority) {
            return Chosen::State(Some(held));
        }
        if self.finals().nth(1).is_some() {
            return Chosen::Forked;
        }
        Chosen::State(self.corroborated(needed))
    }

    /// Whether what has been heard settles the read.
    ///
    /// Answers alone settle a read that found nothing. A state is settled only
    /// once enough peers have named it: until then the read keeps asking,
    /// because the peers that would confirm it may simply not have answered yet.
    ///
    /// Nor is it settled while a state that would replace it has been named
    /// by too few. The write and read quorums overlap in two peers, not two
    /// honest ones: after a write that reached a bare write quorum, the peers
    /// it missed hold the older state, and one more peer replaying that state
    /// corroborates it before a second holder of the newer state has
    /// answered. So the read waits for the rest of the group before letting
    /// the older state stand. The newer state still has to be corroborated to
    /// be returned, so no single peer decides the answer; one that names a
    /// state nobody else holds only makes the read ask everyone.
    ///
    /// A final state changes what settles a read. Nothing replaces it, but a
    /// different final state need not replace it to win: the one most of the
    /// group holds does. So once any final state has been named, the read is
    /// settled only by a final state a `majority` of the group has named, and
    /// otherwise asks everyone, so a fork is seen rather than guessed past.
    fn settled(&self, needed: usize, majority: usize) -> bool {
        if self.finals().next().is_some() {
            return self.final_majority(majority).is_some();
        }
        match self.corroborated(needed) {
            None => !self.any(),
            Some(best) => !self
                .seen
                .iter()
                .any(|(record, count)| *count < needed && record.replaces(best)),
        }
    }
}

/// What a read's tally settles on (see [`Replies::chosen`]).
#[derive(Debug)]
enum Chosen<'a> {
    /// This state, or none at all.
    State(Option<&'a Pointer>),
    /// Two or more different final states, none held by a majority.
    Forked,
}

/// Ask a close group of `peers` for a pointer, tallying the states they name.
///
/// Ends once a read quorum has answered and the tally is settled (see
/// [`Replies::settled`]), or when the group is exhausted.
async fn collect_read(
    in_flight: FuturesUnordered<impl Future<Output = Result<Option<Pointer>>>>,
    peers: usize,
) -> (Answered, Replies) {
    let needed = corroboration(peers);
    let majority = read_quorum(peers);
    let mut replies = Replies::default();
    let answered = ask_the_group(in_flight, read_quorum(peers), |found| {
        replies.add(found);
        replies.settled(needed, majority)
    })
    .await;
    (answered, replies)
}

/// A final state, as [`Client::pointer_finality`] reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalState {
    /// The state's identifier.
    pub state_id: XorName,
    /// The target's raw kind tag.
    pub kind_tag: u8,
    /// The target's address.
    pub target: XorName,
    /// How many peers of the close group hold it.
    pub holders: usize,
}

impl FinalState {
    fn of(record: &Pointer, holders: usize) -> Self {
        let target = record.target();
        Self {
            state_id: record.state_id(),
            kind_tag: target.kind_tag(),
            target: target.address,
            holders,
        }
    }

    /// The pointer this state hands the address over to, if it is a transfer
    /// rather than a freeze.
    #[must_use]
    pub fn transferred_to(&self) -> Option<XorName> {
        match PointerTargetKind::from_tag(self.kind_tag) {
            Some(PointerTargetKind::Pointer) => Some(self.target),
            _ => None,
        }
    }
}

/// Where a pointer stands with respect to finality, as the whole close group
/// reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum FinalityStatus {
    /// No peer that answered holds a final state: the owner can still move
    /// the pointer. A peer that did not answer could hold one, so this rules a
    /// final state out only when every peer of the group answered
    /// ([`PointerFinality::answered`] equals [`PointerFinality::group`]).
    /// `counter` is the corroborated state's, or `None` if no state is.
    Open {
        /// The counter of the state a read would return.
        counter: Option<u64>,
    },
    /// One final state, and no other, but a majority of the group does not
    /// hold it yet: a transfer still spreading, or a write that fell short.
    Settling(FinalState),
    /// One final state, held by a majority of the group, and no other: every
    /// peer of the group answered and none holds a rival. Nothing can move
    /// the pointer off it.
    Final(FinalState),
    /// One final state, held by a majority of the group, and no other among
    /// the peers that answered, but not every peer of the group answered. A
    /// rival held by one that did not would not show, so this is not yet
    /// [`Self::Final`]; ask again once the whole group can answer.
    Unconfirmed(FinalState),
    /// The owner signed two or more different final states. `majority` is
    /// the one a majority holds, which reads return; with none, reads fail.
    Forked {
        /// Every final state seen, most-held first.
        states: Vec<FinalState>,
        /// The state a majority of the group holds, if any.
        majority: Option<FinalState>,
    },
}

/// The answer to [`Client::pointer_finality`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerFinality {
    /// The pointer asked about.
    pub address: XorName,
    /// How many peers the group has: the configured close group, or more if
    /// a lookup returned more.
    pub group: usize,
    /// How many of them answered.
    pub answered: usize,
    /// What they said.
    pub status: FinalityStatus,
}

impl PointerFinality {
    /// Whether the pointer is final beyond dispute: one final state, held by a
    /// majority of the group, with no rival seen.
    #[must_use]
    pub fn is_final(&self) -> bool {
        matches!(self.status, FinalityStatus::Final(_))
    }

    /// The pointer this one was handed over to, once that is beyond dispute.
    #[must_use]
    pub fn transferred_to(&self) -> Option<XorName> {
        match &self.status {
            FinalityStatus::Final(state) => state.transferred_to(),
            _ => None,
        }
    }
}

/// What [`Client::pointer_transfer`] stored, and what the close group said
/// about it afterwards.
#[derive(Debug)]
pub struct PointerTransfer {
    /// The pointer handed over.
    pub address: XorName,
    /// The final state stored.
    pub state_id: XorName,
    /// The whole close group's view once the write landed. An error here does
    /// not undo the transfer, which is stored and final; only the check
    /// failed. Ask again with [`Client::pointer_finality`].
    pub finality: Result<PointerFinality>,
}

/// Whose pointer decides what an address resolves to, after following every
/// transfer (see [`Client::pointer_controller`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PointerController {
    /// The pointer whose owner decides: the address asked about, or the last
    /// pointer it was handed over to.
    pub pointer: XorName,
    /// Every pointer the address was handed over to, in order.
    pub transfers: Vec<XorName>,
    /// Whether that pointer is final without being handed over: nobody can
    /// move it any more.
    pub frozen: bool,
}

/// A final state's description for an error: its identifier and target.
fn describe_final(record: &Pointer) -> String {
    let target = record.target();
    let kind = match target.kind() {
        Some(PointerTargetKind::Pointer) => "pointer",
        Some(PointerTargetKind::Chunk) => "chunk",
        None => "unknown kind",
    };
    format!(
        "final state {} targeting {kind} {}",
        hex::encode(record.state_id()),
        hex::encode(target.address)
    )
}

/// The error for a transfer the network refused because the pointer is
/// already final on another state, if that is what `finality` shows.
fn rival_final(finality: &PointerFinality, ours: &Pointer) -> Option<Error> {
    let ours = ours.state_id();
    let address = hex::encode(finality.address);
    match &finality.status {
        FinalityStatus::Open { .. } => None,
        FinalityStatus::Settling(state)
        | FinalityStatus::Final(state)
        | FinalityStatus::Unconfirmed(state) => (state.state_id != ours).then(|| {
            Error::PointerFinal(format!(
                "pointer {address} is already final on state {} held by {} of {} peers",
                hex::encode(state.state_id),
                state.holders,
                finality.group
            ))
        }),
        FinalityStatus::Forked { states, .. } => Some(Error::PointerForked(format!(
            "pointer {address} holds {} different final states",
            states.len()
        ))),
    }
}

/// The error for an update to a pointer on which any peer holds a final
/// state, if that is what `finality` shows.
///
/// A final state even one peer holds was signed by the owner, and no node
/// holding it will take an update: a transfer or freeze that fell short.
fn final_already(finality: &PointerFinality) -> Option<Error> {
    let address = hex::encode(finality.address);
    match &finality.status {
        FinalityStatus::Open { .. } => None,
        FinalityStatus::Settling(state)
        | FinalityStatus::Final(state)
        | FinalityStatus::Unconfirmed(state) => Some(Error::PointerFinal(format!(
            "pointer {address} has final state {} on {} of {} peers; an update cannot \
             replace it",
            hex::encode(state.state_id),
            state.holders,
            finality.group
        ))),
        FinalityStatus::Forked { states, .. } => Some(Error::PointerForked(format!(
            "pointer {address} holds {} different final states",
            states.len()
        ))),
    }
}

/// What a transfer whose write reported failure comes to, given the finality
/// check made after it.
///
/// A rival final state is the reason, and is named. The transfer's own state
/// held by a majority means the write landed although its acknowledgements
/// were lost: the transfer is stored and final, so it is reported as done, not
/// failed, and nobody pays for it twice. Anything else is the write's own
/// error.
fn after_failed_transfer(
    put: Error,
    finality: Result<PointerFinality>,
    record: &Pointer,
) -> Result<PointerTransfer> {
    let Ok(finality) = finality else {
        return Err(put);
    };
    if let Some(refusal) = rival_final(&finality, record) {
        return Err(refusal);
    }
    let ours = record.state_id();
    match &finality.status {
        FinalityStatus::Final(state) | FinalityStatus::Unconfirmed(state)
            if state.state_id == ours =>
        {
            Ok(PointerTransfer {
                address: record.address(),
                state_id: ours,
                finality: Ok(finality),
            })
        }
        _ => Err(put),
    }
}

/// What a finality check makes of a tally over a group of `group` peers, of
/// which `answered` answered.
fn finality_status(replies: &Replies, group: usize, answered: usize) -> FinalityStatus {
    let majority = read_quorum(group);
    let mut finals: Vec<FinalState> = replies
        .finals()
        .map(|(record, count)| FinalState::of(record, *count))
        .collect();
    finals.sort_by_key(|state| Reverse(state.holders));
    let held_by_majority = finals
        .iter()
        .find(|state| state.holders >= majority)
        .cloned();
    match finals.as_slice() {
        [] => FinalityStatus::Open {
            counter: replies
                .corroborated(corroboration(group))
                .map(Pointer::counter),
        },
        [only] if only.holders >= majority && answered >= group => {
            FinalityStatus::Final(only.clone())
        }
        [only] if only.holders >= majority => FinalityStatus::Unconfirmed(only.clone()),
        [only] => FinalityStatus::Settling(only.clone()),
        _ => FinalityStatus::Forked {
            states: finals,
            majority: held_by_majority,
        },
    }
}

/// What one peer's acknowledgement of a pointer PUT says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PutAck {
    /// It holds the state that was sent.
    Stored,
    /// It holds a state it says beats the one sent. That is one peer's word,
    /// not a verified record, so a write stops for it only once a read
    /// confirms the pointer has moved on.
    Stale,
}

/// Whether one round of a pointer write stored it, from what came back.
///
/// `stored` peers hold the record, `wanted` is the write quorum, and `moved`
/// says a read confirmed the network holds a state that replaces it.
///
/// A refusal ends the write only when no peer stored the record and a second
/// peer refused as well. Once any peer has taken the same proof, one peer
/// refusing it is that peer's problem; and while the rest of the group is
/// silent, one refusal is that peer's word alone. Either way the round is a
/// shortfall to retry with the proof already paid for, rather than a final
/// answer one peer gave that throws the payment away.
fn judge_write(
    address: &XorName,
    stored: usize,
    wanted: usize,
    moved: bool,
    answered: Answered,
) -> Result<()> {
    if stored >= wanted {
        return Ok(());
    }
    if moved {
        return Err(Error::InvalidData(format!(
            "the pointer {} moved while this update was in flight; the network now \
             holds a newer state",
            hex::encode(address)
        )));
    }
    if stored == 0 && answered.refusals >= corroboration(wanted) {
        if let Some(refusal) = answered.refusal {
            return Err(refusal);
        }
    }
    if answered.count > 0 || answered.refusals > 0 {
        return Err(Error::CloseGroupShortfall(format!(
            "pointer {} stored on {stored} of {wanted} close-group peers",
            hex::encode(address)
        )));
    }
    Err(answered
        .last_error
        .unwrap_or_else(|| Error::Protocol("no close-group peer accepted the pointer".to_string())))
}

/// Whether a pointer write that failed this way may succeed if tried again
/// with the same proof: a shortfall, an unreachable group, or a node that
/// could not store it just then, not a refusal of the record itself.
///
/// A node answering with an error reports its own condition, such as a full
/// disk or too many checks running, not a verdict on a record this client
/// signed and paid for, so other peers may still take the same proof.
fn worth_retrying(error: &Error) -> bool {
    matches!(
        error,
        Error::CloseGroupShortfall(_)
            | Error::Network(_)
            | Error::Timeout(_)
            | Error::InsufficientPeers(_)
            | Error::RemotePut { .. }
    )
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
) -> Option<Result<PutAck>> {
    let ChunkMessageBody::PointerPutResponse(response) = body else {
        return None;
    };
    Some(match response {
        // `Unchanged` is success for a retry: the state the client signed is
        // exactly what the node holds.
        PointerPutResponse::Success { address, state_id }
        | PointerPutResponse::Unchanged { address, state_id } => {
            if address == expected_address && state_id == expected_state {
                Ok(PutAck::Stored)
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
        // A claim, about this pointer, that it has moved on. Whether it has
        // is for a read to confirm (see [`judge_write`]).
        PointerPutResponse::Stale { address, .. } if address == expected_address => {
            Ok(PutAck::Stale)
        }
        PointerPutResponse::Stale { address, .. } => Err(Error::InvalidData(format!(
            "peer refused a pointer this client did not send: address {}",
            hex::encode(address)
        ))),
        PointerPutResponse::PaymentRequired { message } => Err(Error::Payment(message)),
        // The node's own reason, kept as it gave it: a node that could not
        // store the record is a shortfall to retry, not a refusal of it.
        PointerPutResponse::Error(e) => Err(Error::RemotePut {
            address: hex::encode(expected_address),
            source: e,
        }),
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

/// Refuse a recipient that does not exist or whose chain leads back to
/// `address`.
///
/// A transfer to a pointer nobody created redirects every reader into a
/// broken chain, and one that leads back here is a cycle; either is final
/// the moment it is stored. The chain is followed as it stands now: the
/// recipient can still move its own pointer later, and a read that meets
/// a cycle then refuses it as it would any other.
///
/// A reader of `address` reads it first and then every pointer from the
/// recipient on, so the recipient's chain gets one hop fewer than
/// [`MAX_POINTER_RESOLVE_DEPTH`]. A chain still going after that is
/// refused: readers could not resolve it, and whether it leads back here
/// is past what can be checked.
async fn check_recipient_chain<Get, Fut>(
    address: &XorName,
    recipient: &XorName,
    get: Get,
) -> Result<()>
where
    Get: Fn(XorName) -> Fut,
    Fut: Future<Output = Result<Option<Pointer>>>,
{
    let mut at = *recipient;
    let mut seen = HashSet::new();
    for hop in 0..MAX_POINTER_RESOLVE_DEPTH.saturating_sub(1) {
        if at == *address {
            return Err(Error::InvalidData(format!(
                "recipient pointer {} leads back to pointer {}; the transfer would be a \
                 cycle",
                hex::encode(recipient),
                hex::encode(address)
            )));
        }
        if !seen.insert(at) {
            return Ok(());
        }
        let Some(record) = get(at).await? else {
            if hop == 0 {
                return Err(Error::InvalidData(format!(
                    "recipient pointer {} does not exist; its owner must create it before \
                     it can receive a transfer",
                    hex::encode(recipient)
                )));
            }
            return Ok(());
        };
        let target = record.target();
        match target.kind() {
            Some(PointerTargetKind::Pointer) => at = target.address,
            _ => return Ok(()),
        }
    }
    if at == *address {
        return Err(Error::InvalidData(format!(
            "recipient pointer {} leads back to pointer {}; the transfer would be a cycle",
            hex::encode(recipient),
            hex::encode(address)
        )));
    }
    Err(Error::InvalidData(format!(
        "recipient pointer {} starts a chain longer than {} hops; readers of pointer {} \
         could not follow the transfer",
        hex::encode(recipient),
        MAX_POINTER_RESOLVE_DEPTH.saturating_sub(1),
        hex::encode(address)
    )))
}

impl Client {
    /// Create a pointer at counter 0, owned by `owner`.
    ///
    /// # Errors
    ///
    /// Returns an error if signing fails, payment fails, or fewer peers than
    /// the write quorum store the record.
    pub async fn pointer_create(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<XorName> {
        let record = self.pointer_sign_create(secret_key, owner, target).await?;
        self.pointer_put(&record).await
    }

    /// Sign the record that creates `owner`'s pointer, after checking the
    /// network holds none.
    ///
    /// Checked before anything is paid: a second creation would pay again for
    /// a state the network either already holds, and answers as unchanged, or
    /// has moved past, and refuses as stale. Either way the payment buys
    /// nothing.
    ///
    /// # Errors
    ///
    /// Returns an error if the pointer already exists, if the network cannot
    /// say whether it does, or if signing fails.
    pub async fn pointer_sign_create(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<Pointer> {
        let address = pointer_address(owner);
        if let Some(existing) = self.pointer_get(&address).await? {
            return Err(Error::InvalidData(format!(
                "pointer {} already exists at counter {}; update it instead",
                hex::encode(address),
                existing.counter()
            )));
        }
        Pointer::create(secret_key, owner, target)
            .map_err(|e| Error::InvalidData(format!("cannot sign pointer: {e}")))
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
    /// exhausted, signing fails, payment fails, or fewer peers than the write
    /// quorum store it.
    pub async fn pointer_update(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<XorName> {
        let record = self.pointer_sign_update(secret_key, owner, target).await?;
        self.pointer_put(&record).await
    }

    /// Sign the record that updates `owner`'s pointer to `target`, without
    /// storing it: one past the counter the network serves, or a creation at 0
    /// if it serves none.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PointerFinal`] or [`Error::PointerForked`] if any peer
    /// of the close group holds a final state for the pointer, even one too
    /// few peers hold for a read to return; otherwise an error if the group
    /// cannot be asked, the current record cannot be read, or signing fails.
    pub async fn pointer_sign_update(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        target: PointerTarget,
    ) -> Result<Pointer> {
        let address = pointer_address(owner);
        // A final state even one peer holds was signed by the owner, and no
        // node holding it will take this update: a transfer or freeze that
        // fell short. A read settles once enough peers agree, before a slow
        // peer holding it may have answered, so the whole group is asked:
        // finish the final state rather than move the rest of the group
        // somewhere those nodes never follow. An update can be replaced, so a
        // peer that does not answer does not hold it up.
        let finality = self.pointer_finality(&address).await?;
        if let Some(refusal) = final_already(&finality) {
            return Err(refusal);
        }
        match self.pointer_get(&address).await? {
            Some(current) => current.update(secret_key, target).map_err(|e| match e {
                PointerError::CounterExhausted => Error::PointerFinal(format!(
                    "pointer {} holds {}; nothing can update it",
                    hex::encode(address),
                    describe_final(&current)
                )),
                e => Error::InvalidData(format!("cannot sign pointer update: {e}")),
            }),
            None => Pointer::create(secret_key, owner, target)
                .map_err(|e| Error::InvalidData(format!("cannot sign pointer: {e}"))),
        }
    }

    /// Hand the pointer owned by `owner` over to the pointer at `recipient`,
    /// for good, and report where the transfer stands.
    ///
    /// Signs the pointer's final state, pointing at `recipient`, pays for it
    /// and stores it (ADR-0018 in `ant-node`). A final state is replaced by
    /// nothing, so from then on every reader of this address is redirected to
    /// the recipient's pointer, which only its owner can move, and this key
    /// can change nothing. The address readers use stays the same.
    ///
    /// The recipient should create a pointer with a fresh key for each address
    /// it receives: a pointer's address derives from its key, so every address
    /// handed to one recipient pointer resolves to the same place.
    ///
    /// Once the write has landed the whole close group is asked where the
    /// transfer stands. That check is reported beside the transfer, not as its
    /// outcome: a check that fails after the write says nothing about a
    /// transfer that is already stored and final. A recipient should check it
    /// with [`Self::pointer_finality`] itself before relying on the transfer.
    ///
    /// # Errors
    ///
    /// As [`Self::pointer_sign_transfer`] before anything is paid; then as
    /// [`Self::pointer_put`], except that a write refused because the pointer
    /// was already final elsewhere is reported as [`Error::PointerFinal`] or
    /// [`Error::PointerForked`].
    pub async fn pointer_transfer(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        recipient: XorName,
    ) -> Result<PointerTransfer> {
        let record = self
            .pointer_sign_transfer(secret_key, owner, recipient)
            .await?;
        let address = record.address();
        if let Err(e) = self.pointer_put(&record).await {
            // Nodes refuse a final state when they hold, or their group
            // proves, a different one; and a write whose acknowledgements
            // were lost may have landed all the same. The group says which.
            let finality = self.pointer_finality(&address).await;
            return after_failed_transfer(e, finality, &record);
        }
        Ok(PointerTransfer {
            address,
            state_id: record.state_id(),
            finality: self.pointer_finality(&address).await,
        })
    }

    /// Sign the final state that hands `owner`'s pointer over to the pointer
    /// at `recipient`, without storing it.
    ///
    /// Everything that can be checked is checked before it is stored, because
    /// once a final state is stored it cannot be taken back: the pointer must
    /// not be final already, the recipient must not be the pointer itself, the
    /// recipient pointer must exist, and following it must not lead back
    /// here.
    ///
    /// "Final already" is asked of the whole close group
    /// ([`Self::pointer_finality`]), not of an ordinary read. A read returns a
    /// state only once two peers name it, so a final state an earlier
    /// transfer left on one peer is not what it returns, and signing a second
    /// final state past that one would fork the pointer for good. Any final
    /// state other than the one signed here refuses the transfer; the same
    /// state, left by an earlier attempt at this very transfer, does not. A
    /// silent peer could hold one, so a transfer goes ahead only when every
    /// peer of the configured close group answered.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PointerFinal`] if the pointer is already final, or
    /// [`Error::PointerForked`] if its owner already forked it,
    /// [`Error::InvalidData`] for a recipient that is this pointer, does not
    /// exist or leads back to it, [`Error::CloseGroupShortfall`] if not every
    /// peer of the group answered, or an error if either pointer cannot be
    /// read or signing fails.
    pub async fn pointer_sign_transfer(
        &self,
        secret_key: &MlDsaSecretKey,
        owner: &MlDsaPublicKey,
        recipient: XorName,
    ) -> Result<Pointer> {
        let address = pointer_address(owner);
        if recipient == address {
            return Err(Error::InvalidData(format!(
                "pointer {} cannot be handed over to itself",
                hex::encode(address)
            )));
        }
        let current = self.pointer_get(&address).await?;
        if let Some(current) = current.as_ref().filter(|record| record.is_terminal()) {
            return Err(Error::PointerFinal(format!(
                "pointer {} already holds {}",
                hex::encode(address),
                describe_final(current)
            )));
        }
        self.check_recipient(&address, &recipient).await?;

        let target = PointerTarget::new(PointerTargetKind::Pointer, recipient);
        let record = match current {
            Some(current) => current.transfer_to(secret_key, recipient),
            None => Pointer::sign(secret_key, owner, FINAL_COUNTER, target),
        }
        .map_err(|e| Error::InvalidData(format!("cannot sign pointer transfer: {e}")))?;

        let finality = self.pointer_finality(&address).await?;
        if let Some(refusal) = rival_final(&finality, &record) {
            return Err(refusal);
        }
        // A transfer cannot be taken back, so no final state being seen is
        // only enough when every peer of the group was asked and answered.
        if finality.answered < finality.group {
            return Err(Error::CloseGroupShortfall(format!(
                "only {} of the {} peers of pointer {}'s close group answered, and a silent \
                 one could hold a final state; a transfer cannot be taken back, so try again \
                 when all of them can be asked",
                finality.answered,
                finality.group,
                hex::encode(address)
            )));
        }
        Ok(record)
    }

    /// Refuse a recipient that does not exist or whose chain leads back to
    /// `address` (see [`check_recipient_chain`]).
    async fn check_recipient(&self, address: &XorName, recipient: &XorName) -> Result<()> {
        check_recipient_chain(address, recipient, |at| async move {
            self.pointer_get(&at).await
        })
        .await
    }

    /// Ask the whole close group where `address` stands with respect to
    /// finality.
    ///
    /// Unlike [`Self::pointer_get`], this does not stop once it has an answer:
    /// every peer is asked, within its own deadline, so a second final state
    /// held by even one of them is seen. That is what a recipient needs before
    /// relying on a transfer — [`FinalityStatus::Final`] means one final state,
    /// held by a majority, with every peer of the configured close group
    /// answering and none holding a rival. A group that did not all answer
    /// can show at most [`FinalityStatus::Unconfirmed`].
    ///
    /// # Errors
    ///
    /// Returns an error if no peer is responsible for the address, or if too
    /// few of the group answer to speak for it.
    pub async fn pointer_finality(&self, address: &XorName) -> Result<PointerFinality> {
        let peers = self.pointer_group(address).await?;
        let in_flight = FuturesUnordered::new();
        for (peer_id, addrs) in &peers {
            in_flight.push(self.send_pointer_get(*address, *peer_id, addrs.clone()));
        }
        let mut replies = Replies::default();
        let answered = ask_the_group(in_flight, peers.len(), |found| {
            replies.add(found);
            false
        })
        .await;

        // Counted over the configured group, as every pointer quorum is: a
        // lookup that came back short is peers that did not answer, not a
        // smaller group.
        let width = quorum_width(peers.len(), self.config().close_group_size);
        let wanted = read_quorum(width);
        if answered.count < wanted {
            let count = answered.count;
            return Err(answered
                .failure()
                .filter(|_| count == 0)
                .unwrap_or_else(|| {
                    Error::CloseGroupShortfall(format!(
                        "only {count} of the close group answered for pointer {}",
                        hex::encode(address)
                    ))
                }));
        }
        Ok(PointerFinality {
            address: *address,
            group: width,
            answered: answered.count,
            status: finality_status(&replies, width, answered.count),
        })
    }

    /// Follow `address` through every transfer to the pointer whose owner now
    /// decides what it resolves to.
    ///
    /// Only transfers are followed — final states targeting a pointer. A
    /// pointer target below the final counter is a forwarding its owner can
    /// still take back, so that owner still decides, and the walk stops there.
    ///
    /// # Errors
    ///
    /// Returns an error if a pointer on the way is missing or unreadable, if
    /// the transfers cycle, or if they are longer than
    /// [`MAX_POINTER_RESOLVE_DEPTH`].
    pub async fn pointer_controller(&self, address: &XorName) -> Result<PointerController> {
        let mut seen = HashSet::new();
        let mut transfers = Vec::new();
        let mut at = *address;
        for _ in 0..MAX_POINTER_RESOLVE_DEPTH {
            if !seen.insert(at) {
                return Err(Error::InvalidData(format!(
                    "pointer transfers cycle back to {}",
                    hex::encode(at)
                )));
            }
            let record = self
                .pointer_get(&at)
                .await?
                .ok_or_else(|| Error::NotFound(format!("no pointer at {}", hex::encode(at))))?;
            match record.transferred_to() {
                Some(next) => {
                    transfers.push(next);
                    at = next;
                }
                None => {
                    return Ok(PointerController {
                        pointer: at,
                        transfers,
                        frozen: record.is_terminal(),
                    })
                }
            }
        }
        Err(Error::InvalidData(format!(
            "pointer transfers from {} are longer than {MAX_POINTER_RESOLVE_DEPTH} hops",
            hex::encode(address)
        )))
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
    /// fails, or if fewer peers than the write quorum store the record.
    pub async fn pointer_put(&self, record: &Pointer) -> Result<XorName> {
        let address = record.address();
        let state_id = record.state_id();

        // Refuse before paying if nobody is responsible for this address, or
        // too few of those who are can take the write. There is no sense
        // buying storage with nowhere to put it, and a quorum of nothing would
        // otherwise be satisfied by nothing — a paid write reported as stored
        // on zero peers.
        self.pointer_write_group(&address).await?;

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
        self.pointer_put_paid(record, proof).await
    }

    /// Quote a signed pointer's state for a payer outside this client, such as
    /// a browser wallet or another external signer, without paying.
    ///
    /// Pay the plan's median quote, build the proof with
    /// [`ChunkPaymentPlan::proof`], then store with [`Self::pointer_put_paid`].
    ///
    /// # Errors
    ///
    /// Returns an error if no peer is responsible for the address, or if too
    /// few acceptable quotes come back.
    pub async fn prepare_pointer_payment(&self, record: &Pointer) -> Result<ChunkPaymentPlan> {
        let address = record.address();
        // As for a wallet payment: nothing is quoted with nowhere to store it.
        self.pointer_write_group(&address).await?;
        self.prepare_payment_plan_split(
            &address,
            &record.state_id(),
            POINTER_WIRE_LEN as u64,
            DATA_TYPE_POINTER,
        )
        .await
    }

    /// Store a signed pointer with a proof already paid for its state.
    ///
    /// The second half of [`Self::pointer_put`], for a proof paid outside this
    /// client. It is also how to retry a write that fell short without paying
    /// again: the proof stays valid for its state, and a node that already
    /// holds that state answers as stored.
    ///
    /// # Errors
    ///
    /// Returns an error if no peer is responsible for the address or if too few
    /// of the close group accept the record.
    pub async fn pointer_put_paid(&self, record: &Pointer, proof: Vec<u8>) -> Result<XorName> {
        // A write that fell short is retried with the same proof, as a chunk
        // store is: peers that took it the first time answer `Unchanged`, which
        // counts, so each round only has to reach the ones that did not. A
        // refusal that says something definite, such as the payment was
        // refused or a peer acknowledged something else, is final only once a
        // second peer repeats it and nobody stored the record; a newer state
        // winning is final once a read confirms it (see [`judge_write`]). One
        // peer's refusal alone is retried like any shortfall.
        let mut attempt = 0;
        loop {
            match self.pointer_put_once(record, &proof).await {
                Ok(address) => return Ok(address),
                Err(e) if attempt < STORE_MAX_RETRIES && worth_retrying(&e) => {
                    attempt += 1;
                    warn!(
                        "pointer {} write fell short ({e}); retry {attempt}/{STORE_MAX_RETRIES}",
                        hex::encode(record.address())
                    );
                    sleep(store_retry_delay(attempt)).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// One round of [`Self::pointer_put_paid`].
    async fn pointer_put_once(&self, record: &Pointer, proof: &[u8]) -> Result<XorName> {
        let address = record.address();
        let state_id = record.state_id();

        // Ask again rather than keep the set from before the payment. Settling
        // on chain takes time, and the peers responsible for an address can
        // change while it does; writing to the old set would then be refused by
        // peers that are no longer responsible while the ones that are were
        // never asked. The lookup above answered whether there was anywhere to
        // store this — this one answers where.
        let targets = self.pointer_group(&address).await?;
        let wanted = write_quorum(quorum_width(targets.len(), self.config().close_group_size));
        if targets.len() < wanted {
            return Err(Error::CloseGroupShortfall(format!(
                "found {} close-group peers for pointer {}, and a write needs {wanted}",
                targets.len(),
                hex::encode(address)
            )));
        }

        let request =
            PointerPutRequest::with_payment(Bytes::from(record.to_bytes()), proof.to_vec());
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

        let (mut stored, mut stale) = (0usize, 0usize);
        let answered = ask_the_group(in_flight, wanted, |ack| {
            match ack {
                PutAck::Stored => stored += 1,
                PutAck::Stale => stale += 1,
            }
            stored >= wanted
        })
        .await;
        // Only a peer's word says the pointer moved on, so a read decides it
        // before the write is abandoned.
        let moved = stored < wanted && stale > 0 && self.moved_past(record).await;
        judge_write(&address, stored, wanted, moved, answered).map(|()| address)
    }

    /// Whether the network has moved past `record`: a read returns a state
    /// that replaces it, or, for a final state, a different final state,
    /// which nothing replaces either and which the group settled on instead.
    /// A read that fails says nothing, and counts as no.
    async fn moved_past(&self, record: &Pointer) -> bool {
        matches!(
            self.pointer_get(&record.address()).await,
            Ok(Some(current)) if current.replaces(record)
                || (record.is_terminal()
                    && current.is_terminal()
                    && current.state_id() != record.state_id())
        )
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
    /// membership that changed in between is not covered here. That is churn,
    /// and what covers it is the nodes' replication, which hands a paid state to
    /// every member of the group that should hold it.
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

    /// The close group a write of `address` goes to, refused before anything
    /// is quoted or paid if too few of it would take the write.
    ///
    /// While nodes are being upgraded some of a close group may not store
    /// pointers yet. A browser can tell which from what each node advertises,
    /// and a write that fewer than a write quorum would take is otherwise paid
    /// for and then falls short. Only a node that says it would refuse counts
    /// against the write: one that cannot be asked in time might still take
    /// it, and the write finds out, as it did before. A native client cannot
    /// tell at all, and its write finds out as before.
    async fn pointer_write_group(
        &self,
        address: &XorName,
    ) -> Result<Vec<(PeerId, Vec<MultiAddr>)>> {
        let group = self.pointer_group(address).await?;
        let wanted = write_quorum(quorum_width(group.len(), self.config().close_group_size));
        // Too few found to reach a write quorum is a shortfall, whatever they
        // would say: no need to ask them.
        if group.len() < wanted {
            return Err(Error::CloseGroupShortfall(format!(
                "found {} close-group peers for pointer {}, and a write needs {wanted}; \
                 nothing was paid",
                group.len(),
                hex::encode(address)
            )));
        }
        let mut asked: FuturesUnordered<_> = group
            .iter()
            .map(|(peer, addrs)| self.network().accepts_pointer_writes(peer, addrs))
            .collect();
        let (mut taking, mut refusing) = (0usize, 0usize);
        while let Some(answer) = asked.next().await {
            match answer {
                Some(true) => taking += 1,
                Some(false) => refusing += 1,
                None => {}
            }
            if taking >= wanted {
                break;
            }
            if group.len().saturating_sub(refusing) < wanted {
                return Err(Error::InsufficientPeers(format!(
                    "{refusing} of the {} close-group peers for pointer {} do not accept \
                     pointer writes, so a write cannot reach the {wanted} it needs; nothing \
                     was paid",
                    group.len(),
                    hex::encode(address)
                )));
            }
        }
        // Enough would take it, or too few could be asked to be sure it
        // cannot land. Probes still out are dropped, which cancels them.
        drop(asked);
        Ok(group)
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
        // peer cannot stall a read whose answer is settled. One that is not,
        // because a newer state has been named but not yet confirmed, waits
        // for the rest of the group, each request within its own deadline:
        // that is what an update in flight costs a reader, and all a peer
        // naming a state nobody else holds can cost it.
        let in_flight = FuturesUnordered::new();
        for (peer_id, addrs) in &peers {
            in_flight.push(self.send_pointer_get(*address, *peer_id, addrs.clone()));
        }

        let width = quorum_width(peers.len(), self.config().close_group_size);
        let wanted = read_quorum(width);
        let needed = corroboration(width);
        let (answered, replies) = collect_read(in_flight, width).await;

        if answered.count == 0 {
            return Err(answered.failure().unwrap_or_else(|| {
                Error::Protocol("no close-group peer answered for the pointer".to_string())
            }));
        }
        // A single answer is not a network verdict: one reachable peer holding
        // a stale record, before replication has reached it, looks exactly like
        // the whole group agreeing. Say so rather than presenting it as the
        // value.
        if answered.count < wanted {
            return Err(Error::CloseGroupShortfall(format!(
                "only {} of the close group answered for pointer {}",
                answered.count,
                hex::encode(address)
            )));
        }
        // The owner signed two final states and no majority holds either:
        // there is nothing to return that the rest of the network would agree
        // with, and asking again sees the same split.
        let corroborated = match replies.chosen(needed, wanted) {
            Chosen::State(state) => state.cloned(),
            Chosen::Forked => {
                return Err(Error::PointerForked(format!(
                    "pointer {} holds {} different final states and none is held by {wanted} \
                     of the {width} peers in its close group",
                    hex::encode(address),
                    replies.finals().count()
                )));
            }
        };
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
    ) -> Result<PutAck> {
        let request_id = self.next_request_id();
        let message = ChunkMessage {
            request_id,
            body: ChunkMessageBody::PointerPutRequest(request),
        };
        let bytes = message
            .encode()
            .map_err(|e| Error::Protocol(format!("cannot encode pointer PUT: {e}")))?;

        send_and_await_chunk_response(
            self.network(),
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
            self.network(),
            &target_peer,
            bytes,
            request_id,
            Duration::from_secs(self.config().chunk_get_timeout_secs),
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
    use ant_protocol::pqc::api::ml_dsa_65;
    use futures::future::BoxFuture;
    use std::collections::HashMap;

    fn keypair(seed: u8) -> (MlDsaPublicKey, MlDsaSecretKey) {
        ml_dsa_65().generate_keypair_from_seed(&[seed; 32])
    }

    /// A write that fell short is retried with the proof already paid; one
    /// that was refused for a reason is not, since retrying cannot change it.
    #[test]
    fn only_a_shortfall_is_retried_with_the_same_proof() {
        for retried in [
            Error::CloseGroupShortfall("3 of 5".into()),
            Error::Network("unreachable".into()),
            Error::Timeout("slow".into()),
            Error::InsufficientPeers("none".into()),
        ] {
            assert!(worth_retrying(&retried), "{retried} must be retried");
        }
        for refused in [
            Error::InvalidData("the pointer moved while this update was in flight".into()),
            Error::Payment("valid payment is required".into()),
            Error::Protocol("pointer PUT refused".into()),
        ] {
            assert!(!worth_retrying(&refused), "{refused} must not be retried");
        }
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
            assert!(matches!(judged(honest), Ok(PutAck::Stored)));
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
            assert!(
                !matches!(judged(lie), Ok(PutAck::Stored)),
                "this must not count as stored"
            );
        }
    }

    /// A lookup that comes back short does not shrink the quorums: they are
    /// counted over the configured group, so one returned peer can neither
    /// complete a write nor decide a read.
    #[test]
    fn a_short_lookup_is_a_shortfall_not_a_smaller_quorum() {
        let width = quorum_width(1, 7);
        assert_eq!(write_quorum(width), 5, "a write still needs five copies");
        assert_eq!(read_quorum(width), 4, "a read still needs four answers");
        assert_eq!(corroboration(width), 2, "and two peers naming its state");
        assert_eq!(quorum_width(9, 7), 9, "a wider lookup is counted as it is");
    }

    fn answered(count: usize, refusal: Option<Error>) -> Answered {
        let refusals = usize::from(refusal.is_some());
        Answered {
            count,
            last_error: None,
            refusal,
            refusals,
        }
    }

    /// One peer's word cannot throw a paid write away. Four peers stored it and
    /// one claims the pointer moved on: unless a read confirms that, the round
    /// is a shortfall, retried with the proof already paid for.
    #[test]
    fn one_peer_cannot_end_a_paid_write_that_others_stored() {
        let address = [7u8; 32];
        let refused = || Some(Error::Payment("pay up".to_string()));

        let unconfirmed = judge_write(&address, 4, 5, false, answered(5, None));
        assert!(
            matches!(&unconfirmed, Err(e) if worth_retrying(e)),
            "a claim nobody confirmed is a shortfall, got {unconfirmed:?}"
        );
        let refused_after_stores = judge_write(&address, 2, 5, false, answered(3, refused()));
        assert!(
            matches!(&refused_after_stores, Err(e) if worth_retrying(e)),
            "a refusal after others took the proof is retried, got {refused_after_stores:?}"
        );

        let confirmed = judge_write(&address, 4, 5, true, answered(5, None));
        assert!(
            matches!(&confirmed, Err(e) if !worth_retrying(e)),
            "a move a read confirmed is final, got {confirmed:?}"
        );
        assert!(
            matches!(
                judge_write(
                    &address,
                    0,
                    5,
                    false,
                    Answered {
                        refusals: 2,
                        ..answered(0, refused())
                    }
                ),
                Err(Error::Payment(_))
            ),
            "a refusal a second peer repeats, with nobody storing, is the answer"
        );
        assert!(judge_write(&address, 5, 5, false, answered(5, None)).is_ok());
    }

    /// Nor can one peer end it while the rest of the group is silent. Six
    /// peers time out and one refuses: that refusal is one peer's word, so
    /// the round is retried with the proof already paid for.
    #[test]
    fn one_refusal_among_silent_peers_is_retried() {
        let address = [7u8; 32];
        let alone = judge_write(
            &address,
            0,
            5,
            false,
            Answered {
                last_error: Some(Error::Timeout("no answer".to_string())),
                ..answered(0, Some(Error::Payment("pay up".to_string())))
            },
        );
        assert!(
            matches!(&alone, Err(e) if worth_retrying(e)),
            "one refusal among silent peers is a shortfall, got {alone:?}"
        );
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

    /// Replies that arrive in the order given, one every ten milliseconds.
    fn arriving(
        replies: Vec<Result<Option<Pointer>>>,
    ) -> FuturesUnordered<BoxFuture<'static, Result<Option<Pointer>>>> {
        let in_flight: FuturesUnordered<BoxFuture<'static, Result<Option<Pointer>>>> =
            FuturesUnordered::new();
        for (at, reply) in (1u64..).zip(replies) {
            in_flight.push(Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10 * at)).await;
                reply
            }));
        }
        in_flight
    }

    /// A write that reached five of seven peers must read back, even when one
    /// peer replays the older record it replaced.
    ///
    /// The two peers the write missed still hold the older state, honestly,
    /// and the replaying peer makes three. If those three answer first, the
    /// older state is corroborated after four answers while the newer one has
    /// been named once. Stopping there hands back the state the write replaced
    /// while four peers hold the new one, so the read keeps asking until the
    /// newer state is corroborated or the group is exhausted. A newer state
    /// nobody corroborates still never wins.
    #[tokio::test(start_paused = true)]
    async fn a_replayed_older_state_cannot_end_a_read_before_the_newer_one_is_heard() {
        let (pk, sk) = keypair(13);
        let old = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let new = old.update(&sk, chunk_target(2)).expect("update");
        let old_reply = || Ok(Some(old.clone()));
        let new_reply = || Ok(Some(new.clone()));

        let (answered, replies) = collect_read(
            arriving(vec![
                old_reply(),
                old_reply(),
                old_reply(),
                new_reply(),
                new_reply(),
                new_reply(),
                new_reply(),
            ]),
            7,
        )
        .await;
        assert_eq!(
            replies
                .corroborated(corroboration(7))
                .expect("an answer")
                .state_id(),
            new.state_id(),
            "the state the write stored is the answer ({} answered)",
            answered.count
        );

        // When nobody else confirms the newer state, it is still one peer's
        // word, and the group's answer stands, only later.
        let (answered, replies) = collect_read(
            arriving(vec![
                old_reply(),
                old_reply(),
                old_reply(),
                new_reply(),
                Err(Error::Timeout("gone".to_string())),
                Err(Error::Timeout("gone".to_string())),
                Err(Error::Timeout("gone".to_string())),
            ]),
            7,
        )
        .await;
        assert_eq!(answered.count, 4);
        assert_eq!(
            replies
                .corroborated(corroboration(7))
                .expect("an answer")
                .state_id(),
            old.state_id(),
            "a newer state one peer names never wins"
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
            answered.failure().is_some(),
            "the failure must be kept to explain the shortfall"
        );
    }

    /// A refusal is kept apart from a missing answer, so a write that fell
    /// short because peers said no can report that rather than a timeout.
    #[tokio::test]
    async fn a_refusal_is_kept_apart_from_a_missing_answer() {
        let in_flight: FuturesUnordered<BoxFuture<'static, Result<()>>> = FuturesUnordered::new();
        in_flight.push(Box::pin(async { Err(Error::Timeout("slow".to_string())) }));
        in_flight.push(Box::pin(async {
            Err(Error::InvalidData(
                "the pointer moved while this update was in flight".to_string(),
            ))
        }));
        in_flight.push(Box::pin(async { Err(Error::Timeout("slow".to_string())) }));
        in_flight.push(Box::pin(async { Ok(()) }));

        let answered = ask_the_group(in_flight, write_quorum(7), |()| true).await;
        assert_eq!(answered.count, 1);
        assert_eq!(answered.refusals, 1);
        assert!(
            matches!(&answered.refusal, Some(Error::InvalidData(message)) if message.contains("moved")),
            "the refusal is what the write reports"
        );
        assert!(matches!(answered.last_error, Some(Error::Timeout(_))));
    }

    /// Refusals are counted as they arrive through the group, not only the
    /// first kept. One refusal among six silent peers is retried with the
    /// proof already paid for; a second makes it the answer.
    #[tokio::test]
    async fn refusals_are_counted_through_the_group_and_judged_by_how_many() {
        let group = |refusals: usize| {
            let in_flight: FuturesUnordered<BoxFuture<'static, Result<PutAck>>> =
                FuturesUnordered::new();
            for peer in 0..7 {
                in_flight.push(Box::pin(async move {
                    if peer < refusals {
                        Err(Error::Payment("pay up".to_string()))
                    } else {
                        Err(Error::Timeout("slow".to_string()))
                    }
                }));
            }
            in_flight
        };
        let address = [7u8; 32];
        let wanted = write_quorum(7);

        let one = ask_the_group(group(1), wanted, |_| true).await;
        assert_eq!(one.refusals, 1);
        let judged = judge_write(&address, 0, wanted, false, one);
        assert!(
            matches!(&judged, Err(e) if worth_retrying(e)),
            "one refusal among silent peers is retried, got {judged:?}"
        );

        let two = ask_the_group(group(2), wanted, |_| true).await;
        assert_eq!(two.refusals, 2);
        let judged = judge_write(&address, 0, wanted, false, two);
        assert!(
            matches!(&judged, Err(Error::Payment(_))),
            "two refusals with nobody storing are the answer, got {judged:?}"
        );
    }

    /// A node that cannot store a record just then, such as one whose disk is
    /// full, says nothing about the record. Two of them among five silent
    /// peers leave a round to retry with the same proof.
    #[tokio::test]
    async fn a_node_that_cannot_store_is_a_shortfall_not_a_refusal() {
        let in_flight: FuturesUnordered<BoxFuture<'static, Result<PutAck>>> =
            FuturesUnordered::new();
        for peer in 0..7 {
            in_flight.push(Box::pin(async move {
                if peer < 2 {
                    Err(Error::RemotePut {
                        address: String::new(),
                        source: ant_protocol::ProtocolError::StorageFailed("disk full".to_string()),
                    })
                } else {
                    Err(Error::Timeout("slow".to_string()))
                }
            }));
        }
        let wanted = write_quorum(7);
        let answered = ask_the_group(in_flight, wanted, |_| true).await;
        assert_eq!(answered.refusals, 0, "a full disk is not a refusal");
        let judged = judge_write(&[7u8; 32], 0, wanted, false, answered);
        assert!(
            matches!(&judged, Err(e) if worth_retrying(e)),
            "got {judged:?}"
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

    /// The final state handing `sk`'s pointer over to `recipient`.
    fn handed_to(pk: &MlDsaPublicKey, sk: &MlDsaSecretKey, recipient: u8) -> Pointer {
        Pointer::sign(
            sk,
            pk,
            FINAL_COUNTER,
            PointerTarget::new(PointerTargetKind::Pointer, [recipient; 32]),
        )
        .expect("sign")
    }

    fn state_of(chosen: Chosen<'_>) -> Option<XorName> {
        match chosen {
            Chosen::State(state) => state.map(Pointer::state_id),
            Chosen::Forked => panic!("expected a state, got a fork"),
        }
    }

    /// A transfer every peer holds is the answer, and the read settles on a
    /// majority of them.
    #[test]
    fn a_transfer_the_group_holds_is_read_back() {
        let (pk, sk) = keypair(40);
        let before = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let transfer = handed_to(&pk, &sk, 0x77);

        let mut seen = Replies::default();
        for _ in 0..3 {
            seen.add(Some(transfer.clone()));
            assert!(
                !seen.settled(2, 4),
                "a final state short of a majority does not settle a read"
            );
        }
        seen.add(Some(before.clone()));
        seen.add(Some(transfer.clone()));
        assert!(seen.settled(2, 4));
        assert_eq!(state_of(seen.chosen(2, 4)), Some(transfer.state_id()));
    }

    /// Two final states are unordered, so the merge rule cannot pick one; the
    /// one a majority holds is what most nodes took first, and is the answer
    /// whichever order the replies arrive in, even though the other's target
    /// sorts first.
    #[test]
    fn of_two_final_states_the_majority_is_read_in_any_order() {
        let (pk, sk) = keypair(41);
        let established = handed_to(&pk, &sk, 0x77);
        let late = handed_to(&pk, &sk, 0x01);
        assert!(late.target().to_bytes() < established.target().to_bytes());

        let mut replies = vec![Some(late.clone()), Some(late.clone())];
        replies.extend((0..5).map(|_| Some(established.clone())));
        for rotation in 0..replies.len() {
            let mut order = replies.clone();
            order.rotate_left(rotation);
            let seen = tally(order);
            assert!(seen.settled(2, 4));
            assert_eq!(
                state_of(seen.chosen(2, 4)),
                Some(established.state_id()),
                "rotation {rotation}"
            );
        }
    }

    /// Two final states and no majority: there is no answer, and saying so is
    /// the only honest read. The read is never settled early, so it asks the
    /// whole group before concluding it.
    #[test]
    fn two_final_states_without_a_majority_are_a_fork() {
        let (pk, sk) = keypair(42);
        let before = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let one = handed_to(&pk, &sk, 0x77);
        let other = handed_to(&pk, &sk, 0x01);

        let seen = tally(vec![
            Some(one.clone()),
            Some(one.clone()),
            Some(one.clone()),
            Some(other.clone()),
            Some(other.clone()),
            Some(before.clone()),
            Some(before.clone()),
        ]);
        assert!(!seen.settled(2, 4));
        assert!(matches!(seen.chosen(2, 4), Chosen::Forked));

        // Even a lone peer holding the other side is evidence of a fork: only
        // the owner could have signed it.
        let seen = tally(vec![
            Some(one.clone()),
            Some(one.clone()),
            Some(one.clone()),
            Some(other.clone()),
            Some(before.clone()),
            Some(before.clone()),
            None,
        ]);
        assert!(matches!(seen.chosen(2, 4), Chosen::Forked));
    }

    /// One final state short of a majority and nothing contesting it is a
    /// transfer still spreading. It is returned once corroborated, as any state
    /// is, and one peer's word is not enough.
    #[test]
    fn one_uncontested_final_state_is_read_once_corroborated() {
        let (pk, sk) = keypair(43);
        let before = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let transfer = handed_to(&pk, &sk, 0x77);

        let spreading = tally(vec![
            Some(transfer.clone()),
            Some(transfer.clone()),
            Some(before.clone()),
            Some(before.clone()),
            Some(before.clone()),
        ]);
        assert_eq!(state_of(spreading.chosen(2, 4)), Some(transfer.state_id()));

        let lone = tally(vec![
            Some(transfer.clone()),
            Some(before.clone()),
            Some(before.clone()),
            Some(before.clone()),
        ]);
        assert_eq!(state_of(lone.chosen(2, 4)), Some(before.state_id()));
    }

    /// The whole-group check a recipient runs before relying on a transfer.
    #[test]
    fn finality_names_each_case_a_recipient_must_tell_apart() {
        let (pk, sk) = keypair(44);
        let before = Pointer::create(&sk, &pk, chunk_target(1)).expect("create");
        let transfer = handed_to(&pk, &sk, 0x77);
        let rival = handed_to(&pk, &sk, 0x01);
        let frozen = Pointer::sign(&sk, &pk, FINAL_COUNTER, chunk_target(5)).expect("sign");
        let group = 7;

        let open = finality_status(&tally(vec![Some(before.clone()); 7]), group, group);
        assert_eq!(open, FinalityStatus::Open { counter: Some(0) });

        let status = finality_status(
            &tally(vec![
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(before.clone()),
                None,
                None,
            ]),
            group,
            group,
        );
        match &status {
            FinalityStatus::Final(state) => {
                assert_eq!(state.state_id, transfer.state_id());
                assert_eq!(state.holders, 4);
                assert_eq!(state.transferred_to(), Some([0x77; 32]));
            }
            other => panic!("expected Final, got {other:?}"),
        }

        let status = finality_status(
            &tally(vec![
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(before.clone()),
            ]),
            group,
            3,
        );
        assert!(matches!(status, FinalityStatus::Settling(ref s) if s.holders == 2));

        let status = finality_status(
            &tally(vec![
                Some(rival.clone()),
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(transfer.clone()),
                Some(transfer.clone()),
            ]),
            group,
            5,
        );
        match status {
            FinalityStatus::Forked { states, majority } => {
                assert_eq!(states.len(), 2);
                assert_eq!(
                    states.first().map(|s| s.state_id),
                    Some(transfer.state_id()),
                    "most-held first"
                );
                assert_eq!(majority.map(|s| s.state_id), Some(transfer.state_id()));
            }
            other => panic!("expected Forked, got {other:?}"),
        }

        let status = finality_status(&tally(vec![Some(frozen.clone()); 7]), group, group);
        match status {
            FinalityStatus::Final(state) => assert_eq!(state.transferred_to(), None),
            other => panic!("expected a frozen Final, got {other:?}"),
        }
    }

    /// Four of seven report the transfer and three do not answer. One of the
    /// three could hold a rival, so this is not yet final; a recipient that
    /// relied on it could be relying on the minority side of a fork.
    #[test]
    fn a_majority_is_not_final_while_part_of_the_group_is_silent() {
        let (pk, sk) = keypair(47);
        let transfer = handed_to(&pk, &sk, 0x77);
        let status = finality_status(&tally(vec![Some(transfer.clone()); 4]), 7, 4);
        assert!(
            matches!(&status, FinalityStatus::Unconfirmed(state) if state.holders == 4),
            "got {status:?}"
        );
        let finality = PointerFinality {
            address: transfer.address(),
            group: 7,
            answered: 4,
            status,
        };
        assert!(!finality.is_final());
        assert_eq!(finality.transferred_to(), None);

        // And a rival already seen still refuses a transfer it would fork.
        let other = handed_to(&pk, &sk, 0x01);
        assert!(matches!(
            rival_final(&finality, &other),
            Some(Error::PointerFinal(_))
        ));
        assert!(rival_final(&finality, &transfer).is_none());
    }

    /// The recipient walk, over pointers held in a map.
    async fn walk(
        chain: &HashMap<XorName, Pointer>,
        address: XorName,
        recipient: XorName,
    ) -> Result<()> {
        check_recipient_chain(&address, &recipient, |at| {
            let found = chain.get(&at).cloned();
            async move { Ok(found) }
        })
        .await
    }

    /// A chain of `hops` pointers from `[1; 32]`, each targeting the next,
    /// the last targeting `end`.
    fn chain_to(hops: u8, end: PointerTarget) -> HashMap<XorName, Pointer> {
        (1..=hops)
            .map(|hop| {
                let (pk, sk) = keypair(hop);
                let target = if hop == hops {
                    end
                } else {
                    PointerTarget::new(
                        PointerTargetKind::Pointer,
                        pointer_address(&keypair(hop + 1).0),
                    )
                };
                let record = Pointer::create(&sk, &pk, target).expect("create");
                (record.address(), record)
            })
            .collect()
    }

    /// Readers of the source read it and then every recipient hop, so the
    /// recipient's chain gets one hop fewer than a resolve allows. A chain
    /// that returns to the source at the very last hop is a cycle, and one
    /// too long to follow is refused rather than let through unchecked.
    #[tokio::test]
    async fn a_recipient_chain_is_checked_to_the_last_hop_a_reader_takes() {
        let (source_pk, _) = keypair(200);
        let source = pointer_address(&source_pk);
        let back = PointerTarget::new(PointerTargetKind::Pointer, source);
        let first = pointer_address(&keypair(1).0);
        let allowed = u8::try_from(MAX_POINTER_RESOLVE_DEPTH - 1).expect("small");

        let ends = chain_to(allowed, chunk_target(9));
        assert!(
            walk(&ends, source, first).await.is_ok(),
            "the longest chain a reader can follow"
        );

        let cycles_at_the_end = chain_to(allowed, back);
        assert!(
            matches!(walk(&cycles_at_the_end, source, first).await, Err(Error::InvalidData(m)) if m.contains("cycle")),
            "a chain back to the source at its last hop is a cycle"
        );

        let too_long = chain_to(allowed + 1, chunk_target(9));
        assert!(
            matches!(walk(&too_long, source, first).await, Err(Error::InvalidData(m)) if m.contains("longer")),
            "a chain no reader could follow is refused"
        );

        // One hop longer and back to the source: the walk runs out exactly
        // as it reaches the source, which must not pass as no cycle.
        let cycles_past_the_end = chain_to(allowed + 1, back);
        assert!(walk(&cycles_past_the_end, source, first).await.is_err());

        let nowhere = HashMap::new();
        assert!(
            matches!(walk(&nowhere, source, first).await, Err(Error::InvalidData(m)) if m.contains("does not exist")),
        );
    }

    /// A transfer whose write reported failure, judged by the group after it:
    /// a rival names the refusal, the transfer's own state on a majority is a
    /// transfer that landed with its acknowledgements lost, and anything else
    /// is the write's own error.
    #[test]
    fn a_transfer_the_group_holds_is_done_whatever_its_acknowledgements_said() {
        let (pk, sk) = keypair(48);
        let ours = handed_to(&pk, &sk, 0x77);
        let theirs = handed_to(&pk, &sk, 0x01);
        let finality = |status, answered| PointerFinality {
            address: ours.address(),
            group: 7,
            answered,
            status,
        };
        let lost = || Error::Timeout("no acknowledgement".to_string());

        let landed = after_failed_transfer(
            lost(),
            Ok(finality(FinalityStatus::Final(FinalState::of(&ours, 6)), 7)),
            &ours,
        );
        assert!(
            matches!(&landed, Ok(done) if done.state_id == ours.state_id()),
            "got {landed:?}"
        );
        assert!(after_failed_transfer(
            lost(),
            Ok(finality(
                FinalityStatus::Unconfirmed(FinalState::of(&ours, 4)),
                5
            )),
            &ours,
        )
        .is_ok());

        assert!(matches!(
            after_failed_transfer(
                lost(),
                Ok(finality(
                    FinalityStatus::Final(FinalState::of(&theirs, 6)),
                    7
                )),
                &ours,
            ),
            Err(Error::PointerFinal(_))
        ));
        assert!(matches!(
            after_failed_transfer(
                lost(),
                Ok(finality(
                    FinalityStatus::Settling(FinalState::of(&ours, 2)),
                    7
                )),
                &ours,
            ),
            Err(Error::Timeout(_))
        ));
        assert!(matches!(
            after_failed_transfer(lost(), Err(Error::Timeout("group".to_string())), &ours),
            Err(Error::Timeout(message)) if message == "no acknowledgement"
        ));
    }

    /// An update is refused for any final state the group shows, however few
    /// hold it.
    #[test]
    fn any_final_state_refuses_an_update() {
        let (pk, sk) = keypair(49);
        let transfer = handed_to(&pk, &sk, 0x77);
        let finality = |status| PointerFinality {
            address: transfer.address(),
            group: 7,
            answered: 7,
            status,
        };
        assert!(final_already(&finality(FinalityStatus::Open { counter: Some(3) })).is_none());
        for status in [
            FinalityStatus::Settling(FinalState::of(&transfer, 1)),
            FinalityStatus::Unconfirmed(FinalState::of(&transfer, 4)),
            FinalityStatus::Final(FinalState::of(&transfer, 7)),
        ] {
            assert!(matches!(
                final_already(&finality(status)),
                Some(Error::PointerFinal(_))
            ));
        }
    }

    /// A transfer refused because the pointer was already final on another
    /// state is reported as that, and one that landed is not a refusal.
    #[test]
    fn a_refused_transfer_names_the_final_state_that_beat_it() {
        let (pk, sk) = keypair(45);
        let ours = handed_to(&pk, &sk, 0x77);
        let theirs = handed_to(&pk, &sk, 0x01);
        let finality = |status| PointerFinality {
            address: ours.address(),
            group: 7,
            answered: 7,
            status,
        };

        let beaten = finality(FinalityStatus::Final(FinalState::of(&theirs, 7)));
        assert!(matches!(
            rival_final(&beaten, &ours),
            Some(Error::PointerFinal(_))
        ));
        let landed = finality(FinalityStatus::Final(FinalState::of(&ours, 7)));
        assert!(rival_final(&landed, &ours).is_none());
        assert!(rival_final(&finality(FinalityStatus::Open { counter: Some(3) }), &ours).is_none());
        let forked = finality(FinalityStatus::Forked {
            states: vec![FinalState::of(&ours, 3), FinalState::of(&theirs, 3)],
            majority: None,
        });
        assert!(matches!(
            rival_final(&forked, &ours),
            Some(Error::PointerForked(_))
        ));
    }

    /// A read over a group split by a fork asks everyone, then reports it.
    #[tokio::test(start_paused = true)]
    async fn a_read_of_a_forked_pointer_asks_the_whole_group() {
        let (pk, sk) = keypair(46);
        let one = handed_to(&pk, &sk, 0x77);
        let other = handed_to(&pk, &sk, 0x01);
        let (answered, replies) = collect_read(
            arriving(vec![
                Ok(Some(one.clone())),
                Ok(Some(one.clone())),
                Ok(Some(other.clone())),
                Ok(Some(other.clone())),
                Ok(Some(one.clone())),
                Ok(Some(other.clone())),
                Ok(None),
            ]),
            7,
        )
        .await;
        assert_eq!(answered.count, 7, "no majority, so nobody was skipped");
        assert!(matches!(replies.chosen(2, 4), Chosen::Forked));

        // With a majority it stops as soon as the majority is heard.
        let (answered, replies) = collect_read(
            arriving(vec![
                Ok(Some(other.clone())),
                Ok(Some(one.clone())),
                Ok(Some(one.clone())),
                Ok(Some(one.clone())),
                Ok(Some(one.clone())),
                Ok(Some(other.clone())),
                Ok(Some(one.clone())),
            ]),
            7,
        )
        .await;
        assert_eq!(answered.count, 5);
        assert_eq!(state_of(replies.chosen(2, 4)), Some(one.state_id()));
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
