//! The swarm between clients, over an in-memory stand-in for WebRTC.

use std::time::Duration;

use futures::StreamExt;
use self_encryption::{ChunkInfo, XorName as ChunkName};
use web_time::Instant;

use super::link::FETCH_STALL;
use super::room::{name_of, Identity};
use super::rtc::memory::SharedMemoryRtc;
use super::rtc::{Channel, ChannelEvent};
use super::wire::Message;
use super::*;

const ROOM: RoomKey = [9; 32];
/// The test room's secret, as every reader of its file derives it.
const SECRET: [u8; 32] = [5; 32];
/// The label both ends of a hand-made link use: links made through a room
/// use the room's own.
const TEST_LABEL: &str = "ant-swarm-test";
/// The binding the in-memory stand-in's links share: its SDPs carry no DTLS
/// fingerprints.
fn test_binding() -> link::Binding {
    link::Binding::of(
        "a=fingerprint:sha-256 AA:BB\r\n",
        "a=fingerprint:sha-256 CC:DD\r\n",
        true,
    )
}

fn client(rtc: &SharedMemoryRtc) -> Swarm {
    let swarm = Swarm::with_rtc(Some(Arc::new(rtc.clone())), 1024 * 1024);
    swarm.inner.rooms().push(Arc::new(
        Room::new(ROOM, 1, SECRET).unwrap_or_else(|| panic!("a room")),
    ));
    swarm
}

/// The client's membership of the test room.
fn joined(swarm: &Swarm) -> Arc<Room> {
    swarm
        .inner
        .room(&ROOM)
        .unwrap_or_else(|| panic!("not in the room"))
}

/// Makes `addresses` records of the test room's file, for each client.
fn knows(clients: &[&Swarm], addresses: &[XorName]) {
    for client in clients {
        client.register(&ROOM, addresses.iter().copied());
    }
}

fn record(seed: u8, len: usize) -> (XorName, Bytes) {
    let content = Bytes::from((0..len).map(|n| (n % 251) as u8 ^ seed).collect::<Vec<_>>());
    (ant_protocol::compute_address(&content), content)
}

async fn eventually(what: &str, check: impl Fn() -> bool) {
    for _ in 0..200 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("never: {what}");
}

/// An offer from `offerer`'s side, answered on `answerer`'s, both attached.
async fn link(rtc: &SharedMemoryRtc, offerer: &Swarm, answerer: &Swarm, offer_id: OfferId) {
    link_naming(rtc, offerer, answerer, offer_id, None, None).await;
}

/// As [`link`], each end naming the other as its introducing node did.
async fn link_naming(
    rtc: &SharedMemoryRtc,
    offerer: &Swarm,
    answerer: &Swarm,
    offer_id: OfferId,
    offerer_sees: Option<PeerRef>,
    answerer_sees: Option<PeerRef>,
) {
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(answerer
        .inner
        .attach(
            &joined(answerer),
            false,
            offer_id,
            test_binding(),
            answerer_sees,
            channel
        )
        .is_some());
    let channel = offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(offerer
        .inner
        .attach(
            &joined(offerer),
            true,
            offer_id,
            test_binding(),
            offerer_sees,
            channel
        )
        .is_some());
}

/// One end of a link driven by hand, as a peer that behaves however a test
/// needs, proven as a reader of the room's file; the other end is attached
/// to `swarm`.
async fn hand_driven(rtc: &SharedMemoryRtc, swarm: &Swarm) -> Channel {
    hand_driven_in(rtc, swarm, &joined(swarm)).await
}

/// A hand-driven member's key: the same seed, the same member.
fn member(seed: u8) -> Identity {
    Identity::from_seed(&[seed; 32]).unwrap_or_else(|| panic!("a key"))
}

/// The `Hello` hand-driven member `seed` sends.
fn hello_from(seed: u8) -> Message {
    Message::Hello {
        key: member(seed).public().to_vec(),
    }
}

/// The proof hand-driven member `seed` sends on a link under `offer_id`, as
/// its offerer or not, knowing `secret`.
fn proof_from(seed: u8, secret: &[u8; 32], offer_id: &OfferId, offerer: bool) -> Message {
    Message::Proof {
        mac: *link::proof(secret, &test_binding().proof, offer_id, offerer).as_bytes(),
        signature: member(seed)
            .sign(&link::signed(&test_binding().proof, offer_id, offerer))
            .unwrap_or_else(|| panic!("a signature")),
    }
}

/// The proof the hand-driven end, member 3 and the offerer, sends for
/// `secret`.
fn offerer_proof(secret: &[u8; 32]) -> Message {
    proof_from(3, secret, &[1; 16], true)
}

/// The next message the hand-driven end receives.
async fn next_message(channel: &mut Channel) -> Message {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), channel.events.next())
            .await
            .unwrap_or_else(|_| panic!("no message"));
        match event {
            Some(ChannelEvent::Message(bytes)) => {
                return Message::decode(&bytes).unwrap_or_else(|e| panic!("{e}"))
            }
            Some(ChannelEvent::Open) => {}
            other => panic!("channel ended: {other:?}"),
        }
    }
}

async fn send(channel: &Channel, message: Message) {
    channel
        .sender
        .send(message.encode())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
}

#[tokio::test]
async fn a_record_one_client_holds_is_served_to_another() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    let (address, content) = record(1, 40_000);
    knows(&[&alice, &bob], &[address]);
    alice.seed(address, content.clone());
    link(&rtc, &alice, &bob, [7; 16]).await;
    eventually("bob learns what alice holds", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;

    assert_eq!(bob.fetch(&address).await, Some(content.clone()));
    assert_eq!(bob.stats().records_from_peers, 1);
    assert_eq!(bob.stats().bytes_from_peers, content.len() as u64);
    eventually("alice counts the upload", || {
        alice.stats().records_served == 1
    })
    .await;
    // And so does the room they share, under its tag.
    let fetched = bob.room_stats();
    assert_eq!(
        fetched.first().map(|room| (
            room.tag.clone(),
            room.records_from_peers,
            room.bytes_from_peers
        )),
        Some((room_tag(&SECRET), 1, content.len() as u64))
    );

    // Bob now holds it too, and says so to the clients he meets.
    let carol = client(&rtc);
    knows(&[&carol], &[address]);
    link(&rtc, &bob, &carol, [8; 16]).await;
    eventually("carol learns what bob holds", || {
        carol.inner.links().iter().any(|link| link.has(&address))
    })
    .await;
    assert_eq!(carol.fetch(&address).await, Some(content));
}

#[tokio::test]
async fn a_queued_read_learns_when_a_linked_reader_has_its_record() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    let (address, content) = record(5, 4_000);
    let (stray, _) = record(6, 4_000);
    knows(&[&alice, &bob], &[address]);
    // A record of no room's file, or one nobody announced: the nodes'.
    assert!(!bob.could_serve(&stray));
    assert!(!bob.could_serve(&address));
    link(&rtc, &alice, &bob, [7; 16]).await;
    eventually("the link opens", || bob.stats().links == 1).await;
    assert!(!bob.could_serve(&address));
    alice.seed(address, content.clone());
    eventually("bob learns what alice holds", || bob.could_serve(&address)).await;
    // And once bob holds it himself.
    assert_eq!(bob.fetch(&address).await, Some(content));
    assert!(bob.could_serve(&address));
}

#[tokio::test]
async fn a_link_whose_first_message_comes_before_its_opening_still_opens() {
    // The answerer learns its channel opened only after the offerer's
    // `Hello`, as webrtc-rs may report it: the link opens and proves itself.
    let rtc = SharedMemoryRtc::default();
    rtc.0.open_late.store(true, Ordering::Release);
    let (alice, bob) = (client(&rtc), client(&rtc));
    link(&rtc, &alice, &bob, [7; 16]).await;
    eventually("both ends prove the link", || {
        alice.stats().links == 1 && bob.stats().links == 1
    })
    .await;
    assert_eq!(bob.stats().discovery.links_proven, 1);
}

