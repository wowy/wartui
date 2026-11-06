//! The host's half of a node's update: a state machine that takes one node from its running
//! image to a new one, confirmed. [`wartui_proto::ota::Receiver`] is the node's half.
//!
//! Like [`crate::engine`], it reads no clock and does no I/O. Time arrives as [`Now`], and
//! what it wants sent comes back as [`HostToBridge`] commands for a driver
//! (`runtime::drive_ota`) to queue.
//!
//! **The first `begin` rides a heartbeat**, which names the node-bus slot the node sits in, so the
//! session learns where to send from the node's own heartbeats. [`UpdateBegin`] goes out on each
//! heartbeat from the node until a status for this session answers it. An accepted `begin` puts
//! the node in update mode, where it services the bus continuously, and everything after it goes
//! out as soon as it is due.
//!
//! **One chunk is in flight at a time**, the next sent only on the previous one's `Delivered` or
//! `Undelivered`. The P4 holds a few records per slot and delivers one transfer per ack, so a
//! burst would fill the queue, and a refusal leaves the host resending anyway.
//!
//! **Flow control is the node's `next_offset`, not the bus's ack.** Delivery says the node took
//! the transfer, not that its receiver took the chunk: a resent chunk arrives twice, and one past
//! a lost chunk arrives at the wrong offset. The node reports a status on each 4 KB sector it writes, so after the chunk
//! that completes a sector the host waits for one. Any status moves the host to the offset it
//! names, back or forward, except one below the highest offset the node has reported since its
//! `begin`: that one was overtaken by a newer status, and acting on it would resend what the node
//! already holds. With no status within [`STATUS_TIMEOUT`], the host resends from the
//! last offset the node reported: a node that has those bytes answers the duplicate with its
//! status, and one that lacks them takes them. One rule recovers lost chunks and lost statuses
//! alike. `Idle` means the node dropped the session (30 s of silence, or a reboot), and so
//! does [`MAX_STATUS_TIMEOUTS`] unanswered in a row. So does [`MAX_SEND_FAILURES`] undelivered
//! sends in a row: a node rebooting acknowledges nothing for a second or so. Each returns to the
//! `begin`, which an identical one resumes where the node's progress stands: from 0 after a
//! reboot, which loses the receiver's state.
//! [`MAX_RESTARTS`] and [`BEGIN_TIMEOUT`] are what give up on the node. A session that gives
//! up after the node accepted it sends [`UpdateAbort`] once, so the node leaves update mode at
//! once rather than after its 30 s of silence.
//!
//! **Success, rollback and silence are told apart by the heartbeat's prefix**: version and trial
//! flag, which every minor reads. A node switches slots the moment it reports
//! `Verified`, so its next boot heartbeats the new version with `trial` set. That heartbeat is
//! success even when the `Verified` itself was lost. The new image is confirmed by the first
//! record from the bridge it decodes; [`UpdateConfirm`] goes out on each heartbeat still showing `trial`,
//! and one with the new version and no `trial` is done.
//!
//! The prefix carries no boot counter, and a node on another minor has no layout this host
//! reads beyond it, so "not rebooted yet" is told from "rolled back" by time. The old image
//! heartbeats for a moment after `Verified`: the switch and reboot take milliseconds, but
//! heartbeats already queued at the bridge arrive after it. So an old-version heartbeat
//! counts as failure only [`REBOOT_GRACE`] after `Verified`, by which time the node has either
//! booted the new image or is still on the old one: a switch that failed, or a new image that
//! rolled back. Once a trial heartbeat has been heard, a reboot is proven, and an old-version
//! heartbeat after it is a rollback outright.
//!
//! **A heartbeat is the new image only once this session has sent its commit**, since nothing
//! installs before one. That check comes first in every phase, `Begin` included: a `Verified`
//! that is lost, then resent commits undelivered while the node reboots, return the session to its
//! `begin`, and the new image's first heartbeat must be confirmed, never answered with a
//! `begin`. The node would take that `begin` and stream the image again into the slot holding
//! the old one, destroying the copy a rollback returns to.
//!
//! After the commit, the image's version with `trial` set is the new image on trial: the old
//! image cannot be on trial, because the node confirms a trial image on the first update record
//! it decodes. Without `trial`, the version says it is the new image, confirmed, when it differs
//! from the old one. An image of the version the node already runs (`--force`) leaves the
//! version unable to tell, so done then needs a delivered [`UpdateConfirm`], and a heartbeat of
//! that version without `trial` gets another confirm until one is delivered. A rollback in that
//! case is indistinguishable from a confirmed image once the old image takes a confirm, which is
//! the cost of reinstalling one version.

