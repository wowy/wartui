//! The host's update session, driven on a clock the test invents against a scripted node
//! running the real receiver over a flash slot in memory. Then the same session against the
//! simulated fleet, end to end.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::engine::Now;
use wartui_core::ota::{
    BEGIN_TIMEOUT, CONFIRM_TIMEOUT, Failure, Ids, MAX_RESTARTS, MAX_SEND_FAILURES, OtaEvent,
    OtaImage, OtaSession, Outcome, Phase, REBOOT_GRACE,
};
use wartui_core::runtime::{drive_fleet, now};
use wartui_proto::bus::{
    Board, Capabilities, DieTemp, DownMsg, DownType, Heartbeat, NODE_VERSION, Slot, UPDATE_CHUNK,
    UPDATE_CHUNK_MAX, UpType, UpdateState, UpdateStatus, Vdd, Version,
};
use wartui_proto::link::{BridgeToHost, HostToBridge, UpdateOp, cache_chunk_body, image_tag};
use wartui_proto::mac::Mac;
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
use wartui_proto::radio_link::update::{OP_MAX, Op};
use wartui_proto::reset::ResetCause;

const NODE: Mac = [0x02, 0, 0, 0, 0, 7];
const OLD: Version = Version::new(0, 1, 0);
const NEW: Version = Version::new(0, 1, 1);
const BOARD: Board = Board::Esp32c5;
const SESSION: u8 = 0x5A;
/// The node-bus slot the node sits in.
const SLOT: Slot = slot(0, 3);
/// A slot the node is not in.
const OTHER_SLOT: Slot = slot(1, 0);
/// Five and a half sectors, so the last chunk is short and the last sector partial.
const IMAGE_LEN: usize = 5 * SECTOR + 1500;

const fn slot(bus: u8, index: u8) -> Slot {
    match Slot::new(bus, index) {
        Some(slot) => slot,
        None => panic!("not a slot"),
    }
}

fn image_bytes() -> Vec<u8> {
    (0..IMAGE_LEN).map(|i| u8::try_from(i * 7 % 251).unwrap_or(0)).collect()
}

fn image(version: Version) -> OtaImage {
    let bytes = image_bytes();
    let sha256 = Sha256::digest(&bytes).into();
    OtaImage { bytes: Arc::from(bytes), sha256, version, board: BOARD }
}

/// An update slot held in memory, which fails every operation once told to.
struct Flash {
    flash: Vec<u8>,
    fail: bool,
}

impl SectorSink for Flash {
    type Error = ();

    fn capacity(&self) -> u32 {
        0x1E_0000
    }

    fn erase_sector(&mut self, offset: u32) -> Result<(), ()> {
        if self.fail {
            return Err(());
        }
        let at = offset as usize;
        self.flash[at..at + SECTOR].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), ()> {
        if self.fail {
            return Err(());
        }
        let at = offset as usize;
        self.flash[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
}

/// A node as the firmware behaves: any update record it decodes confirms a trial image, it
/// answers every record that has a status, and it reboots into the image on `Verified`.
struct Node {
    receiver: Box<Receiver>,
    flash: Flash,
    version: Version,
    trial: bool,
    /// Never confirms a trial image.
    rejecting: bool,
    /// Reports `Verified` and stays on the old image.
    cannot_switch: bool,
}

impl Node {
    fn new() -> Self {
        Self {
            receiver: Box::new(Receiver::new(BOARD)),
            flash: Flash { flash: vec![0; 0x1E_0000], fail: false },
            version: OLD,
            trial: false,
            rejecting: false,
            cannot_switch: false,
        }
    }

    fn heartbeat(&self) -> Vec<u8> {
        heartbeat(self.version, self.trial)
    }

    /// Take a record from the host, returning the status the node sends back.
    fn hear(&mut self, kind: u8, body: &[u8]) -> Option<UpdateStatus> {
        let msg = DownMsg::decode(kind, body).ok()?;
        if !self.rejecting {
            self.trial = false;
        }
        let status = match msg {
            DownMsg::UpdateBegin(msg) if self.trial => UpdateStatus {
                session: msg.session,
                state: UpdateState::FlashError,
                next_offset: 0,
            },
            DownMsg::UpdateBegin(msg) => self.receiver.begin(&msg, &self.flash),
            DownMsg::UpdateChunk(msg) => self.receiver.chunk(&msg, &mut self.flash)?,
            DownMsg::UpdateCommit(msg) => self.receiver.commit(&msg, &mut self.flash),
            DownMsg::UpdateAbort(msg) => self.receiver.abort(&msg),
            _ => return None,
        };
        if status.state == UpdateState::Verified && !self.cannot_switch {
            self.reboot(self.receiver.version().unwrap_or(self.version), true);
        }
        Some(status)
    }

    fn reboot(&mut self, version: Version, trial: bool) {
        self.receiver.reset();
        self.version = version;
        self.trial = trial;
    }
}

fn heartbeat(version: Version, trial: bool) -> Vec<u8> {
    heartbeat_of(NODE, version, trial)
}

fn heartbeat_of(mac: Mac, version: Version, trial: bool) -> Vec<u8> {
    Heartbeat {
        capabilities: Capabilities { version, trial, ..Capabilities::here(BOARD) },
        mac,
        counter: 1,
        epoch: 0,
        wifi_refused: 0,
        ble_refused: 0,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
        heap_free: 0,
    }
    .encode()
    .to_vec()
}

/// A heartbeat from `slot`.
fn beat_from(slot: Slot, body: &[u8]) -> OtaEvent<'_> {
    OtaEvent::FromNode { slot, kind: UpType::Heartbeat.as_u8(), body }
}

/// An update status from `slot`.
fn status_from(slot: Slot, body: &[u8]) -> OtaEvent<'_> {
    OtaEvent::FromNode { slot, kind: UpType::UpdateStatus.as_u8(), body }
}

