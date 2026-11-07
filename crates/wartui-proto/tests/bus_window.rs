//! The window toward a node: the P4's `Downlink` and the node's `FlightTaker`, driven alone and
//! together through transfers sealed and verified as on the bus.

use std::collections::VecDeque;

use wartui_proto::bus::{
    ACK_TIMEOUT_MS, AckTracker, CRC_OFFSET, Downlink, FlightTag, FlightTaker, Header,
    RESEND_GAP_MS, SEND_TRIES, TRANSFER_LEN, Take, records, seal, verify,
};
use wartui_proto::link::MAX_NODE_DOWN;

const QUEUE: usize = 8192;
/// The node firmware's inbox: `RECEIVED_MAX`.
const INBOX: usize = 2;
/// The P4 firmware's `ACK_POLL`: how often it clocks a slot whose flights wait, with nothing due.
const ACK_POLL_MS: u64 = 25;

/// A header from the P4 carrying flight `number`.
fn flight(number: u8, sync: bool) -> Header {
    Header::new(1, None).with_flight(FlightTag { number, sync })
}

/// Take the flight `header` carries, and commit it when it is queued, as a node whose inbox
/// accepts it does.
fn take(taker: &mut FlightTaker, header: &Header, room: bool) -> Take {
    let take = taker.take(header, room);
    if take == Take::Queue {
        taker.commit(header);
    }
    take
}

/// A taker past its first flight, which carried SYNC and was numbered `number`.
fn synced(number: u8) -> FlightTaker {
    let mut taker = FlightTaker::new();
    assert_eq!(take(&mut taker, &flight(number, true), true), Take::Queue);
    taker
}

#[test]
fn flight_taker_drops_flight_without_sync_when_nothing_taken_since_boot() {
    let mut taker = FlightTaker::new();
    // It may be the second of two whose first this node never saw.
    assert_eq!(take(&mut taker, &flight(3, false), true), Take::OutOfOrder);
    assert_eq!(take(&mut taker, &flight(0, false), true), Take::OutOfOrder);
    assert_eq!(take(&mut taker, &flight(3, true), true), Take::Queue);
    assert_eq!(take(&mut taker, &flight(0, false), true), Take::Queue, "3 then 0: numbers wrap");
}

#[test]
fn flight_taker_repeats_when_number_is_last_taken() {
    let mut taker = synced(1);
    // A repeat needs no room: nothing is queued.
    assert_eq!(take(&mut taker, &flight(1, false), false), Take::Repeat);
    assert_eq!(take(&mut taker, &flight(2, false), true), Take::Queue, "the repeat moved nothing");
}

#[test]
fn flight_taker_drops_out_of_order_when_number_skips_ahead() {
    let mut taker = synced(1);
    assert_eq!(take(&mut taker, &flight(3, false), true), Take::OutOfOrder);
    assert_eq!(take(&mut taker, &flight(0, false), true), Take::OutOfOrder, "two behind");
    assert_eq!(take(&mut taker, &flight(2, false), true), Take::Queue);
}

#[test]
fn flight_taker_holds_number_when_inbox_full() {
    let mut taker = synced(1);
    assert_eq!(take(&mut taker, &flight(2, false), false), Take::Full);
    assert_eq!(take(&mut taker, &flight(3, false), true), Take::OutOfOrder, "2 was not taken");
    assert_eq!(take(&mut taker, &flight(2, false), true), Take::Queue);
}

#[test]
fn flight_taker_queues_resend_when_flight_was_not_committed() {
    let taker = synced(1);
    // The inbox refused flight 2 after the taker said to queue it, so it was never committed.
    assert_eq!(taker.take(&flight(2, false), true), Take::Queue);
    assert_eq!(taker.take(&flight(2, false), true), Take::Queue, "not a repeat");
}

#[test]
fn flight_taker_adopts_number_when_sync_set() {
    let mut taker = synced(1);
    assert_eq!(take(&mut taker, &flight(1, true), true), Take::Queue, "SYNC outranks a repeat");
    assert_eq!(take(&mut taker, &flight(0, true), true), Take::Queue);
    assert_eq!(take(&mut taker, &flight(1, false), true), Take::Queue);
    assert_eq!(take(&mut taker, &flight(3, true), false), Take::Full);
    assert_eq!(
        take(&mut taker, &flight(2, false), true),
        Take::Queue,
        "a full drop adopts nothing"
    );
}