use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wartui_proto::bus::{
    ACK_TIMEOUT_MS, Capabilities, DownType, Heartbeat, SEND_TRIES, Slot, UPDATE_CHUNK,
    UPDATE_CHUNK_MAX, UpType, UpdateAbort, UpdateBegin, UpdateChunk, UpdateCommit, UpdateConfirm,
    UpdateState, UpdateStatus, Version,
};
use wartui_proto::link::{HostToBridge, ToNodeBody};
use wartui_proto::mac::Mac;
use wartui_proto::ota::SECTOR;

use crate::engine::Now;

/// How long to wait for the status a sector's last chunk is owed, or for a commit's answer,
/// before resending. A sector write takes tens of milliseconds, so this is several of them.
pub const STATUS_TIMEOUT: Duration = Duration::from_millis(500);

/// Status timeouts in a row, about 10 s, after which the node is taken to have dropped the
/// session and the update returns to its `begin`.
pub const MAX_STATUS_TIMEOUTS: u32 = 20;

/// How long a sent record waits for its `Delivered` before it is treated as undelivered: a second
/// past the bridge's own give-up, so the bridge's answer comes first. A lost answer never comes.
pub const SEND_TIMEOUT: Duration =
    Duration::from_millis(ACK_TIMEOUT_MS * SEND_TRIES as u64 + 1_000);

/// Undelivered chunks or commits in a row before the node is taken to have rebooted or wandered off,
/// and the update returns to its `begin`.
pub const MAX_SEND_FAILURES: u32 = 10;

/// How long the `begin` is retried on heartbeats before the node is given up on: six heartbeats
/// of an assigned node, thirty of an unassigned one.
pub const BEGIN_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times an update may start over from its `begin` after losing the node, before
/// the node is given up on. The first `begin` is not counted.
pub const MAX_RESTARTS: u32 = 5;

/// How long after `Verified` a heartbeat from the old image is taken as one sent before the
/// reboot, rather than as the old image running again.
pub const REBOOT_GRACE: Duration = Duration::from_secs(5);

/// How long after `Verified` the new image has to heartbeat.
pub const REBOOT_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the new image has to be confirmed after its first heartbeat.
pub const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

/// Where an update stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Sending `begin` on each heartbeat until the node answers.
    Begin,
    /// Sending chunks.
    Streaming,
    /// Every byte is with the node; waiting for it to check the hash.
    Committing,
    /// The image verified; waiting for the node to boot it.
    AwaitReboot,
    /// The new image is running on trial; confirming it.
    AwaitConfirm,
    /// Confirmed on the new image.
    Done,
    /// Given up; see [`Failure`].
    Failed,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Begin => "waiting for the node",
            Self::Streaming => "sending",
            Self::Committing => "verifying",
            Self::AwaitReboot => "rebooting",
            Self::AwaitConfirm => "confirming",
            Self::Done => "done",
            Self::Failed => "failed",
        })
    }
}

