//! The host's half of a node's update: a state machine that takes one node from its running
//! image to a new one, confirmed. [`wartui_proto::ota::Receiver`] is the node's half.
//!
//! Like [`crate::engine`], it reads no clock and does no I/O. Time arrives as [`Now`], and
//! what it wants sent comes back as [`HostToBridge`] commands for a driver
//! (`runtime::drive_fleet`) to queue. Several sessions share one link, each giving its records
//! ids no other uses ([`Ids`]) and reading only its own node's records.
//!
//! **The `begin` follows the node's heartbeats**, which name the node-bus slot the node sits in.
//! [`UpdateBegin`] goes out on each heartbeat from the node until a status for this session
//! answers it. A session that knows the slot its node was last heard in
//! ([`OtaSession::with_slot`]) sends its first `begin` there at once rather than waiting out a
//! heartbeat; undelivered, it waits for the next heartbeat, which names where the node is now. An
//! accepted `begin` puts the node in update mode, where it services the bus continuously, and
//! everything after it goes out as soon as it is due.
//!
//! **Chunks go by reference.** The image is uploaded once into the P4's cache, and each chunk is
//! a [`HostToBridge::ToNodeChunk`] naming the image by its tag, an offset and a length, from
//! which the P4 builds the record. So the host link carries an image once however many nodes
//! take it. The upload runs beside the `begin`s, and no chunk goes before
//! [`OtaEvent::Cached`] says the P4 has checked the image's digest: a chunk from an image not yet
//! cached is refused, and one from a corrupt copy would fail every node at once.
//!
//! **[`CHUNKS_IN_FLIGHT`] chunks are out at once**, so the next waits at the P4 while the node
//! acknowledges the one before, and each `Delivered` or `Undelivered` sends one more. The P4's
//! queue for a slot holds two with room. A chunk that starts a sector goes only once the node has
//! reported the sectors before it written, so nothing is in flight while the node flushes one and
//! at most two chunks reach its inbox, which holds two. An undelivered chunk sends from its offset
//! again.
//!
//! **Flow control is the node's `next_offset`, not the bus's ack.** Delivery says the node took
//! the transfer, not that its receiver took the chunk: a resent chunk arrives twice, and one past
//! an undelivered chunk arrives at the wrong offset. The node reports a status on each 4 KB sector
//! it writes, so after the chunk that completes a sector the host waits for one. A status moves
//! the host forward, never back. On the node bus, delivery is in order: the P4's queue for a slot
//! is FIFO and stop-and-wait, and a node whose inbox is full leaves the transfer unacknowledged, so
//! it is resent rather than lost. So only an undelivered chunk or the timeout says a chunk was
//! lost. A status naming an offset behind what was sent is the node's answer to a duplicate,
//! written before the chunks after it arrived, and going back on it would resend chunks the node
//! holds, each drawing another such status. The commit's answer is the exception: one naming bytes
//! still missing streams from there.
//! With no status within [`STATUS_TIMEOUT`] of nothing left to send, the host resends from the
//! last offset the node reported: a node that has those bytes answers the duplicate with its
//! status, and one that lacks them takes them. One rule recovers lost chunks and lost statuses
//! alike. `Idle` means the node dropped the session (30 s of silence, or a reboot), and so
//! does [`MAX_STATUS_TIMEOUTS`] unanswered in a row. So does [`MAX_SEND_FAILURES`] undelivered
//! sends with no progress reported between them: a node rebooting acknowledges nothing for a
//! second or so. Each returns to the `begin`, which an identical one resumes where the node's
//! progress stands: from 0 after a reboot, which loses the receiver's state.
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
    Capabilities, DownType, Heartbeat, Slot, UPDATE_CHUNK, UpType, UpdateAbort, UpdateBegin,
    UpdateCommit, UpdateConfirm, UpdateState, UpdateStatus, Version,
};
use wartui_proto::link::{HostToBridge, ToNodeBody, image_tag};
use wartui_proto::mac::Mac;
use wartui_proto::ota::SECTOR;

use crate::engine::{Now, SEND_TIMEOUT};

/// How long to wait for the status a sector's last chunk is owed, or for a commit's answer,
/// before resending. A sector write takes tens of milliseconds, so this is several of them.
pub const STATUS_TIMEOUT: Duration = Duration::from_millis(500);

/// Status timeouts in a row, about 10 s, after which the node is taken to have dropped the
/// session and the update returns to its `begin`.
pub const MAX_STATUS_TIMEOUTS: u32 = 20;