/// A record the session sent, for asserting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Begin,
    Chunk(u32),
    Commit,
    Abort,
    Confirm,
}

/// The record `cmd` carries to its node: a `ToNode`'s, or the chunk a `ToNodeChunk` names, built
/// from `image`, tagged `tag`, as the P4 builds it from its cache.
fn record(cmd: &HostToBridge, image: &[u8], tag: u32) -> (Slot, u16, u8, Vec<u8>) {
    match cmd {
        HostToBridge::ToNode { slot, id, kind, body } => (*slot, *id, *kind, body.to_vec()),
        HostToBridge::ToNodeChunk { slot, id, image: named, session, offset, len } => {
            assert_eq!(*named, tag, "the chunk names the session's image");
            let mut body = [0u8; UPDATE_CHUNK_MAX];
            let n = cache_chunk_body(image, *session, *offset, *len, &mut body)
                .expect("a chunk inside the image");
            (*slot, *id, DownType::UpdateChunk.as_u8(), body[..n].to_vec())
        }
        other => panic!("the session sent {other:?}"),
    }
}

fn summarize(kind: u8, body: &[u8]) -> Out {
    match DownMsg::decode(kind, body) {
        Ok(DownMsg::UpdateBegin(_)) => Out::Begin,
        Ok(DownMsg::UpdateChunk(msg)) => Out::Chunk(msg.offset),
        Ok(DownMsg::UpdateCommit(_)) => Out::Commit,
        Ok(DownMsg::UpdateAbort(_)) => Out::Abort,
        Ok(DownMsg::UpdateConfirm(_)) => Out::Confirm,
        other => panic!("the session sent {other:?}"),
    }
}

/// What becomes of one record on its way to the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Delivered and read; the node's answer comes back.
    Deliver,
    /// Delivered and never read: the node's receive queue overflowed.
    Lose,
    /// Undelivered, and not read.
    Undelivered,
    /// Delivered and read, and the node's answer is lost.
    Mute,
    /// Read, and reported undelivered: the node's acks are lost.
    Unheard,
}

fn deliver(_: Out) -> Fate {
    Fate::Deliver
}

/// The session, the node, and a clock.
struct Bench {
    session: OtaSession,
    /// The image the session sends, as the P4's cache holds it, and its tag.
    image: Arc<[u8]>,
    tag: u32,
    node: Node,
    /// The slot the node heartbeats from.
    slot: Slot,
    start: Instant,
    elapsed: Duration,
    next_beat: Duration,
    queue: VecDeque<HostToBridge>,
    sent: Vec<Out>,
}

impl Bench {
    fn new(image: &OtaImage) -> Self {
        Self::from(image, OLD)
    }

    /// A bench whose node runs `from`.
    fn from(image: &OtaImage, from: Version) -> Self {
        Self::with_ids(image, from, Ids { next: 1, step: 1 })
    }

    /// A bench whose node runs `from`, and whose session takes `ids`.
    fn with_ids(image: &OtaImage, from: Version, ids: Ids) -> Self {
        let start = Instant::now();
        let now = Now { mono: start, unix_ms: 0 };
        let mut node = Node::new();
        node.version = from;
        Self {
            session: OtaSession::new(NODE, from, image, SESSION, ids, now),
            image: Arc::clone(&image.bytes),
            tag: image_tag(&image.sha256),
            node,
            slot: SLOT,
            start,
            elapsed: Duration::ZERO,
            next_beat: Duration::from_millis(1000),
            queue: VecDeque::new(),
            sent: Vec::new(),
        }
    }

    fn now(&self) -> Now {
        Now { mono: self.start + self.elapsed, unix_ms: 0 }
    }

    fn feed(&mut self, event: OtaEvent<'_>) {
        let sends = self.session.handle(self.now(), event);
        self.queue.extend(sends);
    }

    fn beat(&mut self) {
        let frame = self.node.heartbeat();
        self.feed(beat_from(self.slot, &frame));
    }

    fn beat_as(&mut self, version: Version, trial: bool) {
        let frame = heartbeat(version, trial);
        self.feed(beat_from(self.slot, &frame));
    }

    fn status(&mut self, slot: Slot, status: UpdateStatus) {
        self.feed(status_from(slot, &status.encode()));
    }