/// Why an update failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// No status answered the `begin` within [`BEGIN_TIMEOUT`].
    NoAnswer,
    /// The node refused the `begin` with `FlashError`: it has no update slot or `trial` sector,
    /// or its flash failed.
    Refused,
    /// The node is a different board from the image's.
    WrongBoard,
    /// The image is larger than the node's update slot.
    TooLarge,
    /// Writing the node's flash failed.
    FlashError,
    /// Every byte arrived and the hash did not match.
    HashMismatch,
    /// The update started over [`MAX_RESTARTS`] times and lost the node again.
    NotAnswering,
    /// The image verified and the node could not switch slots to it.
    SwitchFailed,
    /// No heartbeat from the new image within [`REBOOT_TIMEOUT`].
    NoReboot,
    /// After [`REBOOT_GRACE`], the old image is the one heartbeating.
    StillOld(Version),
    /// The new image booted, and then the old one came back.
    RolledBack(Version),
    /// The new image ran without being confirmed for [`CONFIRM_TIMEOUT`].
    Unconfirmed,
    /// The node answered with a state this build has no name for.
    UnknownState(u8),
    /// The operator stopped it.
    Cancelled,
    /// The link to the bridge closed.
    LinkClosed,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAnswer => {
                write!(f, "no answer to the update request in {} s", BEGIN_TIMEOUT.as_secs())
            }
            Self::Refused => {
                f.write_str("the node refused the update: no update slot, or its flash failed")
            }
            Self::WrongBoard => f.write_str("the node is a different board from the image"),
            Self::TooLarge => f.write_str("the image is larger than the node's update slot"),
            Self::FlashError => f.write_str("the node could not write its flash"),
            Self::HashMismatch => f.write_str("the image arrived corrupted (hash mismatch)"),
            Self::NotAnswering => f.write_str("node not answering"),
            Self::SwitchFailed => f.write_str("the node could not switch to the new image"),
            Self::NoReboot => {
                write!(f, "no heartbeat from the new image within {} s", REBOOT_TIMEOUT.as_secs())
            }
            Self::StillOld(version) => {
                write!(f, "still running {version}: the new image rolled back or never booted")
            }
            Self::RolledBack(version) => write!(f, "rolled back to {version}"),
            Self::Unconfirmed => f.write_str(
                "the new image was never confirmed; the node rolls back by itself after 3 boots",
            ),
            Self::UnknownState(byte) => write!(f, "the node answered with unknown state {byte}"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::LinkClosed => f.write_str("the link to the bridge closed"),
        }
    }
}

/// How an update ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The node runs the new image, confirmed.
    Done(Version),
    /// It does not.
    Failed(Failure),
}

/// What [`OtaSession::handle`] reacts to.
#[derive(Debug, Clone, Copy)]
pub enum OtaEvent<'a> {
    /// The bridge's answer to a record the session sent.
    Delivery {
        /// The id the session gave the record.
        id: u16,
        /// `Delivered`, rather than `Undelivered`.
        delivered: bool,
    },
    /// A record a node sent. Only the session's own node is read.
    FromNode {
        /// The slot it came from.
        slot: Slot,
        /// Its type, a `bus::UpType` byte.
        kind: u8,
        /// Its body.
        body: &'a [u8],
    },
    /// Time passing, for timeouts. A few times a second is enough.
    Tick,
}

/// Where an update stands, for showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    /// The phase.
    pub phase: Phase,
    /// Bytes the node has reported holding.
    pub acked: u32,
    /// Bytes in the image.
    pub total: u32,
}

/// The image an update carries.
#[derive(Debug, Clone)]
pub struct OtaImage {
    /// The image bytes.
    pub bytes: Arc<[u8]>,
    /// Their SHA-256.
    pub sha256: [u8; 32],
    /// The firmware version the image carries.
    pub version: Version,
    /// The board it was built for.
    pub board: wartui_proto::bus::Board,
}