#[tokio::test]
async fn a_record_nobody_announced_is_left_to_the_nodes() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    link(&rtc, &alice, &bob, [7; 16]).await;
    eventually("the link opens", || bob.stats().links == 1).await;
    let (address, _) = record(2, 100);
    knows(&[&alice, &bob], &[address]);
    assert_eq!(bob.fetch(&address).await, None);
    // Nothing asked, fetched or served: only the link counts, and how it
    // was made.
    assert_eq!(
        bob.stats(),
        SwarmStats {
            links: 1,
            rooms: 1,
            discovery: DiscoveryStats {
                channels_opened: 1,
                links_proven: 1,
                ..DiscoveryStats::default()
            },
            ..SwarmStats::default()
        }
    );
}

#[tokio::test]
async fn a_record_that_fails_its_check_ends_the_link() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut mallory = hand_driven(&rtc, &bob).await;
    let (address, _) = record(3, 1000);
    knows(&[&bob], &[address]);
    send(
        &mallory,
        Message::Have {
            addresses: vec![address],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;

    let fetch = bob.fetch(&address);
    let serve = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut mallory).await {
                let forged = Bytes::from_static(b"not the record");
                for piece in wire::pieces(id, &forged) {
                    send(&mallory, piece).await;
                }
                return;
            }
        }
    };
    let (fetched, ()) = futures::join!(fetch, serve);
    assert_eq!(fetched, None);
    assert_eq!(bob.stats().records_rejected, 1);
    eventually("the link is gone", || bob.inner.links().is_empty()).await;
}

#[tokio::test]
async fn a_peer_that_stalls_costs_one_stall_and_is_told_to_stop() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut slow = hand_driven(&rtc, &bob).await;
    let (address, content) = record(4, 3 * wire::FRAME_BYTES);
    knows(&[&bob], &[address]);
    send(
        &slow,
        Message::Have {
            addresses: vec![address],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;

    let started = Instant::now();
    let fetch = bob.fetch(&address);
    let serve = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut slow).await {
                // One piece, then nothing.
                if let Some(first) = wire::pieces(id, &content).next() {
                    send(&slow, first).await;
                }
                loop {
                    if let Message::Cancel { id: cancelled } = next_message(&mut slow).await {
                        assert_eq!(cancelled, id);
                        return;
                    }
                }
            }
        }
    };
    let (fetched, ()) = futures::join!(fetch, serve);
    assert_eq!(fetched, None);
    let waited = started.elapsed();
    assert!(waited >= FETCH_STALL, "{waited:?}");
    assert!(waited < FETCH_STALL * 3, "{waited:?}");
    // A slow peer is not a bad one: the link stays, but is passed over a while.
    let link = bob.inner.links().first().cloned();
    let link = link.unwrap_or_else(|| panic!("no link"));
    assert!(link.has(&address));
    assert!(!link.offers(&address));
}

#[tokio::test]
async fn a_peer_that_dribbles_a_record_runs_out_of_time() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut slow = hand_driven(&rtc, &bob).await;
    let (address, content) = record(8, 3 * wire::FRAME_BYTES);
    knows(&[&bob], &[address]);
    send(
        &slow,
        Message::Have {
            addresses: vec![address],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;

    let started = Instant::now();
    let fetch = async {
        let fetched = bob.fetch(&address).await;
        (fetched, started.elapsed())
    };
    let serve = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut slow).await {
                // Each piece inside the stall limit, the whole far too slowly.
                for piece in wire::pieces(id, &content) {
                    send(&slow, piece).await;
                    tokio::time::sleep(FETCH_STALL * 3 / 4).await;
                }
                return;
            }
        }
    };
    let ((fetched, waited), ()) = futures::join!(fetch, serve);
    assert_eq!(fetched, None);
    assert!(
        waited < FETCH_STALL + Duration::from_millis(600),
        "{waited:?}"
    );
}

#[tokio::test]
async fn missing_and_busy_answers_fall_back_at_once() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut peer = hand_driven(&rtc, &bob).await;
    let (gone, _) = record(5, 10);
    let (busy, _) = record(6, 10);
    knows(&[&bob], &[gone, busy]);
    send(
        &peer,
        Message::Have {
            addresses: vec![gone, busy],
        },
    )
    .await;
    eventually("bob learns the claims", || {
        bob.inner.links().iter().any(|link| link.has(&busy))
    })
    .await;

    let started = Instant::now();
    let fetches = async { (bob.fetch(&gone).await, bob.fetch(&busy).await) };
    let answers = async {
        for _ in 0..2 {
            loop {
                if let Message::Get { id, address } = next_message(&mut peer).await {
                    let reply = if address == gone {
                        Message::Missing { id }
                    } else {
                        Message::Busy { id }
                    };
                    send(&peer, reply).await;
                    break;
                }
            }
        }
    };
    let ((gone_fetch, busy_fetch), ()) = futures::join!(fetches, answers);
    assert_eq!((gone_fetch, busy_fetch), (None, None));
    assert!(started.elapsed() < FETCH_STALL);
    // A record the peer no longer has is not asked of it again; a busy one is.
    let link = bob.inner.links().first().cloned();
    let link = link.unwrap_or_else(|| panic!("no link"));
    assert!(!link.has(&gone));
    assert!(link.has(&busy));
}

#[tokio::test]
async fn a_read_that_asks_peers_again_goes_on_from_what_it_spent() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut peer = hand_driven(&rtc, &bob).await;
    let (busy, _) = record(6, 10);
    knows(&[&bob], &[busy]);
    send(
        &peer,
        Message::Have {
            addresses: vec![busy],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&busy))
    })
    .await;
    let answer_busy = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut peer).await {
                send(&peer, Message::Busy { id }).await;
                return;
            }
        }
    };
    let read = Pace::default();
    let (first, ()) = futures::join!(bob.fetch_paced(&busy, &read), answer_busy);
    assert_eq!(first, None);
    assert_eq!(bob.stats().asks_busy, 1);
    // The same read asks again, as a queued browser read does when a peer
    // offers its record: the peer it tried is not asked twice.
    assert_eq!(bob.fetch_paced(&busy, &read).await, None);
    assert_eq!(bob.stats().asks_busy, 1);
    // A new read asks it afresh.
    let answer_busy = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut peer).await {
                send(&peer, Message::Busy { id }).await;
                return;
            }
        }
    };
    let new_read = Pace::default();
    let (again, ()) = futures::join!(bob.fetch_paced(&busy, &new_read), answer_busy);
    assert_eq!(again, None);
    assert_eq!(bob.stats().asks_busy, 2);
}

#[tokio::test]
async fn an_ask_that_finds_no_peer_spends_none_of_the_reads_time() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    let (address, content) = record(9, 4_000);
    knows(&[&alice, &bob], &[address]);
    link(&rtc, &alice, &bob, [7; 16]).await;
    eventually("the link opens", || bob.stats().links == 1).await;
    // Nobody offers the record yet: the read's time with peers has not
    // started, however long it then waits at the nodes.
    let read = Pace::default();
    assert_eq!(bob.fetch_paced(&address, &read).await, None);
    assert!(read
        .spent
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .deadline
        .is_none());
    // A peer that offers it later still serves the same read.
    alice.seed(address, content.clone());
    eventually("bob learns what alice holds", || bob.could_serve(&address)).await;
    assert_eq!(bob.fetch_paced(&address, &read).await, Some(content));
}