    /// The node's status naming `next_offset`, from its slot.
    fn receiving(&mut self, next_offset: u32) {
        let status = UpdateStatus { session: SESSION, state: UpdateState::Receiving, next_offset };
        self.status(SLOT, status);
    }

    /// Open the session: a heartbeat, and the node's status at 0 answering the `begin`.
    fn open(&mut self) {
        self.beat();
        assert_eq!(self.queued(), [Out::Begin]);
        self.queue.clear();
        self.receiving(0);
    }

    /// What the session has queued and not yet sent, in order.
    fn queued(&self) -> Vec<Out> {
        self.queue
            .iter()
            .map(|cmd| {
                let (_, _, kind, body) = record(cmd, &self.image, self.tag);
                summarize(kind, &body)
            })
            .collect()
    }

    /// Answer the queued chunk at `offset`, `delivered` or not, without the node reading it.
    fn answer_chunk(&mut self, offset: u32, delivered: bool) {
        let at = self
            .queue
            .iter()
            .position(
                |cmd| matches!(cmd, HostToBridge::ToNodeChunk { offset: o, .. } if *o == offset),
            )
            .unwrap_or_else(|| panic!("chunk {offset} is queued: {:?}", self.queued()));
        let Some(HostToBridge::ToNodeChunk { id, .. }) = self.queue.remove(at) else {
            unreachable!("found above")
        };
        self.feed(OtaEvent::Delivery { id, delivered });
    }

    /// Carry every queued record to the node, the delivery result back before the node's
    /// answer, until nothing is left to send.
    fn settle(&mut self, fate: &mut impl FnMut(Out) -> Fate) {
        self.settle_until(fate, &|_| false);
    }

    /// [`Self::settle`], stopping early once `done`.
    fn settle_until(&mut self, fate: &mut impl FnMut(Out) -> Fate, done: &impl Fn(&Self) -> bool) {
        while !done(self)
            && let Some(cmd) = self.queue.pop_front()
        {
            let (slot, id, kind, body) = record(&cmd, &self.image, self.tag);
            assert_eq!(slot, self.slot, "sent to the slot the node heartbeats from");
            let out = summarize(kind, &body);
            self.sent.push(out);
            let fate = fate(out);
            let status = match fate {
                Fate::Deliver | Fate::Mute | Fate::Unheard => self.node.hear(kind, &body),
                Fate::Lose | Fate::Undelivered => None,
            };
            let delivered = !matches!(fate, Fate::Undelivered | Fate::Unheard);
            self.feed(OtaEvent::Delivery { id, delivered });
            if let Some(status) = status.filter(|_| fate != Fate::Mute) {
                self.status(self.slot, status);
            }
        }
    }

    /// Run in 50 ms ticks, the node heartbeating every second, until `done` or the session
    /// ends, or `limit` passes.
    fn run(
        &mut self,
        fate: &mut impl FnMut(Out) -> Fate,
        limit: Duration,
        done: impl Fn(&Self) -> bool,
    ) {
        let end = self.elapsed + limit;
        loop {
            self.settle_until(fate, &done);
            if done(self) || self.session.outcome().is_some() || self.elapsed >= end {
                return;
            }
            self.elapsed += Duration::from_millis(50);
            if self.elapsed >= self.next_beat {
                self.next_beat += Duration::from_millis(1000);
                self.beat();
            }
            self.feed(OtaEvent::Tick);
        }
    }

    fn run_to_end(&mut self, fate: &mut impl FnMut(Out) -> Fate) -> Outcome {
        self.run(fate, Duration::from_secs(300), |_| false);
        self.session.outcome().cloned().expect("the session ended")
    }

    fn count(&self, out: Out) -> usize {
        self.sent.iter().filter(|o| **o == out).count()
    }

    fn assert_image_written(&self) {
        assert_eq!(&self.node.flash.flash[..IMAGE_LEN], &image_bytes()[..]);
    }
}

#[test]
fn ota_session_updates_and_confirms_node_when_nothing_is_lost() {
    let mut bench = Bench::new(&image(NEW));
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    bench.assert_image_written();
    assert_eq!((bench.node.version, bench.node.trial), (NEW, false));
    // Every chunk once, in order, and one of each other frame.
    let chunks: Vec<u32> = bench
        .sent
        .iter()
        .filter_map(|o| if let Out::Chunk(at) = o { Some(*at) } else { None })
        .collect();
    let expected: Vec<u32> = (0..IMAGE_LEN as u32).step_by(1024).collect();
    assert_eq!(chunks, expected);
    assert_eq!(bench.count(Out::Begin), 1);
    assert_eq!(bench.count(Out::Commit), 1);
    assert_eq!(bench.count(Out::Confirm), 1);
    let progress = bench.session.progress();
    assert_eq!((progress.phase, progress.acked), (Phase::Done, IMAGE_LEN as u32));
}