/// A record awaiting its answer.
#[derive(Debug, Clone, Copy)]
struct Pending {
    id: u16,
    sent: Instant,
    frame: Sent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sent {
    Begin,
    Chunk(u32),
    Commit,
    Confirm,
    Abort,
}

/// One node's update.
#[derive(Debug)]
pub struct OtaSession {
    node: Mac,
    /// The slot the node's latest heartbeat came from, which everything is sent to.
    slot: Option<Slot>,
    bytes: Arc<[u8]>,
    begin: UpdateBegin,
    /// The version the node ran when the update started.
    from: Version,
    phase: Phase,
    phase_since: Instant,
    next_id: u16,
    pending: Option<Pending>,
    /// The next offset to send.
    next: u32,
    /// The last `next_offset` the node reported.
    reported: u32,
    /// The highest `next_offset` the node has reported since its `begin`; a status below it is
    /// stale.
    high: u32,
    /// Whether the node has accepted this session, so a failure owes it an abort.
    accepted: bool,
    /// Whether a failure has an abort to send.
    abort_owed: bool,
    /// Whether this session has sent its commit, before which no heartbeat is the new image.
    committed: bool,
    /// When the status the host is waiting on is overdue.
    wait_status: Option<Instant>,
    send_failures: u32,
    status_timeouts: u32,
    restarts: u32,
    confirm_acked: bool,
    outcome: Option<Outcome>,
}

impl OtaSession {
    /// An update of `node`, running `from`, to `image`.
    ///
    /// `session` names it on the wire; the caller picks one a restarted host is unlikely to
    /// repeat. `first_id` is the first `ToNode` id it uses, so a driver running several
    /// in turn keeps late results of one from matching another ([`Self::next_id`]).
    #[must_use]
    pub fn new(
        node: Mac,
        from: Version,
        image: &OtaImage,
        session: u8,
        first_id: u16,
        now: Now,
    ) -> Self {
        // An image is at most an app slot, under 2 MB.
        let len = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        Self {
            node,
            slot: None,
            bytes: Arc::clone(&image.bytes),
            begin: UpdateBegin {
                session,
                len,
                sha256: image.sha256,
                version: image.version,
                board: image.board,
            },
            from,
            phase: Phase::Begin,
            phase_since: now.mono,
            next_id: first_id,
            pending: None,
            next: 0,
            reported: 0,
            high: 0,
            accepted: false,
            abort_owed: false,
            committed: false,
            wait_status: None,
            send_failures: 0,
            status_timeouts: 0,
            restarts: 0,
            confirm_acked: false,
            outcome: None,
        }
    }

    /// Confirm a node already running `version` on trial, as an update ends: an
    /// [`UpdateConfirm`] on each heartbeat until one without `trial` follows a delivered confirm.
    #[must_use]
    pub fn confirm_trial(node: Mac, version: Version, first_id: u16, now: Now) -> Self {
        let image = OtaImage {
            bytes: Arc::from(Vec::new()),
            sha256: [0; 32],
            version,
            board: wartui_proto::bus::Board::Unknown(0),
        };
        let mut session = Self::new(node, version, &image, 0, first_id, now);
        session.committed = true;
        session.phase = Phase::AwaitConfirm;
        session
    }

    /// React to `event`, returning what to send.
    pub fn handle(&mut self, now: Now, event: OtaEvent<'_>) -> Vec<HostToBridge> {
        let mut out = Vec::new();
        if self.outcome.is_some() {
            return out;
        }
        let now = now.mono;
        match event {
            OtaEvent::Delivery { id, delivered } => {
                self.on_delivery(now, id, delivered, &mut out);
            }
            OtaEvent::FromNode { slot, kind, body } => match UpType::from_u8(kind) {
                // A heartbeat of any layout opens with the prefix and the MAC.
                Some(UpType::Heartbeat) => {
                    if let Ok((caps, mac)) = Heartbeat::identity(body)
                        && mac == self.node
                    {
                        self.slot = Some(slot);
                        self.on_heartbeat(now, caps, &mut out);
                    }
                }
                Some(UpType::UpdateStatus) if self.slot == Some(slot) => {
                    if let Ok(status) = UpdateStatus::decode(body)
                        && status.session == self.begin.session
                    {
                        self.on_status(now, status, &mut out);
                    }
                }
                _ => {}
            },
            OtaEvent::Tick => self.on_tick(now, &mut out),
        }
        if self.abort_owed {
            self.abort_owed = false;
            let abort = UpdateAbort { session: self.begin.session }.encode();
            self.send(now, Sent::Abort, DownType::UpdateAbort, &abort, &mut out);
        }
        out
    }

    /// Stop the update, telling the node to drop the session.
    pub fn cancel(&mut self, now: Now) -> Vec<HostToBridge> {
        let mut out = Vec::new();
        if self.outcome.is_some() {
            return out;
        }
        if matches!(self.phase, Phase::Begin | Phase::Streaming | Phase::Committing) {
            let abort = UpdateAbort { session: self.begin.session }.encode();
            self.send(now.mono, Sent::Abort, DownType::UpdateAbort, &abort, &mut out);
        }
        self.fail(now.mono, Failure::Cancelled);
        out
    }