#[test]
fn a_room_starts_a_bounded_number_of_answers() {
    let room = Room::new([3; 32], 1, SECRET).unwrap_or_else(|| panic!("a room"));
    for _ in 0..room::ANSWERS_PER_WINDOW {
        assert!(room.may_answer());
    }
    assert!(!room.may_answer());
}

#[tokio::test]
async fn pieces_out_of_order_end_the_link() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let mut mallory = hand_driven(&rtc, &bob).await;
    let (address, content) = record(7, 3 * wire::FRAME_BYTES);
    knows(&[&bob], &[address]);
    send(
        &mallory,
        Message::Have {
            addresses: vec![address],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;
    let fetch = bob.fetch(&address);
    let serve = async {
        loop {
            if let Message::Get { id, .. } = next_message(&mut mallory).await {
                if let Some(second) = wire::pieces(id, &content).nth(1) {
                    send(&mallory, second).await;
                }
                return;
            }
        }
    };
    let (fetched, ()) = futures::join!(fetch, serve);
    assert_eq!(fetched, None);
    eventually("the link is gone", || bob.inner.links().is_empty()).await;
}

#[tokio::test]
async fn two_links_between_the_same_clients_settle_on_one() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    // Each offered to the other at once.
    link(&rtc, &alice, &bob, [3; 16]).await;
    link(&rtc, &bob, &alice, [4; 16]).await;
    eventually("one link each", || {
        alice.inner.links().len() == 1 && bob.inner.links().len() == 1
    })
    .await;
    let kept = |swarm: &Swarm| swarm.inner.links().first().map(|link| link.offer_id);
    assert_eq!(kept(&alice), kept(&bob));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(alice.stats().links, 1);
    assert_eq!(bob.stats().links, 1);
}

/// Waits for the hand-driven end to see its connection close.
async fn until_closed(channel: &mut Channel) {
    while let Some(event) = channel.events.next().await {
        if matches!(event, ChannelEvent::Closed) {
            return;
        }
    }
}

#[tokio::test]
async fn records_held_already_are_told_to_links_opened_before_they_were_registered() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, content) = record(1, 2 * wire::FRAME_BYTES);
    // Held from an earlier read, in a room left since: not a record of this
    // room yet when the link opens.
    assert!(alice.inner.store.insert(address, content));
    let mut peer = hand_driven(&rtc, &alice).await;
    eventually("the link opens", || alice.stats().links == 1).await;
    knows(&[&alice], &[address]);
    loop {
        if let Message::Have { addresses } = next_message(&mut peer).await {
            if addresses.contains(&address) {
                break;
            }
        }
    }
}

#[tokio::test]
async fn a_duplicate_settled_on_sight_still_gives_its_peer_this_ends_proof() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    // A link to a peer, kept.
    let _kept = hand_driven(&rtc, &alice).await;
    eventually("the first link opens", || alice.stats().links == 1).await;
    // A second link to the same peer, under a higher offer id, so the
    // duplicate: the peer's name and proof are waiting on it before it starts,
    // and it is settled away the moment they are read.
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let mut second = offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"));
    send(&second, hello_from(3)).await;
    send(&second, proof_from(3, &room.secret, &[9; 16], true)).await;
    assert!(alice
        .inner
        .attach(&room, false, [9; 16], test_binding(), None, channel)
        .is_some());
    // This end's name and proof still reach the peer, for it to settle the
    // same way.
    let (mut hello, mut proof) = (false, false);
    while !(hello && proof) {
        match next_message(&mut second).await {
            Message::Hello { .. } => hello = true,
            Message::Proof { .. } => proof = true,
            other => panic!("before the proof: {other:?}"),
        }
    }
    eventually("only the first link is open", || alice.stats().links == 1).await;
    // The duplicate is let go once its grace is over.
    tokio::time::timeout(Duration::from_secs(5), until_closed(&mut second))
        .await
        .unwrap_or_else(|_| panic!("the duplicate's connection never closed"));
    eventually("the duplicate is let go", || alice.inner.links().len() == 1).await;
}

#[tokio::test]
async fn a_link_remembers_its_peers_newest_names_at_the_newest_nodes() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let _peer = hand_driven(&rtc, &alice).await;
    let link = alice
        .inner
        .links()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("attached"));
    // A long read meets the peer through more rendezvous nodes than a link
    // remembers: the newest ones are kept.
    for node in 1..=6u8 {
        link.also_known_as(Some(([node; 32], [node; 16])));
    }
    assert!(!link.known_at(&[1; 32]) && !link.known_at(&[2; 32]));
    for node in 3..=6u8 {
        assert!(link.known_at(&[node; 32]), "node {node}");
    }
    // A newer name at a node already known replaces the older one, rather
    // than pushing another node out.
    link.also_known_as(Some(([6; 32], [60; 16])));
    for node in 3..=6u8 {
        assert!(link.known_at(&[node; 32]), "node {node}");
    }
}

#[tokio::test]
async fn a_member_cannot_go_by_another_members_key() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    // A member that knows the room's secret, naming itself by member 4's key
    // (sent in the clear to every peer member 4 links to), but able to sign
    // only as itself.
    let mut mallory = unproven(&rtc, &alice, &room).await;
    send(&mallory, hello_from(4)).await;
    send(&mallory, proof_from(5, &room.secret, &[1; 16], true)).await;
    tokio::time::timeout(Duration::from_secs(5), until_closed(&mut mallory))
        .await
        .unwrap_or_else(|_| panic!("a link under another member's key stayed"));
    assert_eq!(alice.stats().links, 0);
}

/// A link to `swarm` in `room` under `offer_id`, its hand-driven end not yet
/// heard from.
async fn attached_under(
    rtc: &SharedMemoryRtc,
    swarm: &Swarm,
    room: &Arc<Room>,
    offer_id: OfferId,
) -> Channel {
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(swarm
        .inner
        .attach(room, false, offer_id, test_binding(), None, channel)
        .is_some());
    offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"))
}

#[tokio::test]
async fn an_unproven_link_under_a_members_key_settles_nothing_away() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    // An impostor names itself by member 4's key, under the lower offer id,
    // and never proves it.
    let impostor = attached_under(&rtc, &alice, &room, [0; 16]).await;
    send(&impostor, hello_from(4)).await;
    // Member 4 itself links, and proves it.
    let genuine = attached_under(&rtc, &alice, &room, [9; 16]).await;
    send(&genuine, hello_from(4)).await;
    send(&genuine, proof_from(4, &room.secret, &[9; 16], true)).await;
    eventually("member 4's link opens", || alice.stats().links == 1).await;
    // It stays: the impostor's link, unproven, is no twin of it.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(alice.stats().links, 1);
    let genuine_link = alice
        .inner
        .links()
        .iter()
        .find(|link| link.offer_id == [9; 16])
        .cloned()
        .unwrap_or_else(|| panic!("member 4's link"));
    assert!(genuine_link.is_open() && !genuine_link.settled_away());
    drop(impostor);
}

#[tokio::test]
async fn a_signature_made_for_another_link_proves_nothing() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    // Member 4's own key and signature, but over another connection's
    // binding: replayed from a link it made elsewhere.
    let elsewhere = link::signed(&[0xee; 32], &[1; 16], true);
    let mut replayed = unproven(&rtc, &alice, &room).await;
    send(&replayed, hello_from(4)).await;
    send(
        &replayed,
        Message::Proof {
            mac: *link::proof(&room.secret, &test_binding().proof, &[1; 16], true).as_bytes(),
            signature: member(4)
                .sign(&elsewhere)
                .unwrap_or_else(|| panic!("a signature")),
        },
    )
    .await;
    tokio::time::timeout(Duration::from_secs(5), until_closed(&mut replayed))
        .await
        .unwrap_or_else(|_| panic!("a replayed signature opened the link"));
    assert_eq!(alice.stats().links, 0);
}