#[test]
fn ota_session_sends_begin_only_when_node_heartbeats() {
    let mut bench = Bench::new(&image(NEW));
    // Nothing names the node's slot until its own heartbeat does.
    for _ in 0..20 {
        bench.elapsed += Duration::from_millis(50);
        bench.feed(OtaEvent::Tick);
    }
    let other = heartbeat_of([9; 6], OLD, false);
    bench.feed(beat_from(SLOT, &other));
    let idle = UpdateStatus { session: SESSION, state: UpdateState::Idle, next_offset: 0 };
    bench.status(SLOT, idle);
    assert!(bench.queue.is_empty(), "nothing before a heartbeat: {:?}", bench.queue);
    bench.beat();
    bench.settle(&mut deliver);
    assert_eq!(bench.sent.first(), Some(&Out::Begin));
}

#[test]
fn ota_session_sends_nothing_when_cancelled_before_any_heartbeat() {
    let mut bench = Bench::new(&image(NEW));
    let sends = bench.session.cancel(bench.now());
    assert!(sends.is_empty(), "no slot to send the abort to: {sends:?}");
    assert_eq!(bench.session.outcome(), Some(&Outcome::Failed(Failure::Cancelled)));
}

#[test]
fn ota_session_ignores_status_when_another_slot_sends_it() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 4096);
    let queued = bench.queue.len();
    // An `Idle` for this session from a slot the node is not in leaves the stream running.
    let idle = UpdateStatus { session: SESSION, state: UpdateState::Idle, next_offset: 0 };
    bench.status(OTHER_SLOT, idle);
    assert_eq!(bench.session.progress().phase, Phase::Streaming);
    assert_eq!(bench.queue.len(), queued, "nothing sent");
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Begin), 1);
}

#[test]
fn ota_session_sends_to_new_slot_when_node_heartbeats_from_another() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut |_| Fate::Lose, Duration::from_secs(3), |b| b.count(Out::Begin) == 2);
    // The node is moved to another slot; `settle` checks every record follows it there.
    bench.slot = OTHER_SLOT;
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    bench.assert_image_written();
}

#[test]
fn ota_session_retries_begin_on_each_heartbeat_when_unanswered() {
    let mut bench = Bench::new(&image(NEW));
    let mut begins = 0;
    let mut fate = |out| match out {
        Out::Begin => {
            begins += 1;
            if begins <= 2 { Fate::Lose } else { Fate::Deliver }
        }
        _ => Fate::Deliver,
    };
    bench.run(&mut fate, Duration::from_secs(10), |b| {
        b.session.progress().phase == Phase::Streaming
    });
    assert_eq!(bench.count(Out::Begin), 3, "one per heartbeat until answered");
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
}

#[test]
fn ota_session_fails_when_begin_goes_unanswered() {
    let mut bench = Bench::new(&image(NEW));
    let outcome = bench.run_to_end(&mut |_| Fate::Lose);
    assert_eq!(outcome, Outcome::Failed(Failure::NoAnswer));
    assert!(bench.elapsed >= BEGIN_TIMEOUT);
}

#[test]
fn ota_session_resends_chunk_when_send_is_undelivered() {
    let mut bench = Bench::new(&image(NEW));
    let mut failed = false;
    let mut fate = |out| {
        if out == Out::Chunk(2048) && !failed {
            failed = true;
            return Fate::Undelivered;
        }
        Fate::Deliver
    };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Chunk(2048)), 2);
    assert_eq!(bench.count(Out::Chunk(1024)), 1);
    bench.assert_image_written();
}

#[test]
fn ota_session_fails_when_chunks_stay_undelivered_across_every_begin() {
    let mut bench = Bench::new(&image(NEW));
    let mut fate = |out| if out == Out::Chunk(2048) { Fate::Undelivered } else { Fate::Deliver };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Failed(Failure::NotAnswering));
    // Each begin resumes at 2048 and sends it until the streak gives up on it.
    let per_begin = MAX_SEND_FAILURES as usize + 1;
    let begins = MAX_RESTARTS as usize + 1;
    assert_eq!(bench.count(Out::Chunk(2048)), per_begin * begins);
    // The node holds the session throughout, so the status a chunk still out draws after each
    // return to the begin answers as the begin would.
    assert!(bench.count(Out::Begin) <= begins);
    assert_eq!(bench.sent.last(), Some(&Out::Abort), "the node is told to leave update mode");
}

#[test]
fn ota_session_begins_again_and_restarts_when_node_reboots_mid_stream() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 8192);
    // A hard reset: the node acknowledges nothing while it boots, and comes back without the
    // session.
    bench.node.reboot(OLD, false);
    let mut fate = |_| Fate::Undelivered;
    bench.run(&mut fate, Duration::from_secs(1), |b| b.session.progress().phase == Phase::Begin);
    assert_eq!(bench.session.progress().phase, Phase::Begin, "back to the begin, not failed");
    assert!(bench.session.outcome().is_none());
    let begins = bench.count(Out::Begin);
    let zeros = bench.count(Out::Chunk(0));
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Begin), begins + 1, "on the next heartbeat");
    assert_eq!(bench.count(Out::Chunk(0)), zeros + 1, "streaming restarts from 0");
    bench.assert_image_written();
}