    /// Where the update stands.
    #[must_use]
    pub fn progress(&self) -> Progress {
        let acked = match self.phase {
            Phase::Begin | Phase::Streaming | Phase::Committing | Phase::Failed => self.reported,
            Phase::AwaitReboot | Phase::AwaitConfirm | Phase::Done => self.begin.len,
        };
        Progress { phase: self.phase, acked, total: self.begin.len }
    }

    /// How it ended, once it has.
    #[must_use]
    pub const fn outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }

    /// The node being updated.
    #[must_use]
    pub const fn node(&self) -> Mac {
        self.node
    }

    /// The first `ToNode` id this session has not used.
    #[must_use]
    pub const fn next_id(&self) -> u16 {
        self.next_id
    }

    /// What a heartbeat says of the new image: `Some(true)` for it on trial, `Some(false)` for it
    /// confirmed, `None` for anything else. The module docs give the rule.
    fn new_image(&self, caps: Capabilities) -> Option<bool> {
        if caps.version != self.begin.version || !self.committed {
            return None;
        }
        if caps.trial {
            return Some(true);
        }
        (self.begin.version != self.from || self.confirm_acked).then_some(false)
    }

    fn on_heartbeat(&mut self, now: Instant, caps: Capabilities, out: &mut Vec<HostToBridge>) {
        let new = self.begin.version;
        match (self.new_image(caps), self.phase) {
            (_, Phase::Done | Phase::Failed) => {}
            (Some(true), _) => {
                if self.phase != Phase::AwaitConfirm {
                    self.enter(now, Phase::AwaitConfirm);
                }
                self.send_confirm(now, out);
            }
            // Confirmed: by our confirm, or in `Committing` and `AwaitReboot` by the commits
            // resent after a lost `Verified`, which the new image heard.
            (Some(false), _) => self.finish(now, Outcome::Done(new)),
            (None, Phase::Begin) => {
                let begin = self.begin.encode();
                self.send(now, Sent::Begin, DownType::UpdateBegin, &begin, out);
            }
            (None, Phase::Streaming | Phase::Committing) => {}
            (None, Phase::AwaitReboot) => {
                if now.duration_since(self.phase_since) >= REBOOT_GRACE {
                    self.fail(now, Failure::StillOld(caps.version));
                }
            }
            // The image's version without `trial`, from a node whose old image carries it too,
            // with no confirm delivered: the confirm's delivery was lost, so it goes again.
            (None, Phase::AwaitConfirm) if caps.version == new && !caps.trial => {
                self.send_confirm(now, out);
            }
            (None, Phase::AwaitConfirm) => self.fail(now, Failure::RolledBack(caps.version)),
        }
    }

    fn on_status(&mut self, now: Instant, status: UpdateStatus, out: &mut Vec<HostToBridge>) {
        let len = self.begin.len;
        if matches!(status.state, UpdateState::Receiving | UpdateState::Verified) {
            self.accepted = true;
        }
        match (self.phase, status.state) {
            (Phase::AwaitReboot, UpdateState::FlashError) => self.fail(now, Failure::SwitchFailed),
            (Phase::AwaitReboot | Phase::AwaitConfirm | Phase::Done | Phase::Failed, _) => {}
            (Phase::Begin, UpdateState::Idle) => {}
            (Phase::Begin, UpdateState::FlashError) => self.fail(now, Failure::Refused),
            (_, UpdateState::WrongBoard) => self.fail(now, Failure::WrongBoard),
            (_, UpdateState::TooLarge) => self.fail(now, Failure::TooLarge),
            (_, UpdateState::FlashError) => self.fail(now, Failure::FlashError),
            (_, UpdateState::HashMismatch) => self.fail(now, Failure::HashMismatch),
            (_, UpdateState::Unknown(byte)) => self.fail(now, Failure::UnknownState(byte)),
            (_, UpdateState::Verified) => self.enter(now, Phase::AwaitReboot),
            (Phase::Streaming | Phase::Committing, UpdateState::Idle) => self.restart(now),
            (Phase::Begin, UpdateState::Receiving) => {
                // A `begin` answers from where the node stands, which a reboot sets back to 0.
                self.high = 0;
                self.stream_from(now, status.next_offset, out);
            }
            // Overtaken by a newer status.
            (Phase::Streaming | Phase::Committing, UpdateState::Receiving)
                if status.next_offset < self.high => {}
            // The last sector's own status, overtaken by the commit.
            (Phase::Committing, UpdateState::Receiving) if status.next_offset >= len => {}
            (Phase::Committing, UpdateState::Receiving) => {
                self.stream_from(now, status.next_offset, out);
            }
            (Phase::Streaming, UpdateState::Receiving) => {
                self.reported = status.next_offset.min(len);
                self.high = self.reported;
                self.next = self.reported;
                self.wait_status = None;
                self.status_timeouts = 0;
                self.send_next(now, out);
            }
        }
    }