/// Undelivered chunks or commits, with no progress reported between them, before the node is taken
/// to have rebooted or wandered off, and the update returns to its `begin`.
pub const MAX_SEND_FAILURES: u32 = 10;

/// Chunks a session has out at once, awaiting their delivery answers. The P4's queue for a
/// slot holds two full chunks with room, and the node's inbox holds two.
pub const CHUNKS_IN_FLIGHT: usize = 2;

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
    /// The P4's cache holds the image, checked, so chunks may go.
    Cached,
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

/// The ids a session gives its records: `next`, then every `step` after it, wrapping.
///
/// Sessions sharing a link step past each other's ids, so a delivery answer matches only the
/// session that sent it: session `index` of `count` takes [`Ids::of`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    /// The id the next record takes.
    pub next: u16,
    /// What each record adds to it.
    pub step: u16,
}

impl Ids {
    /// The ids of session `index` of `count` sharing a link: `index + 1`, stepping by `count`.
    #[must_use]
    pub fn of(index: usize, count: usize) -> Self {
        // A link carries at most a bus's fourteen nodes.
        #[allow(clippy::cast_possible_truncation)]
        Self { next: index as u16 + 1, step: count.max(1) as u16 }
    }

    fn take(&mut self) -> u16 {
        let id = self.next;
        self.next = self.next.wrapping_add(self.step);
        id
    }
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
    /// The image's [`image_tag`], which names it in the P4's cache.
    tag: u32,
    begin: UpdateBegin,
    /// The version the node ran when the update started.
    from: Version,
    phase: Phase,
    phase_since: Instant,
    ids: Ids,
    /// Records awaiting their answers: up to [`CHUNKS_IN_FLIGHT`] chunks, and the last of any
    /// other kind.
    pending: Vec<Pending>,
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
    /// Whether the P4's cache holds the image, before which no chunk goes.
    cached: bool,
    /// Whether a `begin` goes to `slot` on the next tick, before any heartbeat.
    begin_due: bool,
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
    /// repeat. `ids` are the ids it gives its records, which no other session on the link
    /// shares. Its chunks wait for [`OtaEvent::Cached`].
    #[must_use]
    pub fn new(
        node: Mac,
        from: Version,
        image: &OtaImage,
        session: u8,
        ids: Ids,
        now: Now,
    ) -> Self {
        // An image is at most an app slot, under 2 MB.
        let len = u32::try_from(image.bytes.len()).unwrap_or(u32::MAX);
        Self {
            node,
            slot: None,
            tag: image_tag(&image.sha256),
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
            ids,
            pending: Vec::new(),
            next: 0,
            reported: 0,
            high: 0,
            accepted: false,
            abort_owed: false,
            committed: false,
            cached: false,
            begin_due: false,
            wait_status: None,
            send_failures: 0,
            status_timeouts: 0,
            restarts: 0,
            confirm_acked: false,
            outcome: None,
        }
    }

    /// The node was last heard in `slot`, so the first `begin` goes there on the next
    /// [`OtaEvent::Tick`] rather than on the node's next heartbeat.
    #[must_use]
    pub const fn with_slot(mut self, slot: Slot) -> Self {
        self.slot = Some(slot);
        self.begin_due = true;
        self
    }