#[test]
fn ota_session_goes_back_to_reported_offset_when_sector_status_is_lost() {
    let mut bench = Bench::new(&image(NEW));
    let mut muted = false;
    // The chunk completing the second sector lands, and the node's status for it is lost.
    let mut fate = |out| {
        if out == Out::Chunk(7168) && !muted {
            muted = true;
            return Fate::Mute;
        }
        Fate::Deliver
    };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    // The host waited, then resent from the last offset the node reported; the node's
    // answer to that duplicate is the status that was lost. Nothing past the sector went
    // out before it.
    let at = |out| bench.sent.iter().position(|o| *o == out).expect("sent");
    let resent = bench.sent.iter().rposition(|o| *o == Out::Chunk(4096)).expect("sent");
    assert_ne!(resent, at(Out::Chunk(4096)), "chunk 4096 went out twice");
    assert!(resent < at(Out::Chunk(8192)));
    bench.assert_image_written();
}

#[test]
fn ota_session_goes_back_to_gap_when_node_reports_it() {
    let mut bench = Bench::new(&image(NEW));
    let mut lost = false;
    let mut fate = |out| {
        if out == Out::Chunk(1024) && !lost {
            lost = true;
            return Fate::Lose;
        }
        Fate::Deliver
    };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    // 2048 arrived to a node wanting 1024, which said so; the host went back.
    assert_eq!(bench.count(Out::Chunk(1024)), 2);
    bench.assert_image_written();
}

#[test]
fn ota_session_begins_again_and_resumes_when_node_reports_idle() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 8192);
    // The node rebooted and lost the session: its next chunk is answered `Idle`.
    bench.node.receiver.reset();
    let idle = UpdateStatus { session: SESSION, state: UpdateState::Idle, next_offset: 0 };
    bench.status(SLOT, idle);
    bench.queue.clear();
    assert_eq!(bench.session.progress().phase, Phase::Begin);
    let begins = bench.count(Out::Begin);
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Begin), begins + 1);
    bench.assert_image_written();
}

#[test]
fn ota_session_resumes_from_node_offset_when_begin_finds_progress() {
    let image = image(NEW);
    let mut first = Bench::new(&image);
    first.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 8192);
    // The host restarts with the same session byte; the node kept its progress.
    let mut bench = Bench::new(&image);
    bench.node = first.node;
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Chunk(0)), 0, "resumed from the node's offset");
    assert_eq!(bench.count(Out::Chunk(8192)), 1);
    bench.assert_image_written();
}

#[test]
fn ota_session_fails_when_node_reports_flash_error() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 4096);
    bench.node.flash.fail = true;
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Failed(Failure::FlashError));
}

#[test]
fn ota_session_fails_when_node_reports_hash_mismatch() {
    let mut wrong = image(NEW);
    wrong.sha256[0] ^= 1;
    let mut bench = Bench::new(&wrong);
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Failed(Failure::HashMismatch));
    assert_eq!(bench.node.version, OLD);
}

#[test]
fn ota_session_sends_abort_when_failing_after_node_accepted() {
    let mut wrong = image(NEW);
    wrong.sha256[0] ^= 1;
    let mut bench = Bench::new(&wrong);
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Failed(Failure::HashMismatch));
    // Once, so the node leaves update mode now rather than after 30 s of silence.
    assert_eq!(bench.sent.last(), Some(&Out::Abort));
    assert_eq!(bench.count(Out::Abort), 1);
    assert!(!bench.node.receiver.is_active());
}

#[test]
fn ota_session_sends_no_abort_when_begin_is_never_accepted() {
    let mut bench = Bench::new(&image(NEW));
    assert_eq!(bench.run_to_end(&mut |_| Fate::Lose), Outcome::Failed(Failure::NoAnswer));
    assert_eq!(bench.count(Out::Abort), 0);
}

#[test]
fn ota_session_confirms_instead_of_streaming_again_when_rebegun_after_install() {
    let mut bench = Bench::new(&image(NEW));
    let mut first = true;
    // `Verified` is lost; every resent commit then goes undelivered while the node reboots
    // into the new image, which returns the session to its `begin`.
    let mut fate = |out| match out {
        Out::Commit if first => {
            first = false;
            Fate::Mute
        }
        Out::Commit => Fate::Undelivered,
        _ => Fate::Deliver,
    };
    bench.run(&mut fate, Duration::from_secs(5), |b| {
        b.sent.contains(&Out::Commit) && b.session.progress().phase == Phase::Begin
    });
    assert_eq!(bench.session.progress().phase, Phase::Begin);
    assert_eq!((bench.node.version, bench.node.trial), (NEW, true));
    // The new image's first heartbeat is confirmed, never answered with a `begin`.
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Begin), 1, "no second begin");
    assert_eq!(bench.count(Out::Chunk(0)), 1, "no second stream");
    assert!(!bench.node.trial);
}

#[test]
fn ota_session_resends_confirm_when_its_delivery_is_lost_on_same_version() {
    // `--force` onto the version the node runs: only a delivered confirm says it is done.
    let mut bench = Bench::from(&image(NEW), NEW);
    let mut first = true;
    let mut fate = |out| {
        if out == Out::Confirm && first {
            first = false;
            return Fate::Unheard;
        }
        Fate::Deliver
    };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    assert!(!bench.node.trial);
    assert_eq!(bench.count(Out::Confirm), 2, "once more after the lost delivery");
}