    fn on_delivery(&mut self, now: Instant, id: u16, delivered: bool, out: &mut Vec<HostToBridge>) {
        let Some(pending) = self.pending.filter(|p| p.id == id) else { return };
        self.pending = None;
        if delivered {
            self.on_acked(now, pending.frame, out);
        } else {
            self.on_unacked(now, pending.frame, out);
        }
    }

    fn on_acked(&mut self, now: Instant, frame: Sent, out: &mut Vec<HostToBridge>) {
        match frame {
            Sent::Chunk(offset) if self.phase == Phase::Streaming => {
                self.send_failures = 0;
                // A status may have moved the host while the chunk was in flight; then it is
                // the status's offset that counts.
                if offset == self.next {
                    let end = offset + self.chunk_len(offset);
                    self.next = end;
                    if end.is_multiple_of(SECTOR as u32) && end < self.begin.len {
                        self.wait_status = Some(now + STATUS_TIMEOUT);
                    }
                }
                self.send_next(now, out);
            }
            Sent::Commit => self.send_failures = 0,
            Sent::Confirm => self.confirm_acked = true,
            Sent::Begin | Sent::Chunk(_) | Sent::Abort => {}
        }
    }

    fn on_unacked(&mut self, now: Instant, frame: Sent, out: &mut Vec<HostToBridge>) {
        match frame {
            Sent::Chunk(_) if self.phase == Phase::Streaming => {
                if self.note_send_failure(now) {
                    self.send_next(now, out);
                }
            }
            Sent::Commit if self.phase == Phase::Committing => {
                if self.note_send_failure(now) {
                    self.send_commit(now, out);
                }
            }
            // The next heartbeat resends these.
            Sent::Begin | Sent::Chunk(_) | Sent::Commit | Sent::Confirm | Sent::Abort => {}
        }
    }

    /// Count an undelivered send, returning to the `begin` past [`MAX_SEND_FAILURES`]. Whether to
    /// retry.
    fn note_send_failure(&mut self, now: Instant) -> bool {
        self.send_failures += 1;
        if self.send_failures > MAX_SEND_FAILURES {
            self.restart(now);
            return false;
        }
        true
    }

    fn on_tick(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        if let Some(pending) = self.pending
            && now.duration_since(pending.sent) >= SEND_TIMEOUT
        {
            self.pending = None;
            self.on_unacked(now, pending.frame, out);
        }
        let elapsed = now.duration_since(self.phase_since);
        match self.phase {
            Phase::Begin if elapsed >= BEGIN_TIMEOUT => self.fail(now, Failure::NoAnswer),
            Phase::Streaming | Phase::Committing => {
                if !self.wait_status.is_some_and(|deadline| now >= deadline) {
                    return;
                }
                self.wait_status = None;
                self.status_timeouts += 1;
                if self.status_timeouts > MAX_STATUS_TIMEOUTS {
                    self.restart(now);
                } else if self.phase == Phase::Committing {
                    self.send_commit(now, out);
                } else {
                    self.next = self.reported;
                    self.send_next(now, out);
                }
            }
            Phase::AwaitReboot if elapsed >= REBOOT_TIMEOUT => self.fail(now, Failure::NoReboot),
            Phase::AwaitConfirm if elapsed >= CONFIRM_TIMEOUT => {
                self.fail(now, Failure::Unconfirmed);
            }
            _ => {}
        }
    }

