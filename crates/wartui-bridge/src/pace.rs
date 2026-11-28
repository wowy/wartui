//! An update's ops, paced to the link that carries them.
//!
//! A [`Sender`] re-sends the op it asked after [`REPLY_WAIT_MS`] of silence. That wait is for the
//! receiver's answer, so it has to start when the op is written, not when it is queued. Each
//! transport counts a command done once it is written: over USB once its writer thread has
//! written its bytes, over Bluetooth once the C6 answers its write group, about 60 ms or more. A
//! sector's ops can sit queued past the wait: timed from the queue, the sender re-sends into the
//! backlog it timed out behind, and the queue fills.
//!
//! [`PacedSender`] asks its sender for nothing until the link has written the last op it queued,
//! then reports the write ([`Sender::written`]), so the wait runs from there. So at most one
//! sector's ops are queued at once, and a slow link holds the sender rather than filling the
//! queue. A transport counts every command it takes as done, written or dropped
//! (`crate` docs), so an op lost with a discarded queue starts the wait too, and its silence
//! re-sends it.
//!
//! **A link that takes no commands stalls the pacing, not the update.** A transport reads none
//! between connections: serial after a pulled cable, Bluetooth while its link is down or a USB host
//! holds the bridge. A USB write that does not drain takes none either, its writer thread held in
//! `write`. Its count never moves, so a sender held on it would wait forever, and its own
//! silence timeout would never run. Once the last op has gone unwritten for [`STALL_MS`], past
//! anything a working link takes, the sender runs on its own timers until the link's backlog is
//! empty again, and its silence fails the update ([`PacedSender::is_stalled`] says so). Past
//! [`STALL_OPS`] queued in that state, an op is dropped as a refused one is: the sender counts it
//! sent.
//!
//! **A session the host gives up on is ended with `Abort`** ([`abort`]), shared by the bridge's
//! own update and the node image upload. The bridge answers it with `Idle`, and that wait is
//! timed from the write too, so an `Abort` queued behind a slow Bluetooth write still counts as
//! answered. A link that goes down ends the wait at once, since nothing more is written. A bridge
//! that does not answer is no error: its session ends anyway when the host goes or the next
//! `Begin` replaces it.
//!
//! [`REPLY_WAIT_MS`]: wartui_proto::radio_link::update::REPLY_WAIT_MS

use std::time::Duration;

use wartui_proto::bus::{UpdateState, UpdateStatus};
use wartui_proto::link::{BridgeToHost, HostToBridge, UpdateOp};
use wartui_proto::radio_link::update::{OP_MAX, Op, REPLY_WAIT_MS, Sender, Stage};

use crate::{LinkEvent, LinkHandle, SendError};

/// How long the last op may go unwritten before the link counts as stalled: twice the Bluetooth
/// transport's bound on one write (`ble::WRITE_WITHIN`, 5 s), so a write it still waits on never
/// counts.
const STALL_MS: u64 = 10_000;

/// The most ops queued past the backlog once the link is stalled: enough for the sender's
/// re-sends of the op it asked, which end in its silence.
const STALL_OPS: u64 = 5;

/// A [`Sender`] that queues its next ops only once the link has written its last.
#[derive(Debug, Clone)]
pub struct PacedSender<'a> {
    sender: Sender<'a>,
    /// What the link owes, until it has written it.
    owed: Option<Owed>,
}

/// Ops queued and not yet written.
#[derive(Debug, Clone, Copy)]
struct Owed {
    /// The link's [`LinkHandle::queued`] count once the last op was queued.
    queued: u64,
    /// When the first of them was queued.
    since_ms: u64,
    /// Ops queued since the link stalled, `None` until it has.
    stalled: Option<u64>,
}

impl<'a> PacedSender<'a> {
    /// Pace `sender`.
    #[must_use]
    pub const fn new(sender: Sender<'a>) -> Self {
        Self { sender, owed: None }
    }

