//! The host's update session, driven on a clock the test invents against a scripted node
//! running the real receiver over a slot in memory. Then the same session against the
//! simulated fleet, end to end.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::{LinkEvent, LinkHandle};
use wartui_core::engine::Now;
use wartui_core::ota::{
    BEGIN_TIMEOUT, CONFIRM_TIMEOUT, Failure, MAX_REBEGINS, MAX_SEND_FAILURES, OtaEvent, OtaImage,
    OtaSession, Outcome, Phase, REBOOT_GRACE,
};
use wartui_core::runtime::{drive_ota, now};
use wartui_proto::air::{
    Board, Capabilities, DieTemp, Frame, HeartbeatMsg, OtaState, OtaStatusMsg, Vdd, Version,
};
use wartui_proto::link::{BridgeToHost, HostToBridge, SendStatus};
use wartui_proto::mac::Mac;
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
use wartui_proto::reset::ResetCause;

const NODE: Mac = [0x02, 0, 0, 0, 0, 7];
const OLD: Version = Version::new(0, 1, 0);
const NEW: Version = Version::new(0, 1, 1);
const BOARD: Board = Board::Esp32c6;
const SESSION: u8 = 0x5A;
/// Five and a half sectors, so the last chunk is short and the last sector partial.
const IMAGE_LEN: usize = 5 * SECTOR + 1500;

fn image_bytes() -> Vec<u8> {
    (0..IMAGE_LEN).map(|i| u8::try_from(i * 7 % 251).unwrap_or(0)).collect()
}

fn image(version: Version) -> OtaImage {
    let bytes = image_bytes();
    let sha256 = Sha256::digest(&bytes).into();
    OtaImage { bytes: Arc::from(bytes), sha256, version, board: BOARD }
}

/// A slot held in memory, which fails every operation once told to.
struct Slot {
    flash: Vec<u8>,
    fail: bool,
}

impl SectorSink for Slot {
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

/// A node as the firmware behaves: any host frame it decodes confirms a trial image, it
/// answers only the host it has accepted an update from, and it reboots into the image on
/// `Verified`.
struct Node {
    receiver: Box<Receiver>,
    slot: Slot,
    version: Version,
    trial: bool,
    /// Whether the host is this node's core, so it is answered.
    core: bool,
    /// Never confirms a trial image.
    rejecting: bool,
    /// Reports `Verified` and stays on the old image.
    cannot_switch: bool,
}

impl Node {
    fn new() -> Self {
        Self {
            receiver: Box::new(Receiver::new(BOARD)),
            slot: Slot { flash: vec![0; 0x1E_0000], fail: false },
            version: OLD,
            trial: false,
            core: false,
            rejecting: false,
            cannot_switch: false,
        }
    }

    fn heartbeat(&self) -> Vec<u8> {
        heartbeat(self.version, self.trial)
    }

    /// Take a host frame, returning the status the node sends back.
    fn hear(&mut self, payload: &[u8]) -> Option<OtaStatusMsg> {
        let frame = Frame::decode(payload).ok()?;
        if !self.rejecting {
            self.trial = false;
        }
        let held = self.receiver.is_active().then(|| self.receiver.status().session);
        let (status, accepted) = match frame {
            Frame::OtaBegin(msg) if self.trial => (
                OtaStatusMsg { session: msg.session, state: OtaState::FlashError, next_offset: 0 },
                false,
            ),
            Frame::OtaBegin(msg) => {
                let status = self.receiver.begin(&msg, &self.slot);
                (status, matches!(status.state, OtaState::Receiving | OtaState::Verified))
            }
            Frame::OtaChunk(msg) => {
                let status = self.receiver.chunk(&msg, &mut self.slot);
                if held == Some(msg.session) {
                    self.core = true;
                }
                (status?, held == Some(msg.session))
            }
            Frame::OtaCommit(msg) => {
                (self.receiver.commit(&msg, &mut self.slot), held == Some(msg.session))
            }
            Frame::OtaAbort(msg) => (self.receiver.abort(&msg), held == Some(msg.session)),
            _ => return None,
        };
        if accepted {
            self.core = true;
        }
        let answer = self.core.then_some(status);
        if status.state == OtaState::Verified && !self.cannot_switch {
            self.reboot(self.receiver.version().unwrap_or(self.version), true);
        }
        answer
    }