#[test]
fn ota_session_confirms_trial_node_when_started_to_confirm() {
    let start = Instant::now();
    let now = |ms| Now { mono: start + Duration::from_millis(ms), unix_ms: 0 };
    let mut session = OtaSession::confirm_trial(NODE, NEW, Ids { next: 1, step: 1 }, now(0));
    let beat = heartbeat(NEW, true);
    let sends = session.handle(now(10), beat_from(SLOT, &beat));
    let [HostToBridge::ToNode { slot, id, kind, body }] = sends.as_slice() else {
        panic!("one confirm: {sends:?}")
    };
    assert_eq!((*slot, summarize(*kind, body)), (SLOT, Out::Confirm));
    session.handle(now(20), OtaEvent::Delivery { id: *id, delivered: true });
    let beat = heartbeat(NEW, false);
    session.handle(now(1000), beat_from(SLOT, &beat));
    assert_eq!(session.outcome(), Some(&Outcome::Done(NEW)));
}

#[test]
fn ota_session_ignores_stale_status_when_newer_one_arrived_first() {
    let mut bench = Bench::new(&image(NEW));
    // Stopped with chunk 7168 queued and the node's last status at 4096.
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.sent.contains(&Out::Chunk(6144)));
    // Two gap statuses, the newer first: the older is stale and must not rewind the host.
    let status =
        |next_offset| UpdateStatus { session: SESSION, state: UpdateState::Receiving, next_offset };
    bench.status(SLOT, status(6144));
    bench.status(SLOT, status(5120));
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Chunk(6144)), 2, "rewound to the newer gap");
    assert_eq!(bench.count(Out::Chunk(5120)), 1, "never to the stale one");
    bench.assert_image_written();
}

#[test]
fn ota_session_succeeds_when_verified_is_lost_and_new_image_heartbeats_on_trial() {
    let mut bench = Bench::new(&image(NEW));
    let mut fate = |out| if out == Out::Commit { Fate::Mute } else { Fate::Deliver };
    bench.run(&mut fate, Duration::from_secs(5), |b| b.node.version == NEW);
    // The node switched and rebooted; the host never heard `Verified`.
    assert_eq!((bench.node.version, bench.node.trial), (NEW, true));
    bench.queue.clear();
    bench.beat();
    assert_eq!(bench.session.progress().phase, Phase::AwaitConfirm);
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    assert!(!bench.node.trial);
}

#[test]
fn ota_session_succeeds_when_verified_is_lost_and_resent_commit_confirms_new_image() {
    let mut bench = Bench::new(&image(NEW));
    let mut muted = false;
    let mut fate = |out| {
        if out == Out::Commit && !muted {
            muted = true;
            return Fate::Mute;
        }
        Fate::Deliver
    };
    // The resent commit reaches the new image, which confirms on hearing the host and answers
    // that it holds no session; its next heartbeat is the new image, confirmed.
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    assert!(bench.count(Out::Commit) >= 2);
}

#[test]
fn ota_session_fails_when_node_rolls_back() {
    let mut bench = Bench::new(&image(NEW));
    bench.node.rejecting = true;
    bench.run(&mut deliver, Duration::from_secs(30), |b| {
        b.session.progress().phase == Phase::AwaitConfirm
    });
    assert_eq!(bench.session.progress().phase, Phase::AwaitConfirm);
    // Three unconfirmed boots later, the old image is back.
    bench.node.reboot(OLD, false);
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Failed(Failure::RolledBack(OLD)));
}

#[test]
fn ota_session_ignores_old_image_heartbeat_when_reboot_grace_has_not_passed() {
    let mut bench = Bench::new(&image(NEW));
    bench.node.cannot_switch = true;
    bench.run(&mut deliver, Duration::from_secs(30), |b| {
        b.session.progress().phase == Phase::AwaitReboot
    });
    bench.beat_as(OLD, false);
    assert_eq!(bench.session.progress().phase, Phase::AwaitReboot, "a beat from before the reboot");
    // The node never switched, and keeps heartbeating the old image.
    let outcome = bench.run_to_end(&mut deliver);
    assert_eq!(outcome, Outcome::Failed(Failure::StillOld(OLD)));
    assert!(bench.elapsed >= REBOOT_GRACE);
}

#[test]
fn ota_session_fails_when_new_image_is_never_confirmed() {
    let mut bench = Bench::new(&image(NEW));
    bench.node.rejecting = true;
    bench.run(&mut deliver, Duration::from_secs(30), |b| {
        b.session.progress().phase == Phase::AwaitConfirm
    });
    let start = bench.elapsed;
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Failed(Failure::Unconfirmed));
    assert!(bench.elapsed - start >= CONFIRM_TIMEOUT);
    assert!(bench.count(Out::Confirm) > 10, "a confirm on every trial heartbeat");
}