    /// Queue every op the sender has at time `now_ms`, each as `wrap` makes it a command, once
    /// the link has written the ops queued before. Call it on each status, on a timer and on
    /// each [`LinkHandle::written`] change while [`Self::is_waiting`].
    ///
    /// # Errors
    /// [`SendError`] if the link refused an op. The sender counts it sent, so its silence
    /// re-sends it.
    pub fn pump(
        &mut self,
        link: &LinkHandle,
        now_ms: u64,
        wrap: impl Fn(UpdateOp) -> HostToBridge,
    ) -> Result<(), SendError> {
        if let Some(owed) = &mut self.owed {
            if link.written_count() >= owed.queued {
                self.owed = None;
                self.sender.written(now_ms);
            } else if owed.stalled.is_none() {
                if now_ms.saturating_sub(owed.since_ms) < STALL_MS {
                    return Ok(());
                }
                owed.stalled = Some(0);
            }
        }
        let mut buf = [0u8; OP_MAX];
        while let Some(len) = self.sender.next(now_ms, &mut buf) {
            if let Some(Owed { stalled: Some(extra), .. }) = &mut self.owed {
                if *extra >= STALL_OPS {
                    continue;
                }
                *extra += 1;
            }
            let op = UpdateOp::from_slice(&buf[..len]).expect("an op fits OP_MAX");
            link.send_urgent(wrap(op))?;
            let queued = link.queued();
            match &mut self.owed {
                Some(owed) => owed.queued = queued,
                None => self.owed = Some(Owed { queued, since_ms: now_ms, stalled: None }),
            }
        }
        Ok(())
    }

    /// Whether it waits for the link to write what it queued, so a [`LinkHandle::written`]
    /// change can let its next ops go.
    #[must_use]
    pub fn is_waiting(&self) -> bool {
        self.owed.is_some()
    }

    /// Whether it holds ops the link has left unwritten for [`STALL_MS`], so a silence that
    /// fails the update is the link's rather than the receiver's.
    #[must_use]
    pub fn is_stalled(&self) -> bool {
        self.owed.is_some_and(|owed| owed.stalled.is_some())
    }

    /// The session its ops name ([`Sender::session`]).
    #[must_use]
    pub fn session(&self) -> u8 {
        self.sender.session()
    }

    /// Take a status from the receiver ([`Sender::on_status`]).
    pub fn on_status(&mut self, status: &UpdateStatus) {
        self.sender.on_status(status);
    }

    /// Where the sender stands ([`Sender::stage`]).
    #[must_use]
    pub fn stage(&self) -> Stage {
        self.sender.stage()
    }

    /// Whether the receiver has reported the image verified ([`Sender::is_verified`]).
    #[must_use]
    pub fn is_verified(&self) -> bool {
        self.sender.is_verified()
    }

    /// The share of the image the receiver has reported holding ([`Sender::percent`]).
    #[must_use]
    pub fn percent(&self) -> u8 {
        self.sender.percent()
    }
}

/// The status an encoded op carries, if it is one: what an `abort` caller's extractor reads out
/// of its own message.
#[must_use]
pub fn status_in(bytes: &[u8]) -> Option<UpdateStatus> {
    match Op::decode(bytes)? {
        Op::Status(status) => Some(status),
        _ => None,
    }
}