    fn reboot(&mut self, version: Version, trial: bool) {
        self.receiver.reset();
        self.version = version;
        self.trial = trial;
        self.core = false;
    }
}

fn heartbeat(version: Version, trial: bool) -> Vec<u8> {
    HeartbeatMsg {
        counter: 1,
        epoch: 0,
        capabilities: Capabilities { version, trial, ..Capabilities::here(BOARD) },
        wifi_refused: 0,
        ble_refused: 0,
        beat: 1,
        unsent: 0,
        dwell: 0,
        prev_dwell: 0,
        tx_power: 8,
        reset_cause: ResetCause::PowerOn,
        vdd: Vdd::NONE,
        die_temp: DieTemp::NONE,
    }
    .encode()
    .to_vec()
}

/// A frame the session sent, for asserting on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Out {
    Begin,
    Chunk(u32),
    Commit,
    Abort,
    Confirm,
}

fn summarize(payload: &[u8]) -> Out {
    match Frame::decode(payload) {
        Ok(Frame::OtaBegin(_)) => Out::Begin,
        Ok(Frame::OtaChunk(msg)) => Out::Chunk(msg.offset),
        Ok(Frame::OtaCommit(_)) => Out::Commit,
        Ok(Frame::OtaAbort(_)) => Out::Abort,
        Ok(Frame::OtaConfirm(_)) => Out::Confirm,
        other => panic!("the session sent {other:?}"),
    }
}

/// What becomes of one frame on its way to the node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fate {
    /// Acked and read; the node's answer comes back.
    Deliver,
    /// Acked and never read: the node's receive queue overflowed.
    Lose,
    /// Not acked, and not read.
    Unacked,
    /// Acked and read, and the node's answer is lost.
    Mute,
    /// Read, and its ack is lost.
    Unheard,
}

fn deliver(_: Out) -> Fate {
    Fate::Deliver
}

/// The session, the node, and a clock.
struct Bench {
    session: OtaSession,
    node: Node,
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
        let start = Instant::now();
        let now = Now { mono: start, unix_ms: 0 };
        let mut node = Node::new();
        node.version = from;
        Self {
            session: OtaSession::new(NODE, from, image, SESSION, 1, now),
            node,
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
        self.feed(OtaEvent::Rx { src: NODE, payload: &frame });
    }

    fn beat_as(&mut self, version: Version, trial: bool) {
        let frame = heartbeat(version, trial);
        self.feed(OtaEvent::Rx { src: NODE, payload: &frame });
    }

    /// Carry every queued frame to the node, the result back before the node's answer as
    /// the bridge's priority ring has it, until nothing is left to send.
    fn settle(&mut self, fate: &mut impl FnMut(Out) -> Fate) {
        self.settle_until(fate, &|_| false);
    }