    /// Confirm a node already running `version` on trial, as an update ends: an
    /// [`UpdateConfirm`] on each heartbeat until one without `trial` follows a delivered confirm.
    #[must_use]
    pub fn confirm_trial(node: Mac, version: Version, ids: Ids, now: Now) -> Self {
        let image = OtaImage {
            bytes: Arc::from(Vec::new()),
            sha256: [0; 32],
            version,
            board: wartui_proto::bus::Board::Unknown(0),
        };
        let mut session = Self::new(node, version, &image, 0, ids, now);
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
            OtaEvent::Cached => {
                self.cached = true;
                self.send_next(now, &mut out);
            }
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
            (None, Phase::Begin) => self.send_begin(now, out),
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
                self.on_progress(now, status.next_offset.min(len), out);
            }
        }
    }

    /// The node holds everything before `offset`, by a status no older than the last. Sending
    /// moves forward to it, never back: the module docs give the rule.
    fn on_progress(&mut self, now: Instant, offset: u32, out: &mut Vec<HostToBridge>) {
        // No further than the last: the answer to a duplicate.
        if offset <= self.reported {
            return;
        }
        self.send_failures = 0;
        self.reported = offset;
        self.high = offset;
        self.wait_status = None;
        self.status_timeouts = 0;
        self.next = self.next.max(offset);
        self.send_next(now, out);
    }

    fn on_delivery(&mut self, now: Instant, id: u16, delivered: bool, out: &mut Vec<HostToBridge>) {
        let Some(at) = self.pending.iter().position(|p| p.id == id) else { return };
        let pending = self.pending.remove(at);
        if delivered {
            self.on_acked(now, pending.frame, out);
        } else {
            self.on_unacked(now, pending.frame, out);
        }
    }

    fn on_acked(&mut self, now: Instant, frame: Sent, out: &mut Vec<HostToBridge>) {
        match frame {
            // Delivery is not progress: a chunk delivered past a lost one is dropped by the node.
            Sent::Chunk(_) if self.phase == Phase::Streaming => self.send_next(now, out),
            Sent::Commit => self.send_failures = 0,
            Sent::Confirm => self.confirm_acked = true,
            Sent::Begin | Sent::Chunk(_) | Sent::Abort => {}
        }
    }

    fn on_unacked(&mut self, now: Instant, frame: Sent, out: &mut Vec<HostToBridge>) {
        match frame {
            Sent::Chunk(offset) if self.phase == Phase::Streaming => {
                if self.note_send_failure(now) {
                    // Unless a status has since said the node holds it.
                    if offset >= self.reported && offset < self.next {
                        self.next = offset;
                    }
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
        while let Some(at) =
            self.pending.iter().position(|p| now.duration_since(p.sent) >= SEND_TIMEOUT)
        {
            let pending = self.pending.remove(at);
            self.on_unacked(now, pending.frame, out);
        }
        let elapsed = now.duration_since(self.phase_since);
        match self.phase {
            Phase::Begin if elapsed >= BEGIN_TIMEOUT => self.fail(now, Failure::NoAnswer),
            Phase::Begin if self.begin_due => self.send_begin(now, out),
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
                    // The silence outlasted anything in flight.
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

    /// Send chunks from `next` until [`CHUNKS_IN_FLIGHT`] are out or the next starts a sector
    /// the node has not reached, or the commit once the last chunk is delivered. With nothing out
    /// and nothing to send, a status is awaited, for [`STATUS_TIMEOUT`]. Nothing goes, and nothing
    /// is awaited, until the image is cached.
    fn send_next(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        if self.phase != Phase::Streaming || !self.cached {
            return;
        }
        while self.chunks_out() < CHUNKS_IN_FLIGHT {
            let offset = self.next;
            if offset >= self.begin.len {
                if self.chunks_out() == 0 {
                    self.enter(now, Phase::Committing);
                    self.send_commit(now, out);
                }
                return;
            }
            if offset.is_multiple_of(SECTOR as u32) && self.reported < offset {
                break;
            }
            let Some(slot) = self.slot else { return };
            let len = self.chunk_len(offset);
            let id = self.ids.take();
            self.pending.push(Pending { id, sent: now, frame: Sent::Chunk(offset) });
            // `chunk_len` is at most `UPDATE_CHUNK`, 1024.
            #[allow(clippy::cast_possible_truncation)]
            let len16 = len as u16;
            let (image, session) = (self.tag, self.begin.session);
            out.push(HostToBridge::ToNodeChunk { slot, id, image, session, offset, len: len16 });
            self.next = offset + len;
        }
        if self.chunks_out() == 0 && self.wait_status.is_none() {
            self.wait_status = Some(now + STATUS_TIMEOUT);
        }
    }

    fn chunks_out(&self) -> usize {
        self.pending.iter().filter(|p| matches!(p.frame, Sent::Chunk(_))).count()
    }

    fn send_begin(&mut self, now: Instant, out: &mut Vec<HostToBridge>) {
        self.begin_due = false;
        let begin = self.begin.encode();
        self.send(now, Sent::Begin, DownType::UpdateBegin, &begin, out);
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

    /// Send one record other than a chunk to the node's slot, which takes the place of the last
    /// such record awaiting its answer. Nothing goes before a heartbeat or [`Self::with_slot`]
    /// has named the slot.
    fn send(
        &mut self,
        now: Instant,
        frame: Sent,
        kind: DownType,
        bytes: &[u8],
        out: &mut Vec<HostToBridge>,
    ) {
        let Some(slot) = self.slot else { return };
        let id = self.ids.take();
        self.pending.retain(|p| matches!(p.frame, Sent::Chunk(_)));
        self.pending.push(Pending { id, sent: now, frame });
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
        self.pending.clear();
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