/// A full-sized body naming record `id`, so each record is a flight of its own, as a chunk is.
fn body(id: u16) -> Vec<u8> {
    let mut body = vec![0; MAX_NODE_DOWN];
    body[..2].copy_from_slice(&id.to_le_bytes());
    body
}

/// A flight's tag, as a transfer that carries it reads.
const fn tag(number: u8, sync: bool) -> Option<FlightTag> {
    Some(FlightTag { number, sync })
}

/// The tag of a transfer the P4 sealed, when it carries a flight.
fn tag_of(buf: &[u8; TRANSFER_LEN]) -> Option<FlightTag> {
    let transfer = verify(buf).expect("the P4 seals good transfers");
    transfer.carries_records().then_some(transfer.header.flight())
}

#[test]
fn downlink_sends_second_flight_when_first_awaits_ack() {
    let mut down = Downlink::<QUEUE>::new();
    let mut buf = [0u8; TRANSFER_LEN];
    down.push(0, 4, &body(0)).unwrap();
    down.fill(&mut buf, Header::default());
    down.sent(0, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(0)), 1).unwrap(), [0]);
    for id in 1..=3 {
        down.push(id, 4, &body(id)).unwrap();
    }
    assert!(down.fill(&mut buf, Header::default()) > 0);
    let first = tag_of(&buf).unwrap();
    down.sent(1, 0);
    assert!(down.has_work(), "the window has room for a second flight");
    assert!(down.fill(&mut buf, Header::default()) > 0);
    let second = tag_of(&buf).unwrap();
    down.sent(2, 0);
    assert_eq!((first.number, second.number), (1, 2));
    assert!(!(first.sync || second.sync));
    assert!(!down.has_work(), "two flights out fill the window");
    assert_eq!(down.fill(&mut buf, Header::default()), 0);
}

#[test]
fn downlink_tags_first_flight_sync_when_new_or_drained() {
    let mut down = Downlink::<QUEUE>::new();
    let mut buf = [0u8; TRANSFER_LEN];
    down.push(0, 4, &body(0)).unwrap();
    down.push(1, 4, &body(1)).unwrap();
    down.fill(&mut buf, Header::default());
    assert!(tag_of(&buf).unwrap().sync);
    down.sent(0, 0);
    assert!(!down.has_work(), "a SYNC flight goes alone");
    assert_eq!(down.on_header(&Header::new(0, Some(0)), 1).unwrap(), [0]);
    down.fill(&mut buf, Header::default());
    assert!(!tag_of(&buf).unwrap().sync);
    down.sent(1, 0);
    down.drain(|_| {});
    down.push(2, 4, &body(2)).unwrap();
    down.fill(&mut buf, Header::default());
    assert!(tag_of(&buf).unwrap().sync, "a slot drained is a slot whose node may be new");
}

/// What happens to one transfer on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wire {
    Clean,
    /// The node's half fails its CRC at the P4, which reads no header; the node read the P4's.
    NodeHalfBad,
    /// The P4's half fails its CRC at the node, which takes nothing and arms its next half.
    P4HalfBad,
}

/// The node's end, as `firmware/node/src/bus.rs` has it: each transfer is checked as it ends,
/// and the next half is armed with the ack straight after.
struct Node {
    taker: FlightTaker,
    tracker: AckTracker,
    seq: u16,
    /// The half it clocks out in the next transfer.
    armed: [u8; TRANSFER_LEN],
    /// The ids of each transfer queued, until the main loop takes them.
    inbox: VecDeque<Vec<u16>>,
    /// Every id the main loop took, in order.
    took: Vec<u16>,
    /// What the taker made of each record-carrying transfer.
    takes: Vec<Take>,
}

impl Node {
    fn boot() -> Self {
        let mut node = Self {
            taker: FlightTaker::new(),
            tracker: AckTracker::new(),
            seq: 0,
            armed: [0; TRANSFER_LEN],
            inbox: VecDeque::new(),
            took: Vec::new(),
            takes: Vec::new(),
        };
        node.arm();
        node
    }

    fn arm(&mut self) {
        seal(&mut self.armed, Header::new(self.seq, self.tracker.ack()), 0);
        self.seq = self.seq.wrapping_add(1);
    }