#[tokio::test]
async fn a_link_closed_then_settled_away_is_still_let_go() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let _peer = hand_driven(&rtc, &alice).await;
    let link = alice
        .inner
        .links()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("attached"));
    eventually("the link opens", || link.is_open()).await;
    // Closed (its room left, say), then settled away as a duplicate before
    // its reader ends: it still leaves the links once the grace is over.
    link.close();
    link.settle_away(&Arc::downgrade(&alice.inner));
    let gone = tokio::time::timeout(Duration::from_secs(5), async {
        while !alice.inner.links().is_empty() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(
        gone.is_ok(),
        "a closed link settled away stayed among the links"
    );
}

#[tokio::test]
async fn a_link_settled_away_stays_connected_for_its_peer_to_settle_too() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut peer = hand_driven(&rtc, &alice).await;
    let link = alice
        .inner
        .links()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("attached"));
    eventually("the link opens", || link.is_open()).await;
    link.settle_away(&Arc::downgrade(&alice.inner));
    // The link stops at once, but still counts against the links' limits
    // while its connection stays, for the peer to take this end's proof and
    // settle on the same link itself.
    assert!(link.is_closed() && link.settled_away());
    assert_eq!(alice.stats().links, 0);
    let early = tokio::time::timeout(Duration::from_secs(1), until_closed(&mut peer)).await;
    assert!(early.is_err(), "the connection closed before the grace");
    assert_eq!(alice.inner.links().len(), 1);
    // Then both go.
    tokio::time::timeout(Duration::from_secs(5), until_closed(&mut peer))
        .await
        .unwrap_or_else(|_| panic!("the connection never closed"));
    eventually("the link is let go", || alice.inner.links().is_empty()).await;
}

#[tokio::test]
async fn a_peer_sends_only_so_many_records_at_once() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut asker = hand_driven(&rtc, &alice).await;
    let records: Vec<_> = (0..6)
        .map(|seed| record(seed, 2 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice], &addresses);
    for (address, content) in &records {
        alice.seed(*address, content.clone());
    }
    // Ask for every record at once, reading nothing until all are asked.
    for (id, (address, _)) in (0u32..).zip(&records) {
        send(
            &asker,
            Message::Get {
                id,
                address: *address,
            },
        )
        .await;
    }
    let mut busy = 0;
    let mut finished = 0;
    while busy + finished < records.len() {
        match next_message(&mut asker).await {
            Message::Busy { .. } => busy += 1,
            Message::Piece {
                total,
                offset,
                data,
                ..
            } if offset as usize + data.len() == total as usize => finished += 1,
            _ => {}
        }
    }
    assert_eq!(finished, 2);
    assert_eq!(busy, 4);
}

#[tokio::test]
async fn a_peer_still_reading_sends_one_record_at_a_time() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut asker = hand_driven(&rtc, &alice).await;
    let records: Vec<_> = (0..3)
        .map(|seed| record(seed, 2 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice], &addresses);
    for (address, content) in &records {
        alice.seed(*address, content.clone());
    }
    // A read of Alice's own runs: her download keeps her CPU and line.
    let reading = alice.reading(&[0; 32]);
    for (id, (address, _)) in (0u32..).zip(&records) {
        send(
            &asker,
            Message::Get {
                id,
                address: *address,
            },
        )
        .await;
    }
    let mut busy = 0;
    let mut finished = 0;
    while busy + finished < records.len() {
        match next_message(&mut asker).await {
            Message::Busy { .. } => busy += 1,
            Message::Piece {
                total,
                offset,
                data,
                ..
            } if offset as usize + data.len() == total as usize => finished += 1,
            _ => {}
        }
    }
    assert_eq!((finished, busy), (1, 2));
    // Done reading, she sends as many as any peer does.
    drop(reading);
    for (id, (address, _)) in (10u32..).zip(&records) {
        send(
            &asker,
            Message::Get {
                id,
                address: *address,
            },
        )
        .await;
    }
    let (mut busy, mut finished) = (0, 0);
    while busy + finished < records.len() {
        match next_message(&mut asker).await {
            Message::Busy { .. } => busy += 1,
            Message::Piece {
                total,
                offset,
                data,
                ..
            } if offset as usize + data.len() == total as usize => finished += 1,
            _ => {}
        }
    }
    assert_eq!((finished, busy), (2, 1));
}

#[tokio::test]
async fn a_peer_still_reading_sends_one_record_at_a_time_across_its_links() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    let mut first = hand_driven_in(&rtc, &alice, &room).await;
    // Another member on a link of its own.
    let mut second = unproven(&rtc, &alice, &room).await;
    send(&second, hello_from(4)).await;
    send(&second, proof_from(4, &room.secret, &[1; 16], true)).await;
    eventually("both links open", || alice.stats().links == 2).await;
    let records: Vec<_> = (0..2)
        .map(|seed| record(seed, 2 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice], &addresses);
    for (address, content) in &records {
        alice.seed(*address, content.clone());
    }
    let _reading = alice.reading(&[0; 32]);
    // One ask on each link, at once.
    for (peer, (address, _)) in [&first, &second].into_iter().zip(&records) {
        send(
            peer,
            Message::Get {
                id: 0,
                address: *address,
            },
        )
        .await;
    }
    let mut outcomes = Vec::new();
    for peer in [&mut first, &mut second] {
        loop {
            match next_message(peer).await {
                Message::Busy { .. } => {
                    outcomes.push("busy");
                    break;
                }
                Message::Piece {
                    total,
                    offset,
                    data,
                    ..
                } if offset as usize + data.len() == total as usize => {
                    outcomes.push("sent");
                    break;
                }
                _ => {}
            }
        }
    }
    outcomes.sort_unstable();
    assert_eq!(outcomes, ["busy", "sent"]);
}

#[tokio::test]
async fn reads_past_what_a_link_takes_go_to_the_nodes() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    let records: Vec<_> = (0..5)
        .map(|seed| record(seed, 3 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice, &bob], &addresses);
    for (address, content) in &records {
        alice.seed(*address, content.clone());
    }
    link(&rtc, &alice, &bob, [7; 16]).await;
    let last = records
        .last()
        .map(|(address, _)| *address)
        .unwrap_or_default();
    eventually("bob learns what alice holds", || {
        bob.inner.links().iter().any(|link| link.has(&last))
    })
    .await;
    // More reads at once than one link takes: one is asked, one waits its
    // turn, and the rest go to the nodes (`None`) rather than queue.
    let fetches = records.iter().map(|(address, _)| bob.fetch(address));
    let fetched = futures::future::join_all(fetches).await;
    let expected: Vec<_> = records
        .iter()
        .enumerate()
        .map(|(index, (_, content))| (index < 2).then(|| content.clone()))
        .collect();
    assert_eq!(fetched, expected);
    assert_eq!(bob.stats().records_from_peers, 2);
    // Each read past them asked the link nothing, so none counts as failed.
    assert_eq!(bob.stats().asks_failed, 0);
    assert_eq!(bob.stats().asks_busy, 0);
    // A read once the link is free is asked again.
    let (address, content) = records.last().unwrap_or_else(|| panic!("records"));
    assert_eq!(bob.fetch(address).await, Some(content.clone()));
}