    /// [`Self::settle`], stopping early once `done`.
    fn settle_until(&mut self, fate: &mut impl FnMut(Out) -> Fate, done: &impl Fn(&Self) -> bool) {
        while !done(self)
            && let Some(cmd) = self.queue.pop_front()
        {
            let HostToBridge::SendEspNow { id, dst, ensure_peer, payload } = cmd else {
                panic!("the session sent {cmd:?}");
            };
            assert_eq!(dst, NODE);
            assert!(ensure_peer);
            let out = summarize(&payload);
            self.sent.push(out);
            let fate = fate(out);
            let status = match fate {
                Fate::Deliver | Fate::Mute | Fate::Unheard => self.node.hear(&payload),
                Fate::Lose | Fate::Unacked => None,
            };
            let acked = if matches!(fate, Fate::Unacked | Fate::Unheard) {
                SendStatus::AckFail
            } else {
                SendStatus::AckOk
            };
            self.feed(OtaEvent::SendResult { id, status: acked });
            if let Some(status) = status.filter(|_| fate != Fate::Mute) {
                self.feed(OtaEvent::Rx { src: NODE, payload: &status.encode() });
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
        assert_eq!(&self.node.slot.flash[..IMAGE_LEN], &image_bytes()[..]);
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
    // A node outside update mode hears nothing but its admin window.
    for _ in 0..20 {
        bench.elapsed += Duration::from_millis(50);
        bench.feed(OtaEvent::Tick);
    }
    assert!(bench.queue.is_empty(), "nothing before a heartbeat: {:?}", bench.queue);
    bench.beat();
    bench.settle(&mut deliver);
    assert_eq!(bench.sent.first(), Some(&Out::Begin));
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
fn ota_session_resends_chunk_when_send_is_unacked() {
    let mut bench = Bench::new(&image(NEW));
    let mut failed = false;
    let mut fate = |out| {
        if out == Out::Chunk(2048) && !failed {
            failed = true;
            return Fate::Unacked;
        }
        Fate::Deliver
    };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Done(NEW));
    assert_eq!(bench.count(Out::Chunk(2048)), 2);
    assert_eq!(bench.count(Out::Chunk(1024)), 1);
    bench.assert_image_written();
}

#[test]
fn ota_session_fails_when_chunks_stay_unacked_across_every_begin() {
    let mut bench = Bench::new(&image(NEW));
    let mut fate = |out| if out == Out::Chunk(2048) { Fate::Unacked } else { Fate::Deliver };
    assert_eq!(bench.run_to_end(&mut fate), Outcome::Failed(Failure::NotAnswering));
    // Each begin resumes at 2048 and sends it until the streak gives up on it.
    let per_begin = MAX_SEND_FAILURES as usize + 1;
    let begins = MAX_REBEGINS as usize + 1;
    assert_eq!(bench.count(Out::Chunk(2048)), per_begin * begins);
    assert_eq!(bench.count(Out::Begin), begins, "the first begin and every return to it");
    assert_eq!(bench.sent.last(), Some(&Out::Abort), "the node is told to leave update mode");
}

#[test]
fn ota_session_begins_again_and_restarts_when_node_reboots_mid_stream() {
    let mut bench = Bench::new(&image(NEW));
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.session.progress().acked >= 8192);
    // A hard reset: the radio acks nothing while the node boots, and the node comes back
    // without the session or its core.
    bench.node.reboot(OLD, false);
    let mut fate = |_| Fate::Unacked;
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
    let idle = OtaStatusMsg { session: SESSION, state: OtaState::Idle, next_offset: 0 };
    bench.feed(OtaEvent::Rx { src: NODE, payload: &idle.encode() });
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
    bench.node.slot.fail = true;
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
    // `Verified` is lost; every resent commit then goes unacked while the node reboots into
    // the new image, which returns the session to its `begin`.
    let mut fate = |out| match out {
        Out::Commit if first => {
            first = false;
            Fate::Mute
        }
        Out::Commit => Fate::Unacked,
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
fn ota_session_resends_confirm_when_its_ack_is_lost_on_same_version() {
    // `--force` onto the version the node runs: only an acked confirm says it is done.
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
    assert_eq!(bench.count(Out::Confirm), 2, "once more after the lost ack");
}

#[test]
fn ota_session_confirms_trial_node_when_started_to_confirm() {
    let start = Instant::now();
    let now = |ms| Now { mono: start + Duration::from_millis(ms), unix_ms: 0 };
    let mut session = OtaSession::confirm_trial(NODE, NEW, 1, now(0));
    let beat = heartbeat(NEW, true);
    let sends = session.handle(now(10), OtaEvent::Rx { src: NODE, payload: &beat });
    let [HostToBridge::SendEspNow { id, payload, .. }] = sends.as_slice() else {
        panic!("one confirm: {sends:?}")
    };
    assert_eq!(summarize(payload), Out::Confirm);
    session.handle(now(20), OtaEvent::SendResult { id: *id, status: SendStatus::AckOk });
    let beat = heartbeat(NEW, false);
    session.handle(now(1000), OtaEvent::Rx { src: NODE, payload: &beat });
    assert_eq!(session.outcome(), Some(&Outcome::Done(NEW)));
}

#[test]
fn ota_session_ignores_stale_status_when_newer_one_arrived_first() {
    let mut bench = Bench::new(&image(NEW));
    // Stopped with chunk 7168 queued and the node's last status at 4096.
    bench.run(&mut deliver, Duration::from_secs(5), |b| b.sent.contains(&Out::Chunk(6144)));
    // Two gap statuses, the newer first: the older is stale and must not rewind the host.
    let status = |next_offset| {
        OtaStatusMsg { session: SESSION, state: OtaState::Receiving, next_offset }.encode()
    };
    bench.feed(OtaEvent::Rx { src: NODE, payload: &status(6144) });
    bench.feed(OtaEvent::Rx { src: NODE, payload: &status(5120) });
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
    // The resent commit reaches the new image, which confirms on hearing the host and does
    // not answer a host it has not accepted an update from.
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
fn ota_session_ignores_frames_when_another_node_sends_them() {
    let mut bench = Bench::new(&image(NEW));
    let frame = heartbeat(OLD, false);
    let sends = bench.session.handle(bench.now(), OtaEvent::Rx { src: [9; 6], payload: &frame });
    assert!(sends.is_empty());
}

#[test]
fn ota_session_reads_heartbeat_prefix_when_node_runs_another_minor() {
    let mut bench = Bench::new(&image(NEW));
    // A longer heartbeat of some other minor's layout: only the prefix is read.
    let mut frame = heartbeat(Version::new(0, 0, 9), false);
    frame.extend_from_slice(&[0xAA; 9]);
    bench.feed(OtaEvent::Rx { src: NODE, payload: &frame });
    let queued: Vec<Out> = bench
        .queue
        .iter()
        .map(|cmd| match cmd {
            HostToBridge::SendEspNow { payload, .. } => summarize(payload),
            other => panic!("the session sent {other:?}"),
        })
        .collect();
    assert_eq!(queued, [Out::Begin]);
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
        if let Some(LinkEvent::Message(BridgeToHost::Rx { src, payload, .. })) = link.recv().await
            && let Ok(caps) = Capabilities::decode_prefix(&payload)
            && let Some(i) = nodes.iter().position(|n| *n == src)
        {
            heard[i] = Some(caps);
        }
    }
    heard.into_iter().flatten().collect()
}

fn sim_image(version: Version) -> OtaImage {
    OtaImage { board: Board::Esp32c5, ..image(version) }
}

#[tokio::test(start_paused = true)]
async fn drive_ota_updates_sim_fleet_when_nodes_drop_chunks_lose_verified_and_roll_back() {
    let old = wartui_bridge::sim::minor_behind(wartui_proto::air::NODE_VERSION);
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

    let new = wartui_proto::air::NODE_VERSION;
    let image = sim_image(new);
    let mut next_id = 1;
    let mut outcomes = Vec::new();
    for (i, node) in nodes.iter().enumerate() {
        let session = u8::try_from(0x40 + i).unwrap_or(1);
        let mut ota = OtaSession::new(*node, old, &image, session, next_id, now());
        let outcome = drive_ota(&mut link, &mut ota, std::future::pending(), |_| {}).await;
        next_id = ota.next_id();
        outcomes.push(outcome);
    }
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