    /// The edge handler: check the transfer that ended, then arm the next half.
    fn receive(&mut self, buf: &[u8; TRANSFER_LEN]) {
        if let Ok(transfer) = verify(buf)
            && transfer.carries_records()
        {
            let take = self.taker.take(&transfer.header, self.inbox.len() < INBOX);
            match take {
                Take::Queue => {
                    let ids = transfer
                        .records()
                        .map(|record| u16::from_le_bytes([record.body[0], record.body[1]]))
                        .collect();
                    self.inbox.push_back(ids);
                    self.taker.commit(&transfer.header);
                    self.tracker.note_records(transfer.header.seq);
                }
                Take::Repeat => self.tracker.note_records(transfer.header.seq),
                Take::Full | Take::OutOfOrder => {}
            }
            self.takes.push(take);
        }
        self.arm();
    }

    /// The main loop takes everything in the inbox.
    fn drain(&mut self) {
        while let Some(ids) = self.inbox.pop_front() {
            self.took.extend(ids);
        }
    }
}

/// The P4's end of one slot, as `firmware/p4-bridge/src/bus.rs` drives its `Downlink`.
struct P4 {
    down: Downlink<QUEUE>,
    seq: u16,
    delivered: Vec<u16>,
    timed_out: Vec<u16>,
    /// The tag of each transfer's flight, or `None` for an idle one.
    carried: Vec<Option<FlightTag>>,
}

impl P4 {
    fn boot() -> Self {
        Self {
            down: Downlink::new(),
            seq: 0,
            delivered: Vec::new(),
            timed_out: Vec::new(),
            carried: Vec::new(),
        }
    }

    fn push(&mut self, ids: impl IntoIterator<Item = u16>) {
        for id in ids {
            self.down.push(id, 4, &body(id)).unwrap();
        }
    }

    /// One transfer with `node` at `now_ms`: the timeout poll and the fill, the exchange, then the
    /// node's header read before the P4's flight is marked sent.
    fn exchange(&mut self, node: &mut Node, wire: Wire, now_ms: u64) {
        if let Some(ids) = self.down.poll(now_ms) {
            self.timed_out.extend(ids);
        }
        let mut buf = [0u8; TRANSFER_LEN];
        let len = self.down.fill(&mut buf, Header::new(self.seq, None));
        self.carried.push(tag_of(&buf));
        let half = node.armed;
        if wire == Wire::P4HalfBad {
            buf[CRC_OFFSET] ^= 0xFF;
        }
        node.receive(&buf);
        if wire != Wire::NodeHalfBad
            && let Some(ids) = self.down.on_header(&verify(&half).unwrap().header, self.seq)
        {
            self.delivered.extend(ids);
        }
        if len > 0 {
            self.down.sent(self.seq, now_ms);
        }
        self.seq = self.seq.wrapping_add(1);
    }

    /// Clean transfers from `now_ms` until nothing is queued or out, draining the node after
    /// each. As the P4 schedules a slot: at once while records are due or a flight just went
    /// out, and otherwise at the window's deadline, no later than [`ACK_POLL_MS`].
    fn settle(&mut self, node: &mut Node, now_ms: u64) {
        let mut now = now_ms;
        for _ in 0..16 {
            if !self.down.has_work() && self.down.deadline_ms().is_none() {
                return;
            }
            self.exchange(node, Wire::Clean, now);
            node.drain();
            let carried = self.carried.last().copied().flatten().is_some();
            if !carried
                && !self.down.has_work()
                && let Some(deadline) = self.down.deadline_ms()
            {
                now = deadline.clamp(now, now + ACK_POLL_MS);
            }
        }
        panic!("the window never settled");
    }

    /// The numbers of the flights carried, idle transfers skipped.
    fn numbers(&self) -> Vec<u8> {
        self.carried.iter().flatten().map(|tag| tag.number).collect()
    }
}

/// A P4 and a node past the first flight, which carries SYNC and goes alone.
fn started() -> (P4, Node) {
    let mut p4 = P4::boot();
    let mut node = Node::boot();
    p4.push([100]);
    p4.settle(&mut node, 0);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([100].as_slice(), [100].as_slice())
    );
    p4.delivered.clear();
    node.took.clear();
    node.takes.clear();
    p4.carried.clear();
    (p4, node)
}

#[test]
fn window_carries_a_flight_in_each_transfer_when_records_stream() {
    let (mut p4, mut node) = started();
    p4.push(1..=4);
    for _ in 0..4 {
        p4.exchange(&mut node, Wire::Clean, 0);
        node.drain();
    }
    // The fifth transfer only fetches the last ack.
    p4.exchange(&mut node, Wire::Clean, 0);
    assert_eq!(p4.numbers(), [1, 2, 3, 0]);
    assert!(p4.carried[..4].iter().all(Option::is_some));
    assert_eq!(p4.delivered, [1, 2, 3, 4]);
    assert_eq!(node.takes, [Take::Queue; 4]);
}