#[tokio::test]
async fn a_link_is_asked_for_one_record_at_a_time() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let records: Vec<_> = (0..2)
        .map(|seed| record(seed, 3 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice], &addresses);
    let mut peer = hand_driven(&rtc, &alice).await;
    send(
        &peer,
        Message::Have {
            addresses: addresses.clone(),
        },
    )
    .await;
    let link = alice
        .inner
        .links()
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("attached"));
    eventually("the link offers both records", || {
        addresses.iter().all(|address| link.offers(address))
    })
    .await;
    let reads = {
        let alice = alice.clone();
        let addresses = addresses.clone();
        tokio::spawn(async move {
            futures::future::join_all(addresses.iter().map(|address| alice.fetch(address))).await
        })
    };
    // Two reads at once, one ask: the next comes once the first record is in.
    for _ in &records {
        let (id, address) = loop {
            if let Message::Get { id, address } = next_message(&mut peer).await {
                break (id, address);
            }
        };
        // What the reader now holds it may say meanwhile, but ask nothing.
        let asked = tokio::time::timeout(Duration::from_millis(300), async {
            loop {
                if let Message::Get { .. } = next_message(&mut peer).await {
                    return;
                }
            }
        });
        assert!(asked.await.is_err(), "a second ask while one is open");
        let content = records
            .iter()
            .find(|(held, _)| *held == address)
            .map(|(_, content)| content.clone())
            .unwrap_or_else(|| panic!("asked for a record of no read"));
        for piece in wire::pieces(id, &content) {
            send(&peer, piece).await;
        }
    }
    let fetched = tokio::time::timeout(Duration::from_secs(10), reads)
        .await
        .unwrap_or_else(|_| panic!("the reads did not end"))
        .unwrap_or_else(|e| panic!("{e}"));
    let expected: Vec<_> = records
        .iter()
        .map(|(_, content)| Some(content.clone()))
        .collect();
    assert_eq!(fetched, expected);
}

#[tokio::test]
async fn two_records_sent_at_once_share_the_channel() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut asker = hand_driven(&rtc, &alice).await;
    // A channel that never makes a send wait, as a browser's often does.
    let records: Vec<_> = (0..2)
        .map(|seed| record(seed, 32 * wire::FRAME_BYTES))
        .collect();
    let addresses: Vec<_> = records.iter().map(|(address, _)| *address).collect();
    knows(&[&alice], &addresses);
    for (address, content) in &records {
        alice.seed(*address, content.clone());
    }
    for (id, address) in (0u32..).zip(&addresses) {
        send(
            &asker,
            Message::Get {
                id,
                address: *address,
            },
        )
        .await;
    }
    let mut order = Vec::new();
    let mut finished = 0;
    while finished < records.len() {
        if let Message::Piece {
            id,
            total,
            offset,
            data,
        } = next_message(&mut asker).await
        {
            order.push(id);
            if offset as usize + data.len() == total as usize {
                finished += 1;
            }
        }
    }
    // The second record's first piece comes long before the first record's
    // last one.
    let second = order.iter().position(|id| *id == 1);
    let first_done = order.iter().rposition(|id| *id == 0);
    assert!(second < first_done, "{order:?}");
    assert!(
        second.is_some_and(|at| at <= 2 * super::link::PIECES_PER_TURN),
        "{order:?}"
    );
}

#[tokio::test]
async fn leaving_a_room_closes_its_links_and_refuses_new_ones() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    link(&rtc, &alice, &bob, [7; 16]).await;
    let room = joined(&alice);
    alice.shutdown();
    eventually("both ends close", || {
        alice.inner.links().is_empty() && bob.inner.links().is_empty()
    })
    .await;
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, _) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(alice
        .inner
        .attach(&room, false, [1; 16], test_binding(), None, channel)
        .is_none());
    drop(offer);
}

#[tokio::test]
async fn a_record_of_another_room_is_neither_announced_nor_served() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    const OTHER: RoomKey = [8; 32];
    alice.inner.rooms().push(Arc::new(
        Room::new(OTHER, 1, SECRET).unwrap_or_else(|| panic!("a room")),
    ));
    let (shared, shared_content) = record(1, 100);
    let (private, private_content) = record(2, 100);
    knows(&[&alice], &[shared]);
    alice.register(&OTHER, [private]);
    alice.seed(shared, shared_content);
    alice.seed(private, private_content);
    // A record of no room's file is not even kept.
    let (stray, stray_content) = record(3, 100);
    alice.seed(stray, stray_content);
    assert_eq!(alice.inner.store.get(&stray), None);

    let mut peer = hand_driven(&rtc, &alice).await;
    let announced = loop {
        if let Message::Have { addresses } = next_message(&mut peer).await {
            break addresses;
        }
    };
    assert_eq!(announced, vec![shared]);
    send(
        &peer,
        Message::Get {
            id: 1,
            address: private,
        },
    )
    .await;
    let answer = loop {
        let message = next_message(&mut peer).await;
        if matches!(message, Message::Missing { .. } | Message::Piece { .. }) {
            break message;
        }
    };
    assert_eq!(answer, Message::Missing { id: 1 });
}

#[tokio::test]
async fn each_room_counts_what_its_own_links_carried() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    const OTHER: RoomKey = [8; 32];
    const OTHER_SECRET: [u8; 32] = [6; 32];
    let other = Arc::new(Room::new(OTHER, 1, OTHER_SECRET).unwrap_or_else(|| panic!("a room")));
    alice.inner.rooms().push(Arc::clone(&other));
    let (address, content) = record(4, 1000);
    alice.register(&OTHER, [address]);
    alice.seed(address, content.clone());
    let mut peer = hand_driven_in(&rtc, &alice, &other).await;
    send(&peer, Message::Get { id: 1, address }).await;
    loop {
        if let Message::Piece {
            total,
            offset,
            data,
            ..
        } = next_message(&mut peer).await
        {
            if u64::from(offset) + data.len() as u64 == u64::from(total) {
                break;
            }
        }
    }
    eventually("alice counts the upload", || {
        alice.stats().records_served == 1
    })
    .await;

    // The upload is the other room's, which the page reading its file shows,
    // and not the test room's.
    let stats = alice.room_stats();
    let of = |secret: &[u8; 32]| {
        let tag = room_tag(secret);
        stats.iter().find(|room| room.tag == tag).cloned()
    };
    assert_ne!(room_tag(&OTHER_SECRET), room_tag(&SECRET));
    let served = |room: Option<RoomStats>| {
        room.map(|room| (room.records_served, room.bytes_served, room.links))
    };
    assert_eq!(
        served(of(&OTHER_SECRET)),
        Some((1, content.len() as u64, 1))
    );
    assert_eq!(served(of(&SECRET)), Some((0, 0, 0)));
}

#[tokio::test]
async fn a_room_names_its_open_links_by_fingerprint_once_proven() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    let peer = unproven(&rtc, &alice, &room).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // A channel that has proved nothing names no open link.
    assert_eq!(
        alice
            .room_stats()
            .first()
            .map(|room| room.open_links.clone()),
        Some(Vec::new())
    );
    send(&peer, hello_from(3)).await;
    send(&peer, offerer_proof(&room.secret)).await;
    eventually("the link opens", || alice.stats().links == 1).await;
    assert_eq!(
        alice
            .room_stats()
            .first()
            .map(|room| room.open_links.clone()),
        Some(vec!["sha-256 AA:BB".to_string()])
    );
}

#[test]
fn a_public_file_room_is_its_address() {
    // An empty map is no file's, and has no room.
    assert!(room_of(&DataMap::new(Vec::new())).is_none());
    let map = DataMap::new(
        (0..3u8)
            .map(|index| ChunkInfo {
                index: usize::from(index),
                dst_hash: ChunkName([index + 1; 32]),
                src_hash: ChunkName([index + 9; 32]),
                src_size: 1024,
            })
            .collect(),
    );
    let (address, _) =
        crate::client_engine::files::public_map_record(&map).unwrap_or_else(|e| panic!("{e}"));
    let (room, secret) = room_of(&map).unwrap_or_else(|| panic!("a room"));
    assert_eq!(room, address);
    // The secret is not the room's key: knowing the key gives no proof.
    assert_ne!(secret, room);
}