/// End `session` with an `Abort`, which `wrap` makes a command, and wait for the bridge's `Idle`
/// for it, which `status` reads out of a message. The wait is [`REPLY_WAIT_MS`] from the link
/// writing the op, and at most [`STALL_MS`] for the write; a `Disconnected` ends it at once. Every
/// other event that arrives meanwhile is dropped: the caller is stopping.
///
/// Returns whether the `Idle` arrived. A bridge that does not answer is not an error.
pub async fn abort(
    link: &mut LinkHandle,
    session: u8,
    wrap: impl Fn(UpdateOp) -> HostToBridge,
    status: impl Fn(&BridgeToHost) -> Option<UpdateStatus>,
) -> bool {
    let mut buf = [0u8; OP_MAX];
    let len = Op::Abort { session }.encode(&mut buf);
    let op = UpdateOp::from_slice(&buf[..len]).expect("an op fits OP_MAX");
    if link.send_urgent(wrap(op)).is_err() {
        return false;
    }
    let queued = link.queued();
    let mut written = link.written();
    let write_by = tokio::time::Instant::now() + Duration::from_millis(STALL_MS);
    // When the answer is due, once the link has written the op.
    let mut reply: Option<tokio::time::Instant> = None;
    loop {
        if reply.is_none() && link.written_count() >= queued {
            reply = Some(tokio::time::Instant::now() + Duration::from_millis(REPLY_WAIT_MS));
        }
        let until = reply.unwrap_or(write_by);
        tokio::select! {
            () = tokio::time::sleep_until(until) => return false,
            () = written.changed(), if reply.is_none() => {}
            event = link.recv() => match event {
                Some(LinkEvent::Message(message)) => {
                    let idle = status(&message).is_some_and(|status| {
                        status.session == session && status.state == UpdateState::Idle
                    });
                    if idle {
                        return true;
                    }
                }
                // Nothing more is written, and the bridge drops the session with its host.
                Some(LinkEvent::Disconnected { .. }) | None => return false,
                Some(_) => {}
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use sha2::{Digest, Sha256};
    use tokio::time::Instant;
    use wartui_proto::bus::{Board, UPDATE_CHUNK};
    use wartui_proto::link::{BridgeToHost, UPDATE_STATUS_MAX, UpdateStatusBytes};
    use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
    use wartui_proto::radio_link::update::{Failure, Op, REPLY_WAIT_MS, Received, TRIES, receive};

    use super::*;
    use crate::{LinkEvent, LinkPlumbing, link_pair};

    const DIGEST: [u8; 32] = [0x42; 32];

    /// Four sectors and a part, so the last sector ends on the commit.
    const IMAGE_LEN: usize = 4 * SECTOR + 1_500;

    /// The ops one sector takes.
    const SECTOR_OPS: u64 = (SECTOR / UPDATE_CHUNK) as u64;

    struct Slot(Vec<u8>);

    impl SectorSink for Slot {
        type Error = ();

        fn capacity(&self) -> u32 {
            u32::try_from(self.0.len()).unwrap_or(u32::MAX)
        }

        fn erase(&mut self, offset: u32, len: u32) -> Result<(), ()> {
            let at = offset as usize;
            self.0[at..at + len as usize].fill(0xFF);
            Ok(())
        }

        fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), ()> {
            let at = offset as usize;
            self.0[at..at + data.len()].copy_from_slice(data);
            Ok(())
        }
    }

    /// What a written op was.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Sent {
        Begin,
        Chunk(u32),
        Commit,
        Install,
        Abort,
    }

    fn sent(op: &Op<'_>) -> Sent {
        match op {
            Op::Begin { .. } => Sent::Begin,
            Op::Chunk { offset, .. } => Sent::Chunk(*offset),
            Op::Commit { .. } => Sent::Commit,
            Op::Install { .. } => Sent::Install,
            Op::Abort { .. } => Sent::Abort,
            Op::Status(_) => panic!("a status sent down"),
        }
    }

    /// What the fake transport does with one op it took.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Fate {
        /// Write it, and pass the receiver's status up.
        Write,
        /// Write it, and lose the status.
        LoseStatus,
        /// Drop it and everything queued behind it, as a new connection does.
        Discard,
        /// Take nothing more for this long, as a link between connections, then write it.
        Stall(Duration),
    }

    /// Every op the transport wrote, and when.
    type Log = Arc<Mutex<Vec<(Sent, Instant)>>>;

    /// A transport that takes `per_command` over each command, one at a time, before a receiver
    /// on the bridge takes it, and counts each done. `fate` decides each op's end.
    fn transport(
        mut plumbing: LinkPlumbing,
        per_command: Duration,
        mut fate: impl FnMut(Sent) -> Fate + Send + 'static,
    ) -> Log {
        let log = Log::default();
        let written = Arc::clone(&log);
        tokio::spawn(async move {
            let mut receiver = Box::new(Receiver::new(Board::Esp32c6));
            let mut slot = Slot(vec![0xFF; 64 * SECTOR]);
            while let Some(cmd) = plumbing.commands.recv().await {
                tokio::time::sleep(per_command).await;
                let HostToBridge::Update(bytes) = cmd else { panic!("not an update op: {cmd:?}") };
                let op = Op::decode(&bytes).expect("an op");
                let what = sent(&op);
                let fate = fate(what);
                if let Fate::Stall(stall) = fate {
                    tokio::time::sleep(stall).await;
                }
                if fate == Fate::Discard {
                    plumbing.commands.done(1);
                    plumbing.commands.discard();
                    continue;
                }
                written.lock().unwrap().push((what, Instant::now()));
                let status = match receive(&mut receiver, op, Some(&mut slot), false) {
                    Received::Status(status) => status,
                    Received::Install(_) => None,
                };
                plumbing.commands.done(1);
                if let Some(status) = status.filter(|_| fate != Fate::LoseStatus) {
                    let mut buf = [0u8; UPDATE_STATUS_MAX];
                    let len = Op::Status(status).encode(&mut buf);
                    let up =
                        BridgeToHost::Update(UpdateStatusBytes::from_slice(&buf[..len]).unwrap());
                    if plumbing.events.send(LinkEvent::Message(up)).await.is_err() {
                        return;
                    }
                }
            }
        });
        log
    }

    fn image() -> Vec<u8> {
        (0..IMAGE_LEN).map(|i| u8::try_from(i % 251).unwrap_or(0)).collect()
    }

    /// Drive `paced` over `link` as `flash-bridge` does, until it installs, and return the
    /// largest backlog it left after a pump, or how it failed.
    async fn drive(link: &mut LinkHandle, paced: &mut PacedSender<'_>) -> Result<u64, Failure> {
        let start = Instant::now();
        let mut written = link.written();
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        let mut most = 0;
        loop {
            let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            paced.pump(link, now, HostToBridge::Update).expect("the op is queued");
            most = most.max(link.backlog());
            match paced.stage() {
                Stage::Sending => {}
                Stage::Installed => return Ok(most),
                Stage::Failed(failure) => return Err(failure),
            }
            tokio::select! {
                _ = tick.tick() => {}
                () = written.changed(), if paced.is_waiting() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(BridgeToHost::Update(bytes))) => {
                        if let Some(Op::Status(status)) = Op::decode(&bytes) {
                            paced.on_status(&status);
                        }
                    }
                    Some(_) => {}
                    None => panic!("the link closed"),
                },
            }
        }
    }

    /// Run one update over a transport taking `per_command` per command, to its end.
    async fn run(
        per_command: Duration,
        fate: impl FnMut(Sent) -> Fate + Send + 'static,
    ) -> (Log, Result<u64, Failure>) {
        let image = image();
        let sha256: [u8; 32] = Sha256::digest(&image).into();
        let (mut link, plumbing) = link_pair();
        let log = transport(plumbing, per_command, fate);
        let mut paced = PacedSender::new(Sender::new(&image, sha256, &DIGEST));
        let end = drive(&mut link, &mut paced).await;
        (log, end)
    }

    /// [`run`], for an update that installs.
    async fn update(
        per_command: Duration,
        fate: impl FnMut(Sent) -> Fate + Send + 'static,
    ) -> (Log, u64) {
        let (log, end) = run(per_command, fate).await;
        (log, end.expect("the update installs"))
    }

    fn chunks(log: &Log, offset: u32) -> Vec<Instant> {
        let log = log.lock().unwrap();
        log.iter().filter(|(sent, _)| *sent == Sent::Chunk(offset)).map(|(_, at)| *at).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_sends_each_chunk_once_when_transport_outlasts_the_reply_wait() {
        // Each command takes longer than the reply wait: timed from the queue, every asked
        // chunk would go again before it was written.
        let per_command = Duration::from_millis(REPLY_WAIT_MS + 200);
        let (log, _) = update(per_command, |_| Fate::Write).await;
        for offset in (0..IMAGE_LEN).step_by(UPDATE_CHUNK) {
            let offset = u32::try_from(offset).unwrap();
            assert_eq!(chunks(&log, offset).len(), 1, "chunk {offset}");
        }
        let log = log.lock().unwrap();
        assert_eq!(log.iter().filter(|(sent, _)| *sent == Sent::Begin).count(), 1);
        assert_eq!(log.iter().filter(|(sent, _)| *sent == Sent::Commit).count(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_keeps_backlog_within_a_sector_when_link_is_slow() {
        let (_, most) = update(Duration::from_millis(200), |_| Fate::Write).await;
        assert!(most <= SECTOR_OPS, "{most} commands queued at once");
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_resends_asked_op_when_status_is_lost_after_the_write() {
        let last_of_sector = u32::try_from(SECTOR - UPDATE_CHUNK).unwrap();
        let mut lost = false;
        let (log, _) = update(Duration::from_millis(100), move |sent| {
            if !lost && sent == Sent::Chunk(last_of_sector) {
                lost = true;
                return Fate::LoseStatus;
            }
            Fate::Write
        })
        .await;
        let sends = chunks(&log, last_of_sector);
        assert_eq!(sends.len(), 2, "the asked chunk, once more");
        // It waited behind the sector's other chunks; the wait runs from its write.
        let waited = sends[1] - sends[0];
        assert!(waited >= Duration::from_millis(REPLY_WAIT_MS), "re-sent {waited:?} after");
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_resumes_when_transport_discards_its_queue() {
        // The second chunk goes with the rest of its sector, the asked chunk among them.
        let mut discarded = false;
        let (log, _) = update(Duration::from_millis(100), move |sent| {
            if !discarded && sent == Sent::Chunk(u32::try_from(UPDATE_CHUNK).unwrap()) {
                discarded = true;
                return Fate::Discard;
            }
            Fate::Write
        })
        .await;
        // Installing means the receiver verified the whole image.
        for offset in (0..IMAGE_LEN).step_by(UPDATE_CHUNK) {
            let offset = u32::try_from(offset).unwrap();
            assert!(!chunks(&log, offset).is_empty(), "chunk {offset} never written");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_fails_silent_when_transport_stops_taking_commands() {
        // The second sector's first chunk is the last the link takes, as a pulled cable or a
        // Bluetooth link that stays down leaves it.
        let second = u32::try_from(SECTOR).unwrap();
        let start = Instant::now();
        let (log, end) = run(Duration::from_millis(100), move |sent| {
            if sent == Sent::Chunk(second) {
                return Fate::Stall(Duration::from_secs(3_600));
            }
            Fate::Write
        })
        .await;
        assert_eq!(end, Err(Failure::Silent));
        let took = start.elapsed();
        let bound = Duration::from_millis(STALL_MS + u64::from(TRIES) * REPLY_WAIT_MS);
        assert!(took <= bound + Duration::from_secs(1), "failed after {took:?}");
        assert!(chunks(&log, second).is_empty(), "the stalled chunk was never written");
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_reports_stall_when_link_writes_nothing_for_stall_ms() {
        // `flash-bridge` names a port that stopped taking the image apart from a receiver that
        // stopped answering, and this is how it tells them apart.
        let image = image();
        let sha256: [u8; 32] = Sha256::digest(&image).into();
        let (mut link, plumbing) = link_pair();
        let second = u32::try_from(SECTOR).unwrap();
        transport(plumbing, Duration::from_millis(100), move |sent| {
            if sent == Sent::Chunk(second) {
                return Fate::Stall(Duration::from_secs(3_600));
            }
            Fate::Write
        });
        let mut paced = PacedSender::new(Sender::new(&image, sha256, &DIGEST));
        assert!(!paced.is_stalled());
        assert_eq!(drive(&mut link, &mut paced).await, Err(Failure::Silent));
        assert!(paced.is_stalled());
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_reports_no_stall_when_the_link_writes_everything() {
        let image = image();
        let sha256: [u8; 32] = Sha256::digest(&image).into();
        let (mut link, plumbing) = link_pair();
        transport(plumbing, Duration::from_millis(100), |_| Fate::Write);
        let mut paced = PacedSender::new(Sender::new(&image, sha256, &DIGEST));
        drive(&mut link, &mut paced).await.expect("the update installs");
        assert!(!paced.is_stalled());
    }

    /// The cache's or the update's status, as `abort`'s callers read it.
    fn update_status(message: &BridgeToHost) -> Option<UpdateStatus> {
        match message {
            BridgeToHost::Update(bytes) => status_in(bytes),
            _ => None,
        }
    }

    /// Abort session 7 over a transport taking `per_command` per command, and how long it took.
    async fn abort_over(per_command: Duration, fate: Fate) -> (bool, Duration, Log) {
        let (mut link, plumbing) = link_pair();
        let log = transport(plumbing, per_command, move |_| fate);
        let start = Instant::now();
        let answered = abort(&mut link, 7, HostToBridge::Update, update_status).await;
        (answered, start.elapsed(), log)
    }

    #[tokio::test(start_paused = true)]
    async fn pace_abort_returns_true_when_bridge_answers_idle() {
        let (answered, _, log) = abort_over(Duration::from_millis(10), Fate::Write).await;
        assert!(answered);
        assert_eq!(log.lock().unwrap()[0].0, Sent::Abort);
    }

    #[tokio::test(start_paused = true)]
    async fn pace_abort_returns_false_when_no_idle_arrives_within_reply_wait() {
        let per_command = Duration::from_millis(10);
        let (answered, took, _) = abort_over(per_command, Fate::LoseStatus).await;
        assert!(!answered);
        assert_eq!(took, per_command + Duration::from_millis(REPLY_WAIT_MS));
    }

    #[tokio::test(start_paused = true)]
    async fn pace_abort_waits_from_the_write_when_link_is_slow() {
        // Over Bluetooth a command can sit queued past the reply wait: timed from the queue, the
        // `Idle` would arrive after the wait gave up.
        let per_command = Duration::from_millis(REPLY_WAIT_MS + 200);
        let (answered, took, _) = abort_over(per_command, Fate::Write).await;
        assert!(answered);
        assert!(took > Duration::from_millis(REPLY_WAIT_MS), "{took:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn pace_abort_returns_false_when_link_goes_down() {
        // The transport holds the op unwritten, as one between connections does.
        let (mut link, mut plumbing) = link_pair();
        tokio::spawn(async move {
            let _held = plumbing.commands.recv().await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let reason = "the bridge disconnected".to_owned();
            let gone = LinkEvent::Disconnected { reason, cause: crate::DisconnectCause::Other };
            let _ = plumbing.events.send(gone).await;
            std::future::pending::<()>().await;
        });
        let start = Instant::now();
        assert!(!abort(&mut link, 7, HostToBridge::Update, update_status).await);
        assert_eq!(start.elapsed(), Duration::from_millis(100), "at the drop");
    }

    #[tokio::test(start_paused = true)]
    async fn pace_abort_gives_up_when_link_never_writes() {
        let stall = Fate::Stall(Duration::from_secs(3_600));
        let (answered, took, _) = abort_over(Duration::from_millis(10), stall).await;
        assert!(!answered);
        assert_eq!(took, Duration::from_millis(STALL_MS));
    }

    #[tokio::test(start_paused = true)]
    async fn paced_sender_sends_each_chunk_once_when_transport_resumes_before_the_stall_bound() {
        // Past the reply wait many times over, but short of the stall bound: the sender waits.
        let stall = Duration::from_millis(STALL_MS - 2_000);
        let second = u32::try_from(SECTOR).unwrap();
        let mut stalled = false;
        let (log, _) = update(Duration::from_millis(100), move |sent| {
            if !stalled && sent == Sent::Chunk(second) {
                stalled = true;
                return Fate::Stall(stall);
            }
            Fate::Write
        })
        .await;
        for offset in (0..IMAGE_LEN).step_by(UPDATE_CHUNK) {
            let offset = u32::try_from(offset).unwrap();
            assert_eq!(chunks(&log, offset).len(), 1, "chunk {offset}");
        }
    }
}