#[test]
fn window_delivers_both_flights_when_one_ack_names_the_second() {
    let (mut p4, mut node) = started();
    p4.push([1, 2]);
    p4.exchange(&mut node, Wire::Clean, 0);
    // The fetch of flight 1's ack carries flight 2, and its header is lost.
    p4.exchange(&mut node, Wire::NodeHalfBad, 0);
    assert!(p4.delivered.is_empty());
    p4.exchange(&mut node, Wire::Clean, 0);
    assert_eq!(p4.delivered, [1, 2]);
    assert_eq!(p4.numbers(), [1, 2], "nothing went twice");
}

#[test]
fn window_resends_from_oldest_when_next_header_lacks_its_ack() {
    let (mut p4, mut node) = started();
    p4.push([1, 2]);
    p4.exchange(&mut node, Wire::P4HalfBad, 0);
    p4.settle(&mut node, 0);
    assert_eq!(p4.numbers(), [1, 2, 1, 2], "flight 2 went behind a flight the node missed");
    assert_eq!(node.takes, [Take::OutOfOrder, Take::Queue, Take::Queue]);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([1, 2].as_slice(), [1, 2].as_slice())
    );
}

#[test]
fn window_resends_both_in_order_when_inbox_full_on_first() {
    let (mut p4, mut node) = started();
    // Two flights the main loop has not taken yet fill the inbox.
    p4.push([1, 2]);
    p4.exchange(&mut node, Wire::Clean, 0);
    p4.exchange(&mut node, Wire::Clean, 0);
    assert_eq!(node.inbox.len(), INBOX);
    p4.push([3, 4]);
    for _ in 0..3 {
        p4.exchange(&mut node, Wire::Clean, 0);
    }
    assert_eq!(node.takes[2..4], [Take::Full, Take::OutOfOrder]);
    node.drain();
    p4.settle(&mut node, 0);
    assert_eq!(node.took, [1, 2, 3, 4], "each once, in order");
    assert_eq!(p4.delivered, [1, 2, 3, 4]);
}

#[test]
fn window_acks_resend_without_queuing_when_ack_lost() {
    let (mut p4, mut node) = started();
    p4.push([1]);
    p4.exchange(&mut node, Wire::Clean, 0);
    // Every header for a second is lost, so the P4 resends after its timeout.
    for now in [1, 25, 500, 999] {
        p4.exchange(&mut node, Wire::NodeHalfBad, now);
    }
    assert!(p4.delivered.is_empty());
    p4.exchange(&mut node, Wire::NodeHalfBad, ACK_TIMEOUT_MS);
    p4.settle(&mut node, ACK_TIMEOUT_MS);
    assert_eq!(p4.numbers(), [1, 1]);
    assert_eq!(node.takes, [Take::Queue, Take::Repeat]);
    assert_eq!((p4.delivered.as_slice(), node.took.as_slice()), ([1].as_slice(), [1].as_slice()));
}

#[test]
fn window_takes_sync_flight_when_p4_restarts_mid_stream() {
    let (mut p4, mut node) = started();
    p4.push([1, 2]);
    p4.settle(&mut node, 0);
    // The node expects flight 3; a restarted P4 numbers from 0.
    let mut p4 = P4::boot();
    p4.push([5]);
    p4.settle(&mut node, 0);
    assert_eq!(p4.carried[0], Some(FlightTag { number: 0, sync: true }));
    assert_eq!(*node.takes.last().unwrap(), Take::Queue);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([5].as_slice(), [1, 2, 5].as_slice())
    );
}

#[test]
fn window_resends_flight_with_sync_when_node_reboots() {
    let (mut p4, mut node) = started();
    p4.push([1]);
    p4.settle(&mut node, 0);
    // The node reboots without the slot going absent, so the P4's next flight lacks SYNC.
    let mut node = Node::boot();
    p4.push([2]);
    p4.settle(&mut node, 0);
    // The header of the transfer that carried flight 2 has no ack, so the next one resends it
    // with SYNC; the last fetches its ack.
    assert_eq!(p4.carried, [tag(1, false), None, tag(2, false), tag(2, true), None]);
    assert_eq!(node.takes, [Take::OutOfOrder, Take::Queue]);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([1, 2].as_slice(), [2].as_slice())
    );
}