/// A hand-driven end of a link to `swarm` in `room`, proven.
async fn hand_driven_in(rtc: &SharedMemoryRtc, swarm: &Swarm, room: &Arc<Room>) -> Channel {
    let channel = unproven(rtc, swarm, room).await;
    send(&channel, hello_from(3)).await;
    send(&channel, offerer_proof(&room.secret)).await;
    channel
}

/// A hand-driven end of a link to `swarm` in `room` that has proven nothing.
async fn unproven(rtc: &SharedMemoryRtc, swarm: &Swarm, room: &Arc<Room>) -> Channel {
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(swarm
        .inner
        .attach(room, false, [1; 16], test_binding(), None, channel)
        .is_some());
    offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"))
}

/// The name the client at the other end of `channel` goes by in the room.
async fn hello(channel: &mut Channel) -> [u8; 16] {
    loop {
        if let Message::Hello { key } = next_message(channel).await {
            return name_of(&key);
        }
    }
}

#[tokio::test]
async fn a_client_goes_by_another_name_in_each_room() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    const OTHER: RoomKey = [8; 32];
    let other = Arc::new(Room::new(OTHER, 1, SECRET).unwrap_or_else(|| panic!("a room")));
    alice.inner.rooms().push(Arc::clone(&other));
    let mut here = hand_driven(&rtc, &alice).await;
    let mut there = hand_driven_in(&rtc, &alice, &other).await;
    assert_ne!(hello(&mut here).await, hello(&mut there).await);
}

#[tokio::test]
async fn a_link_made_for_a_room_left_is_not_kept_when_it_is_joined_again() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let left = joined(&alice);
    alice.inner.rooms().retain(|room| !Arc::ptr_eq(room, &left));
    alice.inner.leave_room(&left);
    alice.inner.rooms().push(Arc::new(
        Room::new(ROOM, 1, SECRET).unwrap_or_else(|| panic!("a room")),
    ));
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, _) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    // The answer the old membership was making arrives after the new one began.
    assert!(alice
        .inner
        .attach(&left, false, [1; 16], test_binding(), None, channel)
        .is_none());
    drop(offer);
    // The new membership's links are its own.
    let _peer = hand_driven(&rtc, &alice).await;
    alice.inner.leave_room(&left);
    assert_eq!(alice.inner.links().len(), 1);
    assert!(alice.inner.links().iter().all(|link| !link.is_closed()));
}

#[test]
fn every_membership_is_newer_than_the_last() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut last = 0;
    // Across swarms too: one counter for the process.
    let bob = client(&rtc);
    for round in 0..1000 {
        let swarm = if round % 2 == 0 { &alice } else { &bob };
        let next = swarm.inner.next_membership();
        assert!(next > last);
        last = next;
    }
}

#[tokio::test]
async fn an_ask_cancelled_and_made_again_sends_only_the_new_record() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut asker = hand_driven(&rtc, &alice).await;
    let (first, first_content) = record(1, 5 * wire::FRAME_BYTES);
    let (second, second_content) = record(2, 3 * wire::FRAME_BYTES + 10);
    knows(&[&alice], &[first, second]);
    alice.seed(first, first_content);
    alice.seed(second, second_content.clone());
    // Asked, cancelled and asked again under the same id, all at once.
    send(
        &asker,
        Message::Get {
            id: 7,
            address: first,
        },
    )
    .await;
    send(&asker, Message::Cancel { id: 7 }).await;
    send(
        &asker,
        Message::Get {
            id: 7,
            address: second,
        },
    )
    .await;
    let mut assembly = None;
    let whole = loop {
        if let Message::Piece {
            id,
            total,
            offset,
            data,
        } = next_message(&mut asker).await
        {
            assert_eq!(id, 7);
            assert_eq!(
                total as usize,
                second_content.len(),
                "a piece of the cancelled record"
            );
            let assembly = assembly.get_or_insert_with(|| {
                wire::Assembly::new(total).unwrap_or_else(|| panic!("size"))
            });
            if let Some(whole) = assembly
                .add(total, offset, &data)
                .unwrap_or_else(|e| panic!("{e}"))
            {
                break whole;
            }
        }
    };
    assert_eq!(whole, second_content);
    eventually("the link's transfers are given back", || {
        alice
            .inner
            .links()
            .iter()
            .all(|link| link.transfers_running() == 0)
    })
    .await;
}

#[tokio::test]
async fn a_peer_that_cannot_prove_the_room_learns_nothing_and_is_dropped() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, content) = record(1, 1000);
    knows(&[&alice], &[address]);
    alice.seed(address, content);
    // It knows the room's key, as its rendezvous nodes do, but not the file's
    // map, so not the secret.
    let mut peer = unproven(&rtc, &alice, &joined(&alice)).await;
    send(&peer, hello_from(3)).await;
    send(&peer, offerer_proof(&[6; 32])).await;
    send(&peer, Message::Get { id: 1, address }).await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), peer.events.next())
            .await
            .unwrap_or_else(|_| panic!("the link was not closed"));
        match event {
            Some(ChannelEvent::Message(bytes)) => {
                let message = Message::decode(&bytes).unwrap_or_else(|e| panic!("{e}"));
                assert!(
                    matches!(message, Message::Hello { .. } | Message::Proof { .. }),
                    "told {message:?}"
                );
            }
            Some(ChannelEvent::Open) => {}
            Some(ChannelEvent::Closed) | None => break,
        }
    }
    assert_eq!(alice.stats().records_served, 0);
}

#[tokio::test]
async fn a_peer_that_asks_before_proving_is_dropped() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, content) = record(1, 1000);
    knows(&[&alice], &[address]);
    alice.seed(address, content);
    let mut peer = unproven(&rtc, &alice, &joined(&alice)).await;
    send(&peer, Message::Get { id: 1, address }).await;
    send(&peer, offerer_proof(&SECRET)).await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), peer.events.next())
            .await
            .unwrap_or_else(|_| panic!("the link was not closed"));
        match event {
            Some(ChannelEvent::Message(bytes)) => {
                let message = Message::decode(&bytes).unwrap_or_else(|e| panic!("{e}"));
                assert!(
                    matches!(message, Message::Hello { .. } | Message::Proof { .. }),
                    "told {message:?}"
                );
            }
            Some(ChannelEvent::Open) => {}
            Some(ChannelEvent::Closed) | None => break,
        }
    }
}

#[test]
fn a_link_s_binding_follows_both_fingerprints() {
    let offer = "v=0\r\na=fingerprint:sha-256 AA:BB\r\n";
    let answer = "v=0\r\na=fingerprint:sha-256 CC:DD\r\n";
    let other = "v=0\r\na=fingerprint:sha-256 EE:FF\r\n";
    assert_eq!(link::binding(offer, answer), link::binding(offer, answer));
    // A node in the middle, with an answer or an offer of its own.
    assert_ne!(link::binding(offer, answer), link::binding(offer, other));
    assert_ne!(link::binding(offer, answer), link::binding(other, answer));
    // Each end names its own fingerprint.
    assert_eq!(
        link::Binding::of(offer, answer, true).fingerprint,
        "sha-256 AA:BB"
    );
    assert_eq!(
        link::Binding::of(offer, answer, false).fingerprint,
        "sha-256 CC:DD"
    );
    // Each end proves its own role.
    let binding = link::binding(offer, answer);
    assert_ne!(
        link::proof(&SECRET, &binding, &[1; 16], true),
        link::proof(&SECRET, &binding, &[1; 16], false)
    );
}