    /// Start streaming at `offset`, where the node says it stands.
    fn stream_from(&mut self, now: Instant, offset: u32, out: &mut Vec<HostToBridge>) {
        self.enter(now, Phase::Streaming);
        self.reported = offset.min(self.begin.len);
        self.high = self.reported;
        self.next = self.reported;
        self.send_next(now, out);
    }

    /// Send the chunk at `next`, or the commit after the last one, unless a chunk is in flight
    /// or a sector's status is awaited.
    fn send_next(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        if self.phase != Phase::Streaming || self.wait_status.is_some() {
            return;
        }
        if self.pending.is_some_and(|p| matches!(p.frame, Sent::Chunk(_))) {
            return;
        }
        if self.next >= self.begin.len {
            self.enter(now, Phase::Committing);
            self.send_commit(now, out);
            return;
        }
        let offset = self.next;
        let start = offset as usize;
        let data = &self.bytes[start..start + self.chunk_len(offset) as usize];
        let msg = UpdateChunk { session: self.begin.session, offset, data };
        let mut buf = [0u8; UPDATE_CHUNK_MAX];
        let Some(n) = msg.encode_into(&mut buf) else { return };
        self.send(now, Sent::Chunk(offset), DownType::UpdateChunk, &buf[..n], out);
    }

    fn send_commit(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        let commit = UpdateCommit { session: self.begin.session }.encode();
        self.committed = true;
        self.send(now, Sent::Commit, DownType::UpdateCommit, &commit, out);
        self.wait_status = Some(now + STATUS_TIMEOUT);
    }

    fn send_confirm(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        self.send(now, Sent::Confirm, DownType::UpdateConfirm, &UpdateConfirm.encode(), out);
    }

    /// Send one record to the node's slot. Nothing goes before a heartbeat has named the slot,
    /// and every phase that sends begins on one.
    fn send(
        &mut self,
        now: Instant,
        frame: Sent,
        kind: DownType,
        bytes: &[u8],
        out: &mut Vec<HostToBridge>,
    ) {
        let Some(slot) = self.slot else { return };
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending = Some(Pending { id, sent: now, frame });
        // Every update body fits `MAX_NODE_DOWN` (`bus::update`'s assertions).
        let body = ToNodeBody::from_slice(bytes).unwrap_or_default();
        out.push(HostToBridge::ToNode { slot, id, kind: kind.as_u8(), body });
    }

    /// Bytes in the chunk at `offset`: [`UPDATE_CHUNK`], or what is left of the image.
    fn chunk_len(&self, offset: u32) -> u32 {
        // `UPDATE_CHUNK` is 1024.
        #[allow(clippy::cast_possible_truncation)]
        let full = UPDATE_CHUNK as u32;
        full.min(self.begin.len - offset)
    }

    /// Start over from the `begin`, which goes out on the node's next heartbeat.
    fn restart(&mut self, now: Instant) {
        if self.restarts == MAX_RESTARTS {
            self.fail(now, Failure::NotAnswering);
            return;
        }
        self.restarts += 1;
        self.enter(now, Phase::Begin);
    }

    fn enter(&mut self, now: Instant, phase: Phase) {
        self.phase = phase;
        self.phase_since = now;
        self.pending = None;
        self.wait_status = None;
        self.send_failures = 0;
        self.status_timeouts = 0;
    }

    fn finish(&mut self, now: Instant, outcome: Outcome) {
        let phase = if matches!(outcome, Outcome::Done(_)) { Phase::Done } else { Phase::Failed };
        self.enter(now, phase);
        self.outcome = Some(outcome);
    }

    /// End the update, owing the node an abort when it holds the session in update mode. A
    /// cancel sends its own.
    fn fail(&mut self, now: Instant, failure: Failure) {
        self.abort_owed = self.accepted
            && failure != Failure::Cancelled
            && matches!(self.phase, Phase::Begin | Phase::Streaming | Phase::Committing);
        self.finish(now, Outcome::Failed(failure));
    }
}