#[test]
fn window_takes_both_flights_in_order_when_node_reboots_with_second_due_first() {
    let (mut p4, mut node) = started();
    p4.push([1, 2]);
    // Flight 1 goes out and the node takes nothing; it reboots before the next transfer, and the
    // slot never goes absent.
    p4.exchange(&mut node, Wire::P4HalfBad, 0);
    let mut node = Node::boot();
    p4.settle(&mut node, 0);
    // Flight 2 goes first, to a node that has taken nothing since boot. Its header, with no ack,
    // sends flight 1 again with SYNC, alone: flight 2 waits for its ack.
    assert_eq!(p4.carried, [tag(1, false), tag(2, false), tag(1, true), None, tag(2, false), None]);
    assert_eq!(node.takes, [Take::OutOfOrder, Take::Queue, Take::Queue]);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([1, 2].as_slice(), [1, 2].as_slice()),
        "each taken once, in order, and nothing delivered that was not taken"
    );
}

#[test]
fn downlink_resends_oldest_with_sync_when_header_has_no_ack() {
    let mut down = Downlink::<QUEUE>::new();
    let mut buf = [0u8; TRANSFER_LEN];
    down.push(0, 4, &body(0)).unwrap();
    down.fill(&mut buf, Header::default());
    down.sent(0, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(0)), 1).unwrap(), [0]);
    down.push(1, 4, &body(1)).unwrap();
    down.push(2, 4, &body(2)).unwrap();
    down.fill(&mut buf, Header::default());
    assert_eq!(tag_of(&buf), tag(1, false));
    down.sent(1, 0);
    down.fill(&mut buf, Header::default());
    assert_eq!(tag_of(&buf), tag(2, false));
    down.sent(2, 0);
    // A header with no ack: the node has taken nothing since boot.
    assert_eq!(down.on_header(&Header::new(0, None), 3), None);
    down.fill(&mut buf, Header::default());
    assert_eq!(tag_of(&buf), tag(1, true));
    down.sent(3, 0);
    assert!(!down.has_work(), "flight 2 waits behind a SYNC flight, which goes alone");
    assert_eq!(down.on_header(&Header::new(0, Some(3)), 4).unwrap(), [1]);
    down.fill(&mut buf, Header::default());
    assert_eq!(tag_of(&buf), tag(2, false));
}

#[test]
fn downlink_tags_next_flight_sync_when_header_has_no_ack_and_none_out() {
    let mut down = Downlink::<QUEUE>::new();
    let mut buf = [0u8; TRANSFER_LEN];
    down.push(0, 4, &body(0)).unwrap();
    down.fill(&mut buf, Header::default());
    down.sent(0, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(0)), 1).unwrap(), [0]);
    assert_eq!(down.on_header(&Header::new(0, None), 2), None);
    down.push(1, 4, &body(1)).unwrap();
    down.fill(&mut buf, Header::default());
    assert_eq!(tag_of(&buf), Some(FlightTag { number: 1, sync: true }));
}

#[test]
fn window_gives_up_then_syncs_next_flight_when_tries_time_out() {
    let (mut p4, mut node) = started();
    p4.push([1]);
    // The node never reads the P4's half, and its headers show it: each one sends the flight
    // again at once, which does not restart the timeout.
    let mut now = 0;
    while p4.timed_out.is_empty() {
        p4.exchange(&mut node, Wire::P4HalfBad, now);
        now += 100;
        assert!(now <= 10 * ACK_TIMEOUT_MS, "never gave up");
    }
    assert_eq!(now - 100, u64::from(SEND_TRIES) * ACK_TIMEOUT_MS);
    assert_eq!(p4.timed_out, [1]);
    assert!(p4.delivered.is_empty());
    p4.push([2]);
    p4.carried.clear();
    p4.settle(&mut node, now);
    assert_eq!(p4.carried[0], Some(FlightTag { number: 2, sync: true }));
    assert_eq!((p4.delivered.as_slice(), node.took.as_slice()), ([2].as_slice(), [2].as_slice()));
}

#[test]
fn window_takes_next_flight_when_idle_transfers_come_between() {
    let (mut p4, mut node) = started();
    p4.push([1]);
    p4.settle(&mut node, 0);
    for now in 1..=5 {
        p4.exchange(&mut node, Wire::Clean, now * 250);
    }
    p4.push([2]);
    p4.settle(&mut node, 2_000);
    assert_eq!(p4.numbers(), [1, 2]);
    assert_eq!(node.takes, [Take::Queue, Take::Queue]);
    assert_eq!(
        (p4.delivered.as_slice(), node.took.as_slice()),
        ([1, 2].as_slice(), [1, 2].as_slice())
    );
}