#[tokio::test]
async fn a_proof_without_a_name_does_not_open_the_link() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let mut peer = unproven(&rtc, &alice, &joined(&alice)).await;
    send(&peer, offerer_proof(&SECRET)).await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), peer.events.next())
            .await
            .unwrap_or_else(|_| panic!("the link was not closed"));
        match event {
            Some(ChannelEvent::Message(bytes)) => {
                let message = Message::decode(&bytes).unwrap_or_else(|e| panic!("{e}"));
                assert!(
                    matches!(message, Message::Hello { .. } | Message::Proof { .. }),
                    "told {message:?}"
                );
            }
            Some(ChannelEvent::Open) => {}
            Some(ChannelEvent::Closed) | None => break,
        }
    }
}

#[tokio::test]
async fn a_member_gets_one_link_however_often_it_connects() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    let member = Some(([4; 32], [5; 16]));
    let channel = |rtc: SharedMemoryRtc| async move {
        let (offer, sdp) = rtc
            .offer(TEST_LABEL.into(), Vec::new())
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        let (channel, _) = rtc
            .answer(TEST_LABEL.into(), Vec::new(), sdp)
            .await
            .unwrap_or_else(|e| panic!("{e}"));
        (offer, channel)
    };
    let (_first_offer, first) = channel(rtc.clone()).await;
    assert!(alice
        .inner
        .attach(&room, false, [1; 16], test_binding(), member, first)
        .is_some());
    let (_second_offer, second) = channel(rtc.clone()).await;
    assert!(alice
        .inner
        .attach(&room, false, [2; 16], test_binding(), member, second)
        .is_none());
    // Another member is welcome.
    let (_third_offer, third) = channel(rtc.clone()).await;
    assert!(alice
        .inner
        .attach(
            &room,
            false,
            [3; 16],
            test_binding(),
            Some(([4; 32], [6; 16])),
            third
        )
        .is_some());
}

#[tokio::test]
async fn two_clients_offering_through_one_node_at_once_keep_one_link() {
    let rtc = SharedMemoryRtc::default();
    let (alice, bob) = (client(&rtc), client(&rtc));
    // Through the same node, each names the other as the same member both
    // ways.
    let (alice_as, bob_as) = (Some(([4; 32], [1; 16])), Some(([4; 32], [2; 16])));
    link_naming(&rtc, &alice, &bob, [3; 16], bob_as, alice_as).await;
    link_naming(&rtc, &bob, &alice, [4; 16], alice_as, bob_as).await;
    eventually("one open link each", || {
        alice.stats().links == 1 && bob.stats().links == 1
    })
    .await;
    let kept = |swarm: &Swarm| {
        swarm
            .inner
            .links()
            .iter()
            .find(|link| link.is_open())
            .map(|link| link.offer_id)
    };
    assert_eq!(kept(&alice), kept(&bob));
}

#[tokio::test]
async fn a_member_linked_again_under_another_name_keeps_its_first_link() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    let member = Some(([4; 32], [5; 16]));
    // First link: alice answered the member's offer.
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    assert!(alice
        .inner
        .attach(&room, false, [1; 16], test_binding(), member, channel)
        .is_some());
    let first = offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"));
    send(&first, hello_from(3)).await;
    send(&first, offerer_proof(&SECRET)).await;
    eventually("the first link opens", || alice.stats().links == 1).await;
    // Second link the other way, the same member under a new name.
    let (offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (mut second, answer) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let alice_end = offer.accept(answer).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(alice
        .inner
        .attach(&room, true, [2; 16], test_binding(), member, alice_end)
        .is_some());
    send(&second, hello_from(8)).await;
    send(&second, proof_from(8, &SECRET, &[2; 16], false)).await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(5), second.events.next())
            .await
            .unwrap_or_else(|_| panic!("the second link was not closed"));
        if matches!(event, Some(ChannelEvent::Closed) | None) {
            break;
        }
    }
    eventually("one link left", || alice.inner.links().len() == 1).await;
    drop(first);
}

#[tokio::test]
async fn each_rendezvous_node_is_counted_alone() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let room = joined(&alice);
    let (node_a, node_s) = ([10; 32], [11; 32]);
    // One link, to a member of node A.
    let (_offer, sdp) = rtc
        .offer(TEST_LABEL.into(), Vec::new())
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let (channel, _) = rtc
        .answer(TEST_LABEL.into(), Vec::new(), sdp)
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    let link = alice
        .inner
        .attach(
            &room,
            false,
            [1; 16],
            test_binding(),
            Some((node_a, [1; 16])),
            channel,
        )
        .unwrap_or_else(|| panic!("attached"));
    // Node A has two members, this client and that one: nothing more wanted
    // there. Node S has another member this client has no link to.
    assert!(alice.inner.reserve_connection(&room, &node_a, 2).is_none());
    let connecting = alice.inner.reserve_connection(&room, &node_s, 2);
    assert!(connecting.is_some());
    // While that connection is being made, it counts at every node.
    assert!(alice.inner.reserve_connection(&room, &node_s, 2).is_none());
    assert!(alice
        .inner
        .reserve_connection(&room, &[12; 32], 2)
        .is_none());
    drop(connecting);
    // Once the same peer turns out to be that member of S, S is counted too.
    link.also_known_as(Some((node_s, [2; 16])));
    assert!(alice.inner.reserve_connection(&room, &node_s, 2).is_none());
}

#[tokio::test]
async fn a_read_moves_on_from_links_settled_away_as_duplicates() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, content) = record(1, 3 * wire::FRAME_BYTES);
    knows(&[&alice], &[address]);
    let room = joined(&alice);
    // A proven hand-driven link, by its own name, holding the record.
    let open_one = |nonce: u8| {
        let rtc = rtc.clone();
        let alice = alice.clone();
        let room = Arc::clone(&room);
        async move {
            let peer = unproven(&rtc, &alice, &room).await;
            let link = alice
                .inner
                .links()
                .last()
                .cloned()
                .unwrap_or_else(|| panic!("attached"));
            send(&peer, hello_from(nonce)).await;
            send(&peer, proof_from(nonce, &SECRET, &[1; 16], true)).await;
            send(
                &peer,
                Message::Have {
                    addresses: vec![address],
                },
            )
            .await;
            eventually("the link offers the record", || link.offers(&address)).await;
            (peer, link)
        }
    };
    let (mut first, first_link) = open_one(3).await;
    let reader = {
        let alice = alice.clone();
        tokio::spawn(async move { alice.fetch(&address).await })
    };
    // Each asked link is settled away before it answers, the next one opened
    // meanwhile: more than the peers a read tries, and the read goes on.
    let _ = asked_for(&mut first).await;
    let (mut second, second_link) = open_one(4).await;
    first_link.settle_away(&Arc::downgrade(&alice.inner));
    let _ = asked_for(&mut second).await;
    let (mut third, _) = open_one(5).await;
    second_link.settle_away(&Arc::downgrade(&alice.inner));
    let id = asked_for(&mut third).await;
    for piece in wire::pieces(id, &content) {
        send(&third, piece).await;
    }
    let read = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .unwrap_or_else(|_| panic!("the read did not end"))
        .unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(read, Some(content));
}

/// The id of the next record the hand-driven end is asked for.
async fn asked_for(peer: &mut Channel) -> u32 {
    loop {
        if let Message::Get { id, .. } = next_message(peer).await {
            return id;
        }
    }
}