#[test]
fn ota_session_sends_abort_when_cancelled() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 4096);
    let sends = bench.session.cancel(bench.now());
    bench.queue.extend(sends);
    bench.settle(&mut deliver);
    assert_eq!(bench.sent.last(), Some(&Out::Abort));
    assert_eq!(bench.session.outcome(), Some(&Outcome::Failed(Failure::Cancelled)));
    assert!(!bench.node.receiver.is_active(), "the node dropped the session");
}

#[test]
fn ota_session_ignores_heartbeat_when_another_node_sends_it() {
    let mut bench = Bench::new(&image(NEW));
    let frame = heartbeat_of([9; 6], OLD, false);
    let sends = bench.session.handle(bench.now(), beat_from(OTHER_SLOT, &frame));
    assert!(sends.is_empty());
}

#[test]
fn ota_session_reads_heartbeat_prefix_when_node_runs_another_minor() {
    let mut bench = Bench::new(&image(NEW));
    // A longer heartbeat of some other minor's layout: only the prefix is read.
    let mut frame = heartbeat(Version::new(0, 0, 9), false);
    frame.extend_from_slice(&[0xAA; 9]);
    bench.feed(beat_from(SLOT, &frame));
    let queued: Vec<Out> = bench
        .queue
        .iter()
        .map(|cmd| match cmd {
            HostToBridge::ToNode { kind, body, .. } => summarize(*kind, body),
            other => panic!("the session sent {other:?}"),
        })
        .collect();
    assert_eq!(queued, [Out::Begin]);
}

#[test]
fn ota_session_sends_two_chunks_before_any_delivery() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    assert_eq!(bench.queued(), [Out::Chunk(0), Out::Chunk(1024)]);
}

#[test]
fn ota_session_sends_next_chunk_on_each_delivery() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    bench.answer_chunk(0, true);
    assert_eq!(bench.queued(), [Out::Chunk(1024), Out::Chunk(2048)]);
}

#[test]
fn ota_session_holds_next_sector_until_status() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    for offset in (0..4096).step_by(1024) {
        bench.answer_chunk(offset, true);
    }
    // The node writes the sector on its last chunk, and says so before the next goes.
    assert_eq!(bench.queued(), []);
    bench.receiving(4096);
    assert_eq!(bench.queued(), [Out::Chunk(4096), Out::Chunk(5120)]);
}

#[test]
fn ota_session_rewinds_to_undelivered_chunk() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    bench.answer_chunk(0, true);
    bench.answer_chunk(1024, false);
    assert_eq!(bench.queued(), [Out::Chunk(2048), Out::Chunk(1024)]);
    // 2048 reached a node lacking 1024, which drops it, so it goes again after 1024.
    bench.answer_chunk(2048, true);
    assert_eq!(bench.queued(), [Out::Chunk(1024), Out::Chunk(2048)]);
}

#[test]
fn ota_session_resends_from_status_offset_without_crossing_sector() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    for offset in (0..4096).step_by(1024) {
        bench.answer_chunk(offset, true);
    }
    bench.receiving(4096);
    bench.answer_chunk(4096, true);
    bench.answer_chunk(5120, true);
    assert_eq!(bench.queued(), [Out::Chunk(6144), Out::Chunk(7168)]);
    // The node lost 4096, and says so on 5120.
    bench.receiving(4096);
    let mut resent = Vec::new();
    for offset in [6144, 7168, 4096, 5120, 6144, 7168] {
        let before = bench.queue.len();
        bench.answer_chunk(offset, true);
        if bench.queue.len() == before {
            resent.extend(bench.queued().last().copied());
        }
    }
    assert_eq!(resent, [Out::Chunk(4096), Out::Chunk(5120), Out::Chunk(6144), Out::Chunk(7168)]);
    assert_eq!(bench.queued(), [], "nothing past 8192 without its sector's status");
}

#[test]
fn ota_session_ignores_status_when_it_names_a_chunk_awaiting_delivery() {
    let mut bench = Bench::new(&image(NEW));
    bench.open();
    for offset in (0..4096).step_by(1024) {
        bench.answer_chunk(offset, true);
    }
    bench.receiving(4096);
    // Written before 4096 arrived, as a duplicate's status is: resending would send it twice.
    bench.receiving(4096);
    assert_eq!(bench.queued(), [Out::Chunk(4096), Out::Chunk(5120)]);
    bench.answer_chunk(4096, true);
    assert_eq!(bench.queued(), [Out::Chunk(5120), Out::Chunk(6144)]);
}

#[test]
fn ota_session_ignores_delivery_for_another_sessions_id() {
    let mut bench = Bench::with_ids(&image(NEW), OLD, Ids { next: 2, step: 3 });
    bench.beat();
    bench.receiving(0);
    let ids: Vec<u16> = bench
        .queue
        .iter()
        .map(|cmd| match cmd {
            HostToBridge::ToNode { id, .. } | HostToBridge::ToNodeChunk { id, .. } => *id,
            other => panic!("the session sent {other:?}"),
        })
        .collect();
    assert_eq!(ids, [2, 5, 8], "the begin, then two chunks");
    let queued = bench.queue.clone();
    bench.feed(OtaEvent::Delivery { id: 3, delivered: true });
    bench.feed(OtaEvent::Delivery { id: 4, delivered: false });
    assert_eq!(bench.queue, queued, "nothing sent");
    assert_eq!(bench.session.progress().phase, Phase::Streaming);
}