#[test]
fn downlink_seals_flight_records_under_header_when_filled() {
    let mut down = Downlink::<QUEUE>::new();
    down.push(9, 4, &body(9)).unwrap();
    let mut buf = [0u8; TRANSFER_LEN];
    let len = down.fill(&mut buf, Header::new(4, Some(2)));
    let transfer = verify(&buf).unwrap();
    assert_eq!(transfer.payload.len(), len);
    assert_eq!((transfer.header.seq, transfer.header.acked()), (4, Some(2)));
    let ids: Vec<u16> =
        records(transfer.payload).map(|r| u16::from_le_bytes([r.body[0], r.body[1]])).collect();
    assert_eq!(ids, [9]);
}

#[test]
fn window_paces_resends_when_headers_keep_lacking_ack() {
    let (mut p4, mut node) = started();
    p4.push([1]);
    // The node never reads the P4's half. A transfer each millisecond, well inside the timeout,
    // is far more than the P4 clocks a slot.
    let mut sends = Vec::new();
    for now in 0..200 {
        p4.exchange(&mut node, Wire::P4HalfBad, now);
        if p4.carried.last().copied().flatten().is_some() {
            sends.push(now);
        }
        if sends.len() >= 3 && p4.carried.last().copied().flatten().is_none() {
            assert!(!p4.down.has_work(), "a paced go-back holds the resend");
            assert!(p4.down.deadline_ms().is_some_and(|at| at > now));
        }
    }
    // The first go-back resends in the transfer after the one that fetched its header.
    assert_eq!(sends[..2], [0, 2]);
    assert!(sends.len() > 4, "resends keep coming: {sends:?}");
    for pair in sends[1..].windows(2) {
        assert!(pair[1] - pair[0] >= RESEND_GAP_MS, "{sends:?}");
    }
}

/// Fill the next transfer as `seq` and mark it sent at `now_ms` when it carries a flight.
fn send(down: &mut Downlink<QUEUE>, seq: &mut u16, now_ms: u64) -> Option<FlightTag> {
    let mut buf = [0u8; TRANSFER_LEN];
    let len = down.fill(&mut buf, Header::new(*seq, None));
    if len > 0 {
        down.sent(*seq, now_ms);
    }
    *seq += 1;
    tag_of(&buf)
}

#[test]
fn downlink_gives_next_flight_full_tries_when_it_becomes_oldest() {
    let mut down = Downlink::<QUEUE>::new();
    let mut seq = 0;
    // Past the first flight, which carries SYNC and goes alone.
    down.push(0, 4, &body(0)).unwrap();
    send(&mut down, &mut seq, 0);
    assert_eq!(down.on_header(&Header::new(0, Some(0)), seq).unwrap(), [0]);
    down.push(1, 4, &body(1)).unwrap();
    down.push(2, 4, &body(2)).unwrap();
    // Flights 1 and 2 go out, and time out twice.
    for now in [0, ACK_TIMEOUT_MS, 2 * ACK_TIMEOUT_MS] {
        assert_eq!(down.poll(now), None);
        assert_eq!(send(&mut down, &mut seq, now).map(|tag| tag.number), Some(1));
        assert_eq!(send(&mut down, &mut seq, now).map(|tag| tag.number), Some(2));
    }
    // The node takes flight 1's last send and drops flight 2's.
    let last_send = 2 * ACK_TIMEOUT_MS;
    let now = last_send + 10;
    assert_eq!(down.on_header(&Header::new(0, Some(seq - 2)), seq).unwrap(), [1]);
    assert_eq!(down.deadline_ms(), Some(last_send + ACK_TIMEOUT_MS), "timed from its last send");
    let mut timeouts = 0;
    let mut at = now;
    assert_eq!(send(&mut down, &mut seq, at).map(|tag| tag.number), Some(2));
    loop {
        at = down.deadline_ms().unwrap();
        if let Some(ids) = down.poll(at) {
            assert_eq!(ids, [2]);
            break;
        }
        timeouts += 1;
        assert!(send(&mut down, &mut seq, at).is_some());
    }
    assert_eq!(timeouts + 1, SEND_TRIES);
    assert_eq!(at, last_send + u64::from(SEND_TRIES) * ACK_TIMEOUT_MS);
}