#[test]
fn a_peer_on_pace_waits_about_as_long_as_the_nodes_take_now() {
    let rtc = SharedMemoryRtc::default();
    let swarm = client(&rtc);
    let floor = Duration::from_secs(6);
    // Before any read went to the nodes: the floor.
    assert_eq!(swarm.patience(floor), floor);
    // Busy nodes: about their time, which a running average follows.
    for _ in 0..20 {
        swarm.note_node_read(Duration::from_secs(10));
    }
    let busy = swarm.patience(floor);
    assert!(
        busy > Duration::from_millis(9_900) && busy <= Duration::from_secs(10),
        "{busy:?}"
    );
    // Never past the time a peer has to send a record.
    for _ in 0..20 {
        swarm.note_node_read(Duration::from_secs(60));
    }
    assert_eq!(swarm.patience(floor), link::FETCH_TOTAL);
    // Quick nodes again: back to the floor.
    for _ in 0..40 {
        swarm.note_node_read(Duration::from_millis(500));
    }
    assert_eq!(swarm.patience(floor), floor);
}

#[tokio::test(start_paused = true)]
async fn a_room_whose_read_still_runs_never_makes_way() {
    let rooms: Vec<Arc<Room>> = (1..=MAX_ROOMS)
        .map(|key| {
            let key = u8::try_from(key).unwrap_or_else(|_| panic!("a room key"));
            Arc::new(Room::new([key; 32], 1, SECRET).unwrap_or_else(|| panic!("a room")))
        })
        .collect();
    // The first room's read waits at busy nodes far past the turnover; the
    // others have gone unread as long.
    let first = rooms.first().cloned().unwrap_or_else(|| panic!("a room"));
    first.begin_read();
    tokio::time::advance(ROOM_TURNOVER * 3).await;
    let now = crate::runtime::Clock::now();
    assert_eq!(first.unread_for(now), Duration::ZERO);
    assert_ne!(room_to_leave(&rooms, now), Some(0));
    // Once it ends, the room goes unread from then.
    first.end_read(now);
    for room in rooms.iter().skip(1) {
        room.note_read(now);
    }
    tokio::time::advance(ROOM_TURNOVER - Duration::from_millis(1)).await;
    assert_eq!(room_to_leave(&rooms, crate::runtime::Clock::now()), None);
    tokio::time::advance(Duration::from_millis(1)).await;
    let now = crate::runtime::Clock::now();
    assert_eq!(first.unread_for(now), ROOM_TURNOVER);
    assert!(room_to_leave(&rooms, now).is_some());
}

#[tokio::test]
async fn a_read_counts_its_rooms_as_read_while_it_runs() {
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, _) = record(4, 100);
    knows(&[&alice], &[address]);
    let room = alice
        .inner
        .rooms()
        .first()
        .cloned()
        .unwrap_or_else(|| panic!("a room"));
    let reading = alice.reading(&address);
    assert_eq!(
        room.unread_for(crate::runtime::Clock::now() + ROOM_TURNOVER * 2),
        Duration::ZERO
    );
    drop(reading);
    assert!(room.unread_for(crate::runtime::Clock::now() + ROOM_TURNOVER * 2) >= ROOM_TURNOVER);
}

#[tokio::test]
async fn a_room_joined_while_its_file_is_read_counts_that_read() {
    const LATER: RoomKey = [42; 32];
    let rtc = SharedMemoryRtc::default();
    let alice = client(&rtc);
    let (address, _) = record(4, 100);
    // A read of a record of a file whose room is not joined yet (it waited
    // for a room to make way, say).
    let reading = alice.reading(&address);
    let room = Room::new(LATER, 1, [6; 32]).unwrap_or_else(|| panic!("a room"));
    // Admitted as a join admits it.
    let _ = alice.inner.admit(
        &mut alice.inner.rooms(),
        room,
        [address].into_iter().collect(),
    );
    let room = alice
        .inner
        .room(&LATER)
        .unwrap_or_else(|| panic!("the room"));
    let later = crate::runtime::Clock::now() + ROOM_TURNOVER * 2;
    assert_eq!(room.unread_for(later), Duration::ZERO);
    // The read's end counts for it as for a room joined before it.
    drop(reading);
    assert!(room.unread_for(crate::runtime::Clock::now() + ROOM_TURNOVER * 2) >= ROOM_TURNOVER);
}

#[tokio::test(start_paused = true)]
async fn a_join_past_the_room_limit_leaves_only_a_room_gone_unread() {
    let rooms: Vec<Arc<Room>> = (1..=MAX_ROOMS)
        .map(|key| {
            let key = u8::try_from(key).unwrap_or_else(|_| panic!("a room key"));
            Arc::new(Room::new([key; 32], 1, SECRET).unwrap_or_else(|| panic!("a room")))
        })
        .collect();
    // Every room read within the turnover: none makes way, and the join waits.
    tokio::time::advance(ROOM_TURNOVER - Duration::from_millis(1)).await;
    assert_eq!(room_to_leave(&rooms, crate::runtime::Clock::now()), None);
    // Past it, with all but the third read again lately: the third makes way,
    // never a room still read.
    tokio::time::advance(Duration::from_millis(1)).await;
    let now = crate::runtime::Clock::now();
    for (position, room) in rooms.iter().enumerate() {
        if position != 2 {
            room.note_read(now);
        }
    }
    assert_eq!(room_to_leave(&rooms, now), Some(2));
    // Of rooms all gone unread, the one unread longest.
    tokio::time::advance(ROOM_TURNOVER).await;
    assert_eq!(room_to_leave(&rooms, crate::runtime::Clock::now()), Some(2));
}

#[tokio::test]
async fn a_read_out_of_time_does_not_mark_its_peer_slow() {
    let rtc = SharedMemoryRtc::default();
    let bob = client(&rtc);
    let peer = hand_driven(&rtc, &bob).await;
    let (address, _) = record(9, 3 * wire::FRAME_BYTES);
    knows(&[&bob], &[address]);
    send(
        &peer,
        Message::Have {
            addresses: vec![address],
        },
    )
    .await;
    eventually("bob learns the claim", || {
        bob.inner.links().iter().any(|link| link.has(&address))
    })
    .await;
    let link = bob.inner.links().first().cloned();
    let link = link.unwrap_or_else(|| panic!("no link"));
    // The read has half a second left, and the peer sends nothing in it.
    let outcome = link
        .get(
            address,
            Instant::now() + Duration::from_millis(500),
            &Pace::default(),
        )
        .await;
    assert!(matches!(outcome, GetOutcome::Failed));
    // The read's time ran out, not the peer's: the next read asks it again.
    assert!(link.offers(&address));
    drop(peer);
}

#[tokio::test(start_paused = true)]
async fn a_room_asks_the_nodes_nothing_before_its_first_record() {
    let longest = Duration::from_secs(30);
    // Nothing read: the room starts once the longest wait is over.
    let unread = Room::new(ROOM, 1, SECRET).unwrap_or_else(|| panic!("a room"));
    let started = tokio::time::Instant::now();
    unread.until_started(longest).await;
    assert_eq!(started.elapsed(), longest);
    // A read's first record in after 2 s: the room starts then, and from
    // then on at once.
    let read = Room::new([8; 32], 1, SECRET).unwrap_or_else(|| panic!("a room"));
    let started = tokio::time::Instant::now();
    let record_in = async {
        tokio::time::sleep(Duration::from_secs(2)).await;
        read.start_finding();
    };
    futures::join!(read.until_started(longest), record_in);
    assert_eq!(started.elapsed(), Duration::from_secs(2));
    let again = tokio::time::Instant::now();
    read.until_started(longest).await;
    assert_eq!(again.elapsed(), Duration::ZERO);
}