#[test]
fn ota_session_sends_chunks_as_tonodechunk_with_image_tag() {
    let image = image(NEW);
    let mut bench = Bench::new(&image);
    bench.open();
    let HostToBridge::ToNodeChunk { slot, image: tag, session, offset, len, .. } = bench.queue[1]
    else {
        panic!("a chunk command: {:?}", bench.queue[1]);
    };
    assert_eq!((slot, tag, session), (SLOT, image_tag(&image.sha256), SESSION));
    assert_eq!((offset, usize::from(len)), (1024, UPDATE_CHUNK));
    // The last is what is left of the image.
    assert_eq!(bench.run_to_end(&mut deliver), Outcome::Done(NEW));
    bench.assert_image_written();
}

// End to end, against the simulated fleet.

/// How much faster than real time the fake fleet runs.
const SPEED: f64 = 20.0;

async fn connected(link: &mut LinkHandle) {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(_)) => return,
            Some(_) => {}
            None => panic!("the link closed"),
        }
    }
}

/// The version each node heartbeats, once all of `nodes` have.
async fn versions(link: &mut LinkHandle, nodes: &[Mac]) -> Vec<Capabilities> {
    let mut heard = vec![None; nodes.len()];
    while heard.iter().any(Option::is_none) {
        if let Some(LinkEvent::Message(BridgeToHost::FromNode { kind, body, .. })) =
            link.recv().await
            && kind == UpType::Heartbeat.as_u8()
            && let Ok((caps, mac)) = Heartbeat::identity(&body)
            && let Some(i) = nodes.iter().position(|n| *n == mac)
        {
            heard[i] = Some(caps);
        }
    }
    heard.into_iter().flatten().collect()
}

fn sim_image(version: Version) -> OtaImage {
    OtaImage { board: Board::Esp32c5, ..image(version) }
}

/// Upload `image` into the bridge's node image cache, as `wartui ota` does first.
async fn upload(link: &mut LinkHandle, image: &OtaImage) {
    let send = |link: &LinkHandle, op: Op<'_>| {
        let mut buf = [0u8; OP_MAX];
        let len = op.encode(&mut buf);
        let op = UpdateOp::from_slice(&buf[..len]).expect("an op fits");
        link.send_urgent(HostToBridge::CacheImage(op)).expect("queued");
    };
    let len = u32::try_from(image.bytes.len()).expect("a small image");
    send(link, Op::Begin { session: 1, len, sha256: image.sha256 });
    for (i, data) in image.bytes.chunks(UPDATE_CHUNK).enumerate() {
        let offset = u32::try_from(i * UPDATE_CHUNK).expect("a small image");
        send(link, Op::Chunk { session: 1, offset, data });
    }
    send(link, Op::Commit { session: 1 });
    loop {
        if let Some(LinkEvent::Message(BridgeToHost::CacheImage(bytes))) = link.recv().await
            && let Some(Op::Status(status)) = Op::decode(&bytes)
            && status.state == UpdateState::Verified
        {
            return;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn drive_fleet_updates_sim_fleet_when_nodes_drop_chunks_lose_verified_and_roll_back() {
    let old = wartui_bridge::sim::minor_behind(NODE_VERSION);
    let mut link = SimTransport::new(SimConfig {
        node_count: 4,
        speed: SPEED,
        fleet_version: old,
        lossy_node: Some((1, 0.2)),
        rejecting_node: Some(2),
        mute_verified_node: Some(3),
        ..SimConfig::default()
    })
    .start()
    .expect("starting the simulator");
    connected(&mut link).await;
    let nodes: Vec<Mac> = (0..4).map(SimTransport::node_mac).collect();
    for caps in versions(&mut link, &nodes).await {
        assert_eq!((caps.version, caps.trial), (old, false));
    }

    let new = NODE_VERSION;
    let image = sim_image(new);
    upload(&mut link, &image).await;
    let mut sessions: Vec<OtaSession> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| {
            let session = u8::try_from(0x40 + i).unwrap_or(1);
            OtaSession::new(*node, old, &image, session, Ids::of(i, nodes.len()), now())
        })
        .collect();
    let outcomes = drive_fleet(&mut link, &mut sessions, std::future::pending(), |_, _| {}).await;
    assert_eq!(outcomes[0], Outcome::Done(new));
    assert_eq!(outcomes[1], Outcome::Done(new), "chunk drops are recovered");
    assert_eq!(outcomes[2], Outcome::Failed(Failure::RolledBack(old)));
    assert_eq!(outcomes[3], Outcome::Done(new), "a lost `Verified` is recovered");

    let after = versions(&mut link, &nodes).await;
    assert_eq!((after[0].version, after[0].trial), (new, false));
    assert_eq!((after[1].version, after[1].trial), (new, false));
    assert_eq!((after[2].version, after[2].trial), (old, false));
    assert_eq!((after[3].version, after[3].trial), (new, false));
}
