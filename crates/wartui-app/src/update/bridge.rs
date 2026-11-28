//! The P4 bridge's update, sent over the link the host already reaches it over.
//!
//! **The link carries the update.** Over USB the board's Type-C reaches the P4's UART0 through a
//! CH343, the port the first flash also uses (`firmware/p4-bridge/README.md` § "First flash");
//! over Bluetooth the C6 forwards the host's frames to the P4. After the first flash the P4 takes
//! its app image as `HostToBridge::Update` ops: the `radio_link::update` layout it sends its own
//! C6, driven by the same `Sender`, on a `TICK`, on each status and as the link writes what it
//! queued. Host bytes the P4 loses while it writes a sector cost nothing: the sender waits for
//! each sector's status and rewinds on what it says. The sender is paced to the link
//! (`wartui_bridge::pace`), so at most a sector's ops are queued, and a full queue is an error.
//!
//! **A stop is taken only while the image is sent.** It ends the bridge's session with `Abort`,
//! waits for the `Idle` that answers it (`pace::abort`), and ends [`Failed::Stopped`]; the bridge
//! keeps its image. From `Install` on the bridge boots the new image on trial, and only the
//! host's frames confirm it, so the update runs to its outcome whatever the stop does.
//!
//! **A `Ready` is not always the P4 rebooting.** The P4 announces a new life when only its C6
//! restarts, since `Ready.uptime_ms` is the C6's. While sending, a `Ready` naming the image the
//! P4 ran before is that, and the send goes on: a P4 that did reboot dropped the session, which
//! the sender notices. One naming another image means the P4 is no longer running the image the
//! send began against, so the send stops. After `Install`, the next new life is the P4's
//! (`on_ready`): over USB its CH343 stays on the bus, so the connection holds through the
//! reboot, and the transport reports the new life by its uptime falling.
//!
//! **`Ready`'s frozen core names the outcome** ([`verdict`]). The P4 installs and reboots into
//! the new image on trial, which writes nothing until a host frame arrives. So from the install
//! on, the host sends an `Identify` every `POLL`, and reads each answer: the transport reports a
//! new life as `Connected` and another answer from the same life as `Again`. The first host frame
//! the P4 decodes once its C6 has answered confirms the image, and `Ready.trial` says how that
//! went:
//!
//! | `image` | `trial` | Outcome |
//! |---|---|---|
//! | the new digest | settled | installed and confirmed |
//! | the new digest | on trial, or a code this build cannot name | not yet: ask again |
//! | the new digest | the confirm failed | not confirmed: it rolls back unless a host confirms it |
//! | the old digest, in a new life | any | rolled back |
//! | the old digest, in the same life | any | not yet: the bridge has not rebooted |
//! | any other digest | any | another image |
//!
//! The P4's trial watchdog resets a hung image twice, and the third boot returns it to the old
//! one, which is why the wait is `COME_BACK`. The bootloader's log after each reset arrives as
//! garbage, which a held connection passes over.
//!
//! **Every `Identify` repeats the P4's announcement**: its faults, panics and the carrier's, the
//! cache's and the fleet's lines. So from `Install` on, when the polls start, each distinct line
//! is said once per bridge life, and a new life says them again. Before it no poll runs, and every
//! line is said.
//!
//! **Then the C6.** A new P4 image carries its C6's image, which the P4 sends the C6 at boot.
//! Its progress arrives as error lines, handed on until the C6's new life, or until `QUIET`
//! passes without one.
//!
//! **Over Bluetooth the link drops at each restart, and the outcome is read across the
//! reconnects** (`Settling`). The P4 resets its C6 at boot, so its reboot ends the connection,
//! and the C6's own update ends it again; the transport reconnects on its own each time. Before
//! `Begin` the scan is stopped, as the node update stops it, so the stream toward the host leaves
//! the C6's small packet pool to the writes; the P4 boots stopped anyway. A drop while the image
//! is sent ends the update, since the P4 drops its update session when its host goes, and the
//! bridge keeps its image. After `Install`:
//!
//! - A `Ready` whose C6 is older than the install is the P4's life before it: the install never
//!   reached the P4, which dropped its session with the link. The update ends at once. A C6 that
//!   restarts alone at that moment comes back younger than the install, and the update reads that
//!   as a rollback.
//! - Any other `Ready` is a life after the install, and the update is judged on the latest
//!   answer's image and trial, so trial boots ending on the old image read as a rollback. Each
//!   reconnect is answered with an `Identify` at once. The sender is pumped, `Install` again, only
//!   while the link is up and no new life has answered.
//! - The update ends once the image is confirmed and the link has stayed up for `QUIET` with no
//!   new life and no line about the C6's update. Neither `Ready` nor `RadioInfo` names the C6's
//!   image, so that quiet is the C6's outcome. An answer from the same life is no news: the polls
//!   bring one every second.
//! - It waits `COME_BACK_BLUETOOTH` for a new life, then `SETTLE_BLUETOOTH` after the first
//!   one, long enough for three trial boots to end in a rollback.
//!
//! A failed update over Bluetooth is recovered over USB, by updating again there; nothing resumes
//! a partly sent image.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use wartui_bridge::pace::{self, PacedSender};
use wartui_bridge::{BridgeCore, LinkEvent, LinkHandle, SendError};
use wartui_core::runtime::stop_scan;
use wartui_proto::bus::UpdateStatus;
use wartui_proto::link::{BridgeToHost, HostToBridge, RADIO_UPDATING, Trial};
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::radio_link::update::{Failure, Op, Sender, Stage};

use super::image::BridgeImage;
use crate::settings::Link;

/// How often the sender is asked for its next op, besides on each status.
const TICK: Duration = Duration::from_millis(50);

/// How often the host sends an `Identify` from the install on: a rebooted P4 writes nothing
/// until a host frame arrives, its gate closes after 15 s without one, and each answer is a
/// reading of the outcome.
const POLL: Duration = Duration::from_secs(1);

/// How long to wait for the P4 to come back after the install: two trial boots that the 30 s
/// watchdog ends and the third that rolls back, with room for each boot.
const COME_BACK: Duration = Duration::from_secs(120);

/// How long after the new image's first `Ready` to ask again while it reads on trial. The P4
/// confirms on the first host frame it decodes once its C6 has answered, so the next answer
/// after that says how the confirm went.
const CONFIRM_WAIT: Duration = Duration::from_secs(3);

/// The longest the C6's update is followed after the P4 comes back.
const FOLLOW: Duration = Duration::from_secs(30);

/// How long without a line about the C6's update before it is taken as done, or as not needed.
const QUIET: Duration = Duration::from_secs(5);

/// How long to wait over Bluetooth for the P4 to come back after the install: [`COME_BACK`]'s
/// boots, each also waiting out the drop the central notices at its supervision timeout and the
/// reconnect.
const COME_BACK_BLUETOOTH: Duration = Duration::from_secs(180);

/// How long an update over Bluetooth is followed once a new life answers, unless it settles
/// sooner: two trial boots that the P4's 30 s watchdog ends and the third that rolls back, each
/// with its drop and reconnect, so a rollback is read from the latest `Ready` rather than
/// reported as unconfirmed.
const SETTLE_BLUETOOTH: Duration = Duration::from_secs(150);

/// How long a Bluetooth update waits for the bridge to report its scan stopped before sending
/// anyway.
const SCAN_STOP_WAIT: Duration = Duration::from_secs(2);

/// What an update reports as it goes, in the order it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Over Bluetooth, the bridge did not report its scan stopped; the update goes on.
    ScanNotStopped,
    /// The share of the image the bridge has reported holding. Said on every turn of the send,
    /// not only when it changes.
    Sending { percent: u8 },
    /// `Install` is sent; the bridge reboots.
    Installing,
    /// Over Bluetooth, the link dropped as the bridge restarts. Said once.
    LinkDropped,
    /// The new image is running, and its `Ready` says it is confirmed.
    Confirmed,
    /// The radio restarted on its bundled image.
    RadioRestarted,
    /// The radio's update is still running when the wait for it ends.
    RadioStillUpdating,
    /// A line the bridge sent meanwhile; from `Installing` on, once per bridge life.
    Line(Line),
}

/// A line the bridge sent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Line {
    /// An `Error` message.
    Fault(String),
    /// A `Log` message.
    Log(String),
}

/// How an update that did not fail ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Updated {
    /// The bridge already runs the image; nothing was sent.
    AlreadyCurrent,
    /// The bridge runs the image, confirmed.
    Installed,
}

/// Why an update failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failed {
    /// The link dropped while the image was sent, with the transport's reason.
    #[error("the link dropped before the update finished: {reason}")]
    Dropped { reason: String },
    /// The transport ended.
    #[error("the link closed")]
    Closed,
    /// While sending, the bridge restarted into another image than the send began against.
    #[error("the bridge restarted into another image while sending")]
    RestartedIntoOther,
    /// The link stopped writing the image at `percent`: a port that stopped draining, not a
    /// receiver that stopped answering.
    #[error("the port stopped taking the image at {percent}%")]
    PortStalled { percent: u8 },
    /// The sender gave up, and whether the bridge's `Ready` named an image before the update,
    /// which says its firmware takes updates.
    #[error("the sender failed: {failure:?}")]
    Sender { failure: Failure, image_known: bool },
    /// An op could not be queued.
    #[error("queueing an update op: {0}")]
    Queue(SendError),
    /// The caller stopped the update while the image was sent. Its session was aborted, and the
    /// bridge keeps its image.
    #[error("stopped before the install")]
    Stopped,
    /// No new life answered within `secs` of the install, over `over`.
    #[error("the bridge did not come back within {secs} s over {over:?}")]
    NoComeBack { over: Link, secs: u64 },
    /// Over Bluetooth, a `Ready` from the life before the install: the install never reached it.
    #[error("the bridge never took the install")]
    NeverTookInstall,
    /// The bridge came back on the image it ran before.
    #[error("rolled back")]
    RolledBack,
    /// The bridge came back on neither image, or one it could not name.
    #[error("came back running another image")]
    OtherImage,
    /// The bridge runs the new image, and its `Ready` says confirming it failed. The bridge's
    /// own line saying why reaches the caller as a [`Progress::Line`].
    #[error("installed, not confirmed")]
    NotConfirmed,
    /// The bridge runs the new image, still on trial: `secs` after its first `Ready` over USB,
    /// or when the wait over Bluetooth ended (`None`).
    #[error("installed, still on trial")]
    NoConfirm { secs: Option<u64> },
    /// Over Bluetooth, the wait ended with no new life judged.
    #[error("the bridge did not come back")]
    NotBack,
}

/// Update the bridge on `link`, reached over `over`, to `image`. `running` is the image the
/// bridge's `Ready` named (`BridgeInfo::image`). Opening the link and waiting for that `Ready`
/// are the caller's. `stop` resolves when the caller wants the update stopped, which is taken
/// only while the image is sent (the module's `//!`). `say` is `Send`, so a phone's binding runs
/// the update as a task of its own runtime.
///
/// # Errors
/// How the update failed.
pub async fn update(
    link: &mut LinkHandle,
    over: Link,
    running: Option<[u8; DIGEST_LEN]>,
    image: &BridgeImage,
    stop: impl Future<Output = ()> + Send,
    mut say: impl FnMut(Progress) + Send,
) -> Result<Updated, Failed> {
    let say: &mut (dyn FnMut(Progress) + Send) = &mut say;
    let digest = image.digest();
    if running == Some(digest) {
        return Ok(Updated::AlreadyCurrent);
    }
    let bluetooth = over == Link::Bluetooth;
    if bluetooth && !stop_scan(link, SCAN_STOP_WAIT).await {
        say(Progress::ScanNotStopped);
    }

    let mut update = Update::new(image.bytes(), image.sha256(), &digest, running);
    update.send(link, stop, say).await?;
    say(Progress::Installing);
    // From here the polls make the P4 repeat its announcement.
    update.said.once_per_life();
    if bluetooth {
        update.settle_bluetooth(link, say).await?;
        return Ok(Updated::Installed);
    }
    let back = update.come_back(link, say).await?;
    update.confirm(link, back, say).await?;
    say(Progress::Confirmed);
    update.follow_radio(link, say).await;
    Ok(Updated::Installed)
}

/// What one `Ready` says of the update after `Install` ([`verdict`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The new image, confirmed.
    Installed,
    /// The new image, still on trial: the host's frame confirms it, so ask again.
    OnTrial,
    /// The new image, and confirming it failed.
    ConfirmFailed,
    /// The image the bridge ran before, in a life after the install.
    RolledBack,
    /// The image the bridge ran before, in the life the install found: it has not rebooted.
    NotYet,
    /// Neither, or none it could name.
    Other,
}

/// Read a `Ready`'s `core` after the install of `sent`, against the image the bridge named
/// before it, if it named one. `new_life` is whether the `Ready` began a life the install did not
/// find.
#[must_use]
pub fn verdict(
    sent: &[u8; DIGEST_LEN],
    before: Option<[u8; DIGEST_LEN]>,
    core: &BridgeCore,
    new_life: bool,
) -> Verdict {
    match core.image {
        Some(image) if image == *sent => match core.trial {
            Trial::Settled => Verdict::Installed,
            Trial::ConfirmFailed => Verdict::ConfirmFailed,
            // A code this build cannot name is read as on trial (`wartui_proto::link`).
            Trial::OnTrial | Trial::Other(_) => Verdict::OnTrial,
        },
        Some(image) if Some(image) == before && new_life => Verdict::RolledBack,
        Some(image) if Some(image) == before => Verdict::NotYet,
        _ => Verdict::Other,
    }
}

/// Where the update stands when a `Ready` arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Sending the image.
    Sending,
    /// `Install` sent.
    Installing,
}

/// What a `Ready` means to the update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OnReady {
    /// The C6 restarted under the same P4 life; carry on.
    Ignore,
    /// The P4 runs another image than the send began against; stop.
    Abort,
    /// The life after the install.
    Back,
}

/// Read a `Ready` naming `image` in `phase`, against the image the P4 named before the update.
fn on_ready(
    phase: Phase,
    before: Option<[u8; DIGEST_LEN]>,
    image: Option<[u8; DIGEST_LEN]>,
) -> OnReady {
    match phase {
        Phase::Sending if image == before => OnReady::Ignore,
        Phase::Sending => OnReady::Abort,
        // The transport reports a new life only when the uptime falls, and the P4 reboots on
        // the `Install` it accepts, so this is its life after the install.
        Phase::Installing => OnReady::Back,
    }
}

/// One image on its way to the P4.
struct Update<'a> {
    sender: PacedSender<'a>,
    start: Instant,
    /// The digest the image carries, which its `Ready` names once installed.
    sent: [u8; DIGEST_LEN],
    /// The image the P4's `Ready` named before the update, if it named one. Naming one says
    /// its firmware takes updates.
    before: Option<[u8; DIGEST_LEN]>,
    /// The lines said in the bridge's current life.
    said: Said,
}

impl<'a> Update<'a> {
    fn new(
        image: &'a [u8],
        sha256: [u8; DIGEST_LEN],
        digest: &[u8; DIGEST_LEN],
        before: Option<[u8; DIGEST_LEN]>,
    ) -> Self {
        let sender = PacedSender::new(Sender::new(image, sha256, digest));
        Self { sender, start: Instant::now(), sent: *digest, before, said: Said::default() }
    }

    /// Why the update stopped, with whether the bridge named an image before it.
    fn failed(&self, failure: Failure) -> Failed {
        Failed::Sender { failure, image_known: self.before.is_some() }
    }

    /// Send the image until the P4 is told to install, or until `stop`, which aborts the session.
    async fn send(
        &mut self,
        link: &mut LinkHandle,
        stop: impl Future<Output = ()>,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(), Failed> {
        tokio::pin!(stop);
        let mut tick = tokio::time::interval(TICK);
        let mut written = link.written();
        loop {
            // A stop is taken up to the moment `Install` is handed to the link, so one that
            // resolved beside the status that verified the image wins over the pump that would
            // queue it.
            let stopped = tokio::select! {
                biased;
                () = &mut stop => true,
                () = std::future::ready(()) => false,
            };
            if stopped {
                return Err(self.abort(link).await);
            }
            self.pump(link)?;
            match self.sender.stage() {
                Stage::Sending => say(Progress::Sending { percent: self.sender.percent() }),
                Stage::Installed => return Ok(()),
                // The link wrote nothing for the pacing's stall bound: over USB, a port that
                // stopped draining, not a receiver that stopped answering.
                Stage::Failed(Failure::Silent) if self.sender.is_stalled() => {
                    return Err(Failed::PortStalled { percent: self.sender.percent() });
                }
                Stage::Failed(failure) => return Err(self.failed(failure)),
            }
            tokio::select! {
                biased;
                () = &mut stop => return Err(self.abort(link).await),
                _ = tick.tick() => {}
                () = written.changed(), if self.sender.is_waiting() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(message)) => self.take(message, say),
                    Some(LinkEvent::Disconnected { reason, .. }) => {
                        return Err(Failed::Dropped { reason });
                    }
                    Some(LinkEvent::Connected(info)) => {
                        self.said.new_life();
                        if on_ready(Phase::Sending, self.before, info.image) == OnReady::Abort {
                            return Err(Failed::RestartedIntoOther);
                        }
                    }
                    // The same life, which runs the image it ran.
                    Some(LinkEvent::Again(_) | LinkEvent::Garbled(_)) => {}
                    None => return Err(Failed::Closed),
                },
            }
        }
    }

    /// End the session the image is sent in, wait for the bridge's `Idle`, and say why the update
    /// ended.
    async fn abort(&mut self, link: &mut LinkHandle) -> Failed {
        let session = self.sender.session();
        let ended = pace::abort(link, session, HostToBridge::Update, update_status).await;
        tracing::debug!(session, ended, "bridge update aborted");
        Failed::Stopped
    }

    /// Wait out the reboot: `Install` again until the new life's `Ready`, with an `Identify`
    /// every [`POLL`] so the rebooted P4 hears a host. Returns the new life's core.
    async fn come_back(
        &mut self,
        link: &mut LinkHandle,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<BridgeCore, Failed> {
        let wait = async {
            let mut tick = tokio::time::interval(TICK);
            let mut poll = tokio::time::interval(POLL);
            let mut written = link.written();
            loop {
                self.pump(link)?;
                if let Stage::Failed(failure) = self.sender.stage() {
                    return Err(self.failed(failure));
                }
                tokio::select! {
                    _ = tick.tick() => {}
                    () = written.changed(), if self.sender.is_waiting() => {}
                    _ = poll.tick() => identify(link),
                    event = link.recv() => match event {
                        Some(LinkEvent::Connected(info)) => {
                            if on_ready(Phase::Installing, self.before, info.image)
                                == OnReady::Back
                            {
                                self.said.new_life();
                                return Ok(info.core());
                            }
                        }
                        Some(LinkEvent::Message(message)) => self.take(message, say),
                        // The life the install found, not yet rebooted: `verdict`'s `NotYet`.
                        // The transport reopens the port on its own.
                        Some(
                            LinkEvent::Again(_)
                            | LinkEvent::Disconnected { .. }
                            | LinkEvent::Garbled(_),
                        ) => {}
                        None => return Err(Failed::Closed),
                    },
                }
            }
        };
        match tokio::time::timeout(COME_BACK, wait).await {
            Ok(back) => back,
            Err(_) => Err(Failed::NoComeBack { over: Link::Usb, secs: COME_BACK.as_secs() }),
        }
    }

    /// Over USB, from the new life's first `Ready`, `back`: read each answer to an `Identify`
    /// every [`POLL`] until one settles the outcome, for up to [`CONFIRM_WAIT`].
    async fn confirm(
        &mut self,
        link: &mut LinkHandle,
        back: BridgeCore,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(), Failed> {
        let deadline = tokio::time::Instant::now() + CONFIRM_WAIT;
        let mut poll = tokio::time::interval(POLL);
        let mut reading = (back, true);
        loop {
            match verdict(&self.sent, self.before, &reading.0, reading.1) {
                Verdict::Installed => return Ok(()),
                Verdict::ConfirmFailed => return Err(Failed::NotConfirmed),
                Verdict::RolledBack => return Err(Failed::RolledBack),
                Verdict::Other => return Err(Failed::OtherImage),
                Verdict::OnTrial | Verdict::NotYet => {}
            }
            reading = loop {
                tokio::select! {
                    () = tokio::time::sleep_until(deadline) => {
                        return Err(Failed::NoConfirm { secs: Some(CONFIRM_WAIT.as_secs()) });
                    }
                    _ = poll.tick() => identify(link),
                    event = link.recv() => match event {
                        Some(LinkEvent::Message(message)) => self.said.line(&message, say),
                        Some(event) => {
                            if let Some((core, new_life)) = event.ready() {
                                if new_life {
                                    self.said.new_life();
                                }
                                break (core, new_life);
                            }
                        }
                        None => return Err(Failed::Closed),
                    },
                }
            };
        }
    }

    /// Over Bluetooth, from the install until the outcome: the link drops as the P4 reboots and
    /// again as its C6 restarts, and [`Settling`] reads the outcome across the reconnects. The
    /// sender is pumped, `Install` again, while the link is up and until a new life answers.
    async fn settle_bluetooth(
        &mut self,
        link: &mut LinkHandle,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(), Failed> {
        let mut settling = Settling::new(self.sent, self.before, Instant::now());
        let mut tick = tokio::time::interval(TICK);
        let mut poll = tokio::time::interval(POLL);
        let mut written = link.written();
        let mut deadline = tokio::time::Instant::now() + COME_BACK_BLUETOOTH;
        let mut dropped = false;
        loop {
            match settling.settled(Instant::now(), false) {
                Settled::Waiting | Settled::Unconfirmed => {}
                settled => return settled_end(settled, false, say),
            }
            let pumping = settling.up() && !settling.came_back();
            if pumping {
                self.pump(link)?;
                if let Stage::Failed(failure) = self.sender.stage() {
                    return Err(self.failed(failure));
                }
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    if !settling.came_back() {
                        let secs = COME_BACK_BLUETOOTH.as_secs();
                        return Err(Failed::NoComeBack { over: Link::Bluetooth, secs });
                    }
                    let now = Instant::now();
                    let settled = settling.settled(now, true);
                    return settled_end(settled, settling.radio_running(now), say);
                }
                _ = tick.tick() => {}
                () = written.changed(), if pumping && self.sender.is_waiting() => {}
                // Only while connected, so no backlog waits out a drop.
                _ = poll.tick(), if settling.up() => identify(link),
                event = link.recv() => match event {
                    Some(LinkEvent::Connected(info)) => {
                        // Asked at once, so a trial image is confirmed and the answer says so.
                        identify(link);
                        self.said.new_life();
                        let back = settling.came_back();
                        let radio = settling.radio_updated();
                        settling.see(Seen::Ready { core: info.core(), fresh: true }, Instant::now());
                        if !back && settling.came_back() {
                            deadline = tokio::time::Instant::now() + SETTLE_BLUETOOTH;
                        } else if back && radio {
                            say(Progress::RadioRestarted);
                        }
                    }
                    Some(LinkEvent::Again(core)) => {
                        settling.see(Seen::Ready { core, fresh: false }, Instant::now());
                    }
                    Some(LinkEvent::Disconnected { .. }) => {
                        settling.see(Seen::Down, Instant::now());
                        if !dropped {
                            dropped = true;
                            say(Progress::LinkDropped);
                        }
                    }
                    Some(LinkEvent::Message(message)) if !settling.came_back() => {
                        self.take(message, say);
                    }
                    Some(LinkEvent::Message(message)) => {
                        if is_radio_update(&message) {
                            settling.see(Seen::RadioUpdating, Instant::now());
                        }
                        self.said.line(&message, say);
                    }
                    Some(LinkEvent::Garbled(_)) => {}
                    None => return Err(Failed::Closed),
                },
            }
        }
    }

    /// Hand on what the C6's update says, until its new life or [`QUIET`] without a line about
    /// it.
    async fn follow_radio(
        &mut self,
        link: &mut LinkHandle,
        say: &mut (dyn FnMut(Progress) + Send),
    ) {
        let deadline = tokio::time::Instant::now() + FOLLOW;
        let mut quiet_until = tokio::time::Instant::now() + QUIET;
        let mut poll = tokio::time::interval(POLL);
        loop {
            let until = quiet_until.min(deadline);
            let Some(event) = next_event(link, &mut poll, until).await else {
                if until == deadline {
                    say(Progress::RadioStillUpdating);
                }
                return;
            };
            match event {
                Some(LinkEvent::Message(message)) => {
                    if is_radio_update(&message) {
                        quiet_until = tokio::time::Instant::now() + QUIET;
                    }
                    self.said.line(&message, say);
                }
                Some(LinkEvent::Connected(_)) => {
                    say(Progress::RadioRestarted);
                    return;
                }
                Some(_) => {}
                None => return,
            }
        }
    }

    /// Queue every op the sender has for now, once the link has written the last.
    fn pump(&mut self, link: &LinkHandle) -> Result<(), Failed> {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.sender.pump(link, now, HostToBridge::Update).map_err(Failed::Queue)
    }

    /// A message from the P4: a status for the sender, or a line to hand on.
    fn take(&mut self, message: BridgeToHost, say: &mut (dyn FnMut(Progress) + Send)) {
        match message {
            BridgeToHost::Update(bytes) => {
                if let Some(Op::Status(status)) = Op::decode(&bytes) {
                    self.sender.on_status(&status);
                }
            }
            message => self.said.line(&message, say),
        }
    }
}

/// The status a [`BridgeToHost::Update`] carries, as `pace::abort` reads it.
fn update_status(message: &BridgeToHost) -> Option<UpdateStatus> {
    match message {
        BridgeToHost::Update(bytes) => pace::status_in(bytes),
        _ => None,
    }
}

/// The bridge's lines, and which of them its current life has said. Every line is said until
/// [`Self::once_per_life`]: before `Install` no poll runs, so a line that arrives twice was sent
/// twice.
#[derive(Debug, Default)]
struct Said {
    /// Each line is said once per life.
    once: bool,
    /// The lines said in this life, once `once` is set.
    lines: HashSet<Line>,
}

impl Said {
    /// From now on the polls make the P4 repeat its announcement, so say each line once per life.
    fn once_per_life(&mut self) {
        self.once = true;
    }

    /// A new life repeats its lines, and they are said again.
    fn new_life(&mut self) {
        self.lines.clear();
    }

    /// Hand on a bridge's log or error line, unless this life has said it since
    /// [`Self::once_per_life`]; anything else is not a line.
    fn line(&mut self, message: &BridgeToHost, say: &mut (dyn FnMut(Progress) + Send)) {
        let line = match message {
            BridgeToHost::Error { message } => Line::Fault(message.to_string()),
            BridgeToHost::Log { message } => Line::Log(message.to_string()),
            _ => return,
        };
        if !self.once || self.lines.insert(line.clone()) {
            say(Progress::Line(line));
        }
    }
}

/// Ask for a `Ready`, so the P4 hears a host and says where the update stands. Dropped when the
/// queue is full: the next poll asks again.
fn identify(link: &LinkHandle) {
    let _ = link.send_bulk(HostToBridge::Identify);
}

/// The next event, asking every [`POLL`] while waiting, or `None` at `until`.
async fn next_event(
    link: &mut LinkHandle,
    poll: &mut tokio::time::Interval,
    until: tokio::time::Instant,
) -> Option<Option<LinkEvent>> {
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return None,
            _ = poll.tick() => identify(link),
            event = link.recv() => return Some(event),
        }
    }
}

/// Whether `message` is the P4's line about updating its C6 (`wartui_proto::split`,
/// `Pair::fault`).
fn is_radio_update(message: &BridgeToHost) -> bool {
    matches!(message, BridgeToHost::Error { message } if message.starts_with(RADIO_UPDATING))
}

/// What [`Settling`] has come to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Settled {
    /// Not yet.
    Waiting,
    /// A `Ready` from the P4's life before the install: the install never reached it.
    Lost,
    /// On the new image, confirmed.
    Installed,
    /// Back on the image it ran before.
    RolledBack,
    /// Back on neither image, or one it could not name.
    Other,
    /// On the new image, which the P4 failed to confirm.
    NotConfirmed,
    /// On the new image, at the deadline, still on trial.
    Unconfirmed,
}

/// One thing the link showed, for [`Settling`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// The link dropped.
    Down,
    /// A `Ready`: a new life's when `fresh`, else another answer from the same life.
    Ready { core: BridgeCore, fresh: bool },
    /// A line about the C6's update.
    RadioUpdating,
}

/// What an update over Bluetooth has shown since the install, read across reconnects. Pure: the
/// caller passes the time each thing was seen.
#[derive(Debug)]
struct Settling {
    sent: [u8; DIGEST_LEN],
    before: Option<[u8; DIGEST_LEN]>,
    installed_at: Instant,
    /// The latest `Ready` from a life after the install, once one has answered.
    latest: Option<BridgeCore>,
    /// A `Ready` from the life before the install came first.
    lost: bool,
    /// The link is connected.
    up: bool,
    /// When the link last came up or dropped, a new life answered, or a line about the C6's
    /// update arrived.
    news: Instant,
    /// A line about the C6's update arrived since the latest new life.
    radio: bool,
}

impl Settling {
    /// An update whose `Install` went at `installed_at`, still connected.
    fn new(
        sent: [u8; DIGEST_LEN],
        before: Option<[u8; DIGEST_LEN]>,
        installed_at: Instant,
    ) -> Self {
        Self {
            sent,
            before,
            installed_at,
            latest: None,
            lost: false,
            up: true,
            news: installed_at,
            radio: false,
        }
    }

    /// Whether a life after the install has announced itself.
    fn came_back(&self) -> bool {
        self.latest.is_some()
    }

    fn up(&self) -> bool {
        self.up
    }

    /// Whether the link has been up, with no new life and no line about the C6's update, for
    /// [`QUIET`].
    fn quiet(&self, now: Instant) -> bool {
        self.up && now.duration_since(self.news) >= QUIET
    }

    /// Whether a line about the C6's update arrived since the latest new life.
    fn radio_updated(&self) -> bool {
        self.radio
    }

    /// Whether the C6's update may still be running at `now`: a line about it arrived since the
    /// latest new life, and the link has not been quiet since.
    fn radio_running(&self, now: Instant) -> bool {
        self.radio && !self.quiet(now)
    }

    fn see(&mut self, seen: Seen, now: Instant) {
        match seen {
            Seen::Down => {
                self.up = false;
                self.news = now;
            }
            Seen::Ready { core, fresh: true } => {
                self.up = true;
                self.news = now;
                self.radio = false;
                // The P4 resets its C6 at boot, so a C6 older than the install is the P4's life
                // before it, which dropped its update session when the link went.
                let older =
                    u128::from(core.uptime_ms) > now.duration_since(self.installed_at).as_millis();
                if older && !self.came_back() {
                    self.lost = true;
                    return;
                }
                self.latest = Some(core);
            }
            // The same life answering again: only its image and trial are news, and they are no
            // news to the quiet, which the polls would otherwise never let pass.
            Seen::Ready { core, fresh: false } => {
                if let Some(latest) = &mut self.latest {
                    latest.image = core.image;
                    latest.trial = core.trial;
                }
            }
            Seen::RadioUpdating => {
                self.radio = true;
                self.news = now;
            }
        }
    }

    /// Where the update stands at `now`, or at the deadline when `deadline`, which no longer
    /// waits for the link to settle.
    fn settled(&self, now: Instant, deadline: bool) -> Settled {
        if self.lost {
            return Settled::Lost;
        }
        let Some(latest) = &self.latest else { return Settled::Waiting };
        // Every `Ready` kept is from a life after the install.
        match verdict(&self.sent, self.before, latest, true) {
            Verdict::ConfirmFailed => Settled::NotConfirmed,
            Verdict::RolledBack => Settled::RolledBack,
            Verdict::Other => Settled::Other,
            Verdict::Installed if deadline || self.quiet(now) => Settled::Installed,
            Verdict::OnTrial if deadline => Settled::Unconfirmed,
            Verdict::Installed | Verdict::OnTrial | Verdict::NotYet => Settled::Waiting,
        }
    }
}

/// How an update over Bluetooth ended, `Ok` once installed. `radio_running` is whether the C6's
/// update may still be running ([`Settling::radio_running`]).
fn settled_end(
    settled: Settled,
    radio_running: bool,
    say: &mut (dyn FnMut(Progress) + Send),
) -> Result<(), Failed> {
    match settled {
        Settled::Installed => {
            say(Progress::Confirmed);
            if radio_running {
                say(Progress::RadioStillUpdating);
            }
            Ok(())
        }
        Settled::Lost => Err(Failed::NeverTookInstall),
        Settled::RolledBack => Err(Failed::RolledBack),
        Settled::Other => Err(Failed::OtherImage),
        Settled::NotConfirmed => Err(Failed::NotConfirmed),
        Settled::Unconfirmed => Err(Failed::NoConfirm { secs: None }),
        Settled::Waiting => Err(Failed::NotBack),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use wartui_bridge::{BridgeInfo, DisconnectCause, LinkPlumbing, link_pair};
    use wartui_proto::bus::{Board, NodeBoard, UpdateState};
    use wartui_proto::link::{
        BRIDGE_VERSION, LogStr, LoopPhase, UPDATE_STATUS_MAX, UpdateStatusBytes,
    };
    use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
    use wartui_proto::radio_link::update::{Received, receive};
    use wartui_proto::reset::ResetCause;

    use super::super::image::BRIDGE_PROJECT;
    use super::super::testing::{P4_APP_HEADER, fixture_with};
    use super::*;

    const NEW: [u8; DIGEST_LEN] = [0x11; DIGEST_LEN];
    const OLD: [u8; DIGEST_LEN] = [0x22; DIGEST_LEN];
    const MAC: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

    /// A `Ready`'s core naming `image` on `trial`, from a C6 up `uptime_ms`.
    fn core(image: [u8; DIGEST_LEN], trial: Trial, uptime_ms: u32) -> BridgeCore {
        BridgeCore { mac: MAC, image: Some(image), version: BRIDGE_VERSION, trial, uptime_ms }
    }

    #[test]
    fn update_bridge_keeps_sending_when_ready_names_the_old_image() {
        // The C6 restarted, and the P4 announced its new life.
        assert_eq!(on_ready(Phase::Sending, Some(OLD), Some(OLD)), OnReady::Ignore);
        assert_eq!(on_ready(Phase::Sending, None, None), OnReady::Ignore);
    }

    #[test]
    fn update_bridge_stops_sending_when_ready_names_another_image() {
        assert_eq!(on_ready(Phase::Sending, Some(OLD), Some(NEW)), OnReady::Abort);
        assert_eq!(on_ready(Phase::Sending, Some(OLD), None), OnReady::Abort);
    }

    #[test]
    fn update_bridge_takes_ready_as_new_life_when_it_follows_install() {
        // The connection holds through the reboot, so a new life is the only sign of it.
        assert_eq!(on_ready(Phase::Installing, Some(OLD), Some(NEW)), OnReady::Back);
        assert_eq!(on_ready(Phase::Installing, Some(OLD), Some(OLD)), OnReady::Back);
    }

    #[test]
    fn update_bridge_reads_radio_update_when_line_starts_with_shared_text() {
        assert!(is_radio_update(&error(&format!("{RADIO_UPDATING} to the bundled image (5%)"))));
        assert!(!is_radio_update(&error("the radio coprocessor is not answering")));
        assert!(!is_radio_update(&log(RADIO_UPDATING)));
    }

    #[test]
    fn update_bridge_reads_installed_when_new_image_is_settled() {
        for new_life in [true, false] {
            let ready = core(NEW, Trial::Settled, 1_500);
            assert_eq!(verdict(&NEW, Some(OLD), &ready, new_life), Verdict::Installed);
        }
    }

    #[test]
    fn update_bridge_reads_on_trial_when_new_image_is_on_trial() {
        let ready = core(NEW, Trial::OnTrial, 1_500);
        assert_eq!(verdict(&NEW, Some(OLD), &ready, true), Verdict::OnTrial);
    }

    #[test]
    fn update_bridge_reads_on_trial_when_trial_code_is_unnamed() {
        // A later build's trial code: `trial` codes are never reused, and one a host cannot
        // name is on trial.
        let ready = core(NEW, Trial::Other(9), 1_500);
        assert_eq!(verdict(&NEW, Some(OLD), &ready, true), Verdict::OnTrial);
    }

    #[test]
    fn update_bridge_reads_confirm_failed_when_new_image_failed_its_confirm() {
        let ready = core(NEW, Trial::ConfirmFailed, 1_500);
        assert_eq!(verdict(&NEW, Some(OLD), &ready, true), Verdict::ConfirmFailed);
    }

    #[test]
    fn update_bridge_reads_rolled_back_when_new_life_names_old_image() {
        for trial in [Trial::Settled, Trial::OnTrial, Trial::ConfirmFailed] {
            let ready = core(OLD, trial, 1_500);
            assert_eq!(verdict(&NEW, Some(OLD), &ready, true), Verdict::RolledBack);
        }
    }

    #[test]
    fn update_bridge_reads_not_yet_when_same_life_names_old_image() {
        for trial in [Trial::Settled, Trial::OnTrial] {
            let ready = core(OLD, trial, 600_000);
            assert_eq!(verdict(&NEW, Some(OLD), &ready, false), Verdict::NotYet);
        }
    }

    #[test]
    fn update_bridge_reads_other_when_ready_names_neither_image() {
        let ready = core([0x33; DIGEST_LEN], Trial::Settled, 1_500);
        assert_eq!(verdict(&NEW, Some(OLD), &ready, true), Verdict::Other);
        let unnamed = BridgeCore { image: None, ..ready };
        assert_eq!(verdict(&NEW, Some(OLD), &unnamed, true), Verdict::Other);
        assert_eq!(verdict(&NEW, None, &unnamed, true), Verdict::Other);
        assert_eq!(verdict(&NEW, None, &unnamed, false), Verdict::Other);
    }

    #[test]
    fn update_bridge_says_lines_again_when_a_new_life_repeats_them() {
        let mut said = Said::default();
        said.once_per_life();
        let mut lines = Vec::new();
        let mut say = |progress| lines.push(progress);
        said.line(&error("no GPS receiver"), &mut say);
        said.line(&error("no GPS receiver"), &mut say);
        said.line(&log("no GPS receiver"), &mut say);
        said.new_life();
        said.line(&error("no GPS receiver"), &mut say);
        let fault = Progress::Line(Line::Fault("no GPS receiver".to_owned()));
        let info = Progress::Line(Line::Log("no GPS receiver".to_owned()));
        assert_eq!(lines, [fault.clone(), info, fault]);
    }

    /// An update to `NEW` over Bluetooth from `OLD`, installed at `at`.
    fn settling(at: Instant) -> Settling {
        Settling::new(NEW, Some(OLD), at)
    }

    /// `ms` after `at`.
    fn after(at: Instant, ms: u64) -> Instant {
        at + Duration::from_millis(ms)
    }

    /// A new life's `Ready`.
    fn fresh(image: [u8; DIGEST_LEN], trial: Trial, uptime_ms: u32) -> Seen {
        Seen::Ready { core: core(image, trial, uptime_ms), fresh: true }
    }

    /// Another answer from the same life.
    fn again(image: [u8; DIGEST_LEN], trial: Trial, uptime_ms: u32) -> Seen {
        Seen::Ready { core: core(image, trial, uptime_ms), fresh: false }
    }

    #[test]
    fn update_bridge_settles_installed_when_new_image_is_back_confirmed_and_quiet() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        s.see(fresh(NEW, Trial::Settled, 1_500), after(at, 4_000));
        assert_eq!(s.settled(after(at, 6_000), false), Settled::Waiting, "not yet quiet");
        assert_eq!(s.settled(after(at, 9_000), false), Settled::Installed);
    }

    #[test]
    fn update_bridge_keeps_quiet_clock_when_ready_repeats() {
        // The polls bring an answer every second. Each says where the trial stands, and none is
        // news to the quiet.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::OnTrial, 1_500), after(at, 4_000));
        for second in 1..=5 {
            let uptime = 1_500 + second * 1_000;
            s.see(again(NEW, Trial::Settled, uptime), after(at, 4_000 + u64::from(second) * 1_000));
        }
        assert_eq!(s.settled(after(at, 9_000), false), Settled::Installed);
    }

    #[test]
    fn update_bridge_ignores_repeat_when_no_life_after_install_has_answered() {
        // The life the install found answers the polls until the link drops.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(again(OLD, Trial::Settled, 600_000), after(at, 1_000));
        assert!(!s.came_back());
        assert_eq!(s.settled(after(at, 1_000), false), Settled::Waiting);
    }

    #[test]
    fn update_bridge_keeps_waiting_when_radio_update_drops_the_link_after_confirm() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::Settled, 1_500), after(at, 4_000));
        s.see(Seen::RadioUpdating, after(at, 8_000));
        assert_eq!(s.settled(after(at, 12_000), false), Settled::Waiting, "a line 4 s ago");
        s.see(Seen::Down, after(at, 20_000));
        assert_eq!(s.settled(after(at, 30_000), false), Settled::Waiting, "the link is down");
        s.see(fresh(NEW, Trial::Settled, 2_000), after(at, 33_000));
        assert_eq!(s.settled(after(at, 38_000), false), Settled::Installed);
    }

    #[test]
    fn update_bridge_omits_radio_line_when_link_is_down_at_the_deadline_with_no_radio_update() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::Settled, 1_500), after(at, 4_000));
        s.see(Seen::Down, after(at, 6_000));
        let deadline = after(at, 154_000);
        assert_eq!(s.settled(deadline, true), Settled::Installed);
        assert!(!s.radio_running(deadline), "nothing said the radio was updating");
    }

    #[test]
    fn update_bridge_says_radio_line_when_link_is_down_at_the_deadline_after_a_radio_update() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::Settled, 1_500), after(at, 4_000));
        s.see(Seen::RadioUpdating, after(at, 5_000));
        s.see(Seen::Down, after(at, 6_000));
        assert!(s.radio_running(after(at, 154_000)));
        // The C6's new life ends it, and a repeat does not.
        s.see(again(NEW, Trial::Settled, 2_000), after(at, 153_000));
        assert!(s.radio_running(after(at, 154_000)));
        s.see(fresh(NEW, Trial::Settled, 1_000), after(at, 154_000));
        assert!(!s.radio_running(after(at, 154_000)));
    }

    #[test]
    fn update_bridge_says_confirmed_before_radio_line_when_settled_with_radio_running() {
        let mut said = Vec::new();
        let ended = settled_end(Settled::Installed, true, &mut |progress| said.push(progress));
        assert_eq!(ended, Ok(()));
        assert_eq!(said, [Progress::Confirmed, Progress::RadioStillUpdating]);
    }

    #[test]
    fn update_bridge_ends_in_each_failure_when_settled_short_of_installed() {
        let mut say = |_: Progress| panic!("a failure says nothing");
        let mut ended = |settled| settled_end(settled, false, &mut say);
        assert_eq!(ended(Settled::Lost), Err(Failed::NeverTookInstall));
        assert_eq!(ended(Settled::RolledBack), Err(Failed::RolledBack));
        assert_eq!(ended(Settled::Other), Err(Failed::OtherImage));
        assert_eq!(ended(Settled::NotConfirmed), Err(Failed::NotConfirmed));
        assert_eq!(ended(Settled::Unconfirmed), Err(Failed::NoConfirm { secs: None }));
        assert_eq!(ended(Settled::Waiting), Err(Failed::NotBack));
    }

    #[test]
    fn update_bridge_judges_from_latest_ready_when_bridge_reconnects_on_old_image() {
        // The new image announced itself, then its trial watchdog rolled it back.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::OnTrial, 1_500), after(at, 4_000));
        s.see(Seen::Down, after(at, 34_000));
        s.see(fresh(OLD, Trial::Settled, 1_500), after(at, 70_000));
        assert_eq!(s.settled(after(at, 70_000), false), Settled::RolledBack);
    }

    #[test]
    fn update_bridge_reports_install_lost_when_ready_predates_the_install() {
        // The link dropped before the P4 took the `Install`, so it dropped its session and
        // never rebooted: its C6 has been up since before the install.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        s.see(fresh(OLD, Trial::Settled, 600_000), after(at, 3_000));
        assert!(!s.came_back());
        assert_eq!(s.settled(after(at, 3_000), false), Settled::Lost, "at once");
    }

    #[test]
    fn update_bridge_reports_rolled_back_when_trial_boots_end_on_old_image() {
        // Each trial boot runs until the P4's 30 s watchdog, then the link takes a while to
        // drop and come back. Every answer says the image is on trial, and the third boot is the
        // old image.
        const TRIAL_MS: u64 = 30_000;
        const RECONNECT_MS: u64 = 15_000;
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        let first_back = 20_000;
        let mut ready_at = first_back;
        for _ in 0..2 {
            s.see(fresh(NEW, Trial::OnTrial, 1_500), after(at, ready_at));
            s.see(again(NEW, Trial::OnTrial, 21_500), after(at, ready_at + 20_000));
            assert_eq!(s.settled(after(at, ready_at + 20_000), false), Settled::Waiting);
            s.see(Seen::Down, after(at, ready_at + TRIAL_MS));
            ready_at += TRIAL_MS + RECONNECT_MS;
        }
        s.see(fresh(OLD, Trial::Settled, 1_500), after(at, ready_at));
        assert_eq!(s.settled(after(at, ready_at), false), Settled::RolledBack);
        // Within the wait that follows the first new life, so it is never reported unconfirmed.
        assert!(Duration::from_millis(ready_at - first_back) + QUIET < SETTLE_BLUETOOTH);
    }

    #[test]
    fn update_bridge_reports_unconfirmed_at_deadline_when_image_stays_on_trial() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::OnTrial, 1_500), after(at, 4_000));
        s.see(again(NEW, Trial::OnTrial, 9_500), after(at, 12_000));
        assert_eq!(s.settled(after(at, 12_000), false), Settled::Waiting);
        assert_eq!(s.settled(after(at, 154_000), true), Settled::Unconfirmed);
    }

    #[test]
    fn update_bridge_reports_failed_confirm_at_once_when_ready_says_so() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(fresh(NEW, Trial::OnTrial, 1_500), after(at, 4_000));
        s.see(again(NEW, Trial::ConfirmFailed, 1_600), after(at, 4_100));
        assert_eq!(s.settled(after(at, 4_100), false), Settled::NotConfirmed);
    }

    #[test]
    fn update_bridge_waits_for_the_new_life_when_nothing_came_back_by_the_deadline() {
        // The caller words this case itself: the bridge did not come back.
        let at = Instant::now();
        let s = settling(at);
        assert_eq!(s.settled(after(at, 180_000), true), Settled::Waiting);
    }

    fn log(text: &str) -> BridgeToHost {
        BridgeToHost::Log { message: LogStr::try_from(text).unwrap() }
    }

    fn error(text: &str) -> BridgeToHost {
        BridgeToHost::Error { message: LogStr::try_from(text).unwrap() }
    }

    /// A slot the scripted P4 writes the image into.
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

    /// The P4 bridge's `Ready` from `core`.
    fn ready(core: BridgeCore) -> BridgeInfo {
        BridgeInfo {
            mac: core.mac,
            fw_version: "0.1.0".to_owned(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms: core.uptime_ms,
            node_board: NodeBoard::Carrier14,
            radio: None,
            image: core.image,
            version: core.version,
            trial: core.trial,
        }
    }

    /// How the scripted P4 behaves.
    #[derive(Clone)]
    struct Script {
        over: Link,
        /// The image it boots into on `Install`.
        boots: [u8; DIGEST_LEN],
        /// The trial each `Ready` after the reboot names, in turn; the last repeats.
        trials: Vec<Trial>,
        /// Lines behind every `Ready` after the reboot, as each announcement repeats them.
        lines: Vec<BridgeToHost>,
        /// How many `Identify`s the life before answers, with the old image, before the P4 takes
        /// an `Install` it holds.
        old_answers: usize,
        /// Drops its end of the link after the new life's first `Ready`.
        closes: bool,
        /// How long each command takes.
        per_command: Duration,
        /// Lines sent behind the status that answers `Begin`, while the image is sent.
        sending_lines: Vec<BridgeToHost>,
        /// Notified just before the status that says the image is verified goes up.
        verified: Option<Arc<tokio::sync::Notify>>,
    }

    impl Script {
        /// A P4 over `over` that boots into `boots`, settled at once, and says nothing else.
        fn new(over: Link, boots: [u8; DIGEST_LEN]) -> Self {
            Self {
                over,
                boots,
                trials: vec![Trial::Settled],
                lines: Vec::new(),
                old_answers: 0,
                closes: false,
                per_command: Duration::ZERO,
                sending_lines: Vec::new(),
                verified: None,
            }
        }
    }

    /// What the scripted P4 took or did, and when.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Took {
        /// An `Identify`, after the reboot when `rebooted`.
        Identify { rebooted: bool },
        /// An update op, by its code.
        Op(u8),
        /// It answered an `Abort` with `Idle`.
        Idle,
    }

    type TookLog = Arc<Mutex<Vec<(Took, tokio::time::Instant)>>>;

    /// A P4 at the bridge's end of the link: it takes the update into a slot as its receiver
    /// does, and on `Install` reboots into the image `script` names. Over Bluetooth the link
    /// drops first, and the new life's C6 is just up. It answers every `Identify` with a `Ready`
    /// of the life it is in, and after the reboot repeats `script`'s lines behind each. Every
    /// command counts done.
    fn scripted_p4(mut plumbing: LinkPlumbing, script: Script) -> TookLog {
        let took = TookLog::default();
        let log = Arc::clone(&took);
        tokio::spawn(async move {
            let mut receiver = Receiver::new(Board::Esp32p4);
            let mut slot = Slot(vec![0xFF; 64 * SECTOR]);
            // `Identify`s the life before answered, and an `Install` held until they are enough.
            let mut answered = 0;
            let mut held = false;
            // Where the new life stands: when it began and how many `Ready`s it has sent.
            let mut life: Option<(tokio::time::Instant, usize)> = None;
            let started = tokio::time::Instant::now();
            let note = |what| log.lock().unwrap().push((what, tokio::time::Instant::now()));
            while let Some(cmd) = plumbing.commands.recv().await {
                tokio::time::sleep(script.per_command).await;
                plumbing.commands.done(1);
                let mut events = Vec::new();
                match cmd {
                    HostToBridge::Identify => {
                        note(Took::Identify { rebooted: life.is_some() });
                        if let Some((began, readies)) = &mut life {
                            let trial = script.trials[(*readies).min(script.trials.len() - 1)];
                            *readies += 1;
                            let up = u32::try_from(began.elapsed().as_millis()).unwrap();
                            events.push(LinkEvent::Again(core(script.boots, trial, 800 + up)));
                            events.extend(script.lines.iter().cloned().map(LinkEvent::Message));
                        } else {
                            let up = u32::try_from(started.elapsed().as_millis()).unwrap();
                            events.push(LinkEvent::Again(core(OLD, Trial::Settled, 600_000 + up)));
                            answered += 1;
                            if held && answered >= script.old_answers {
                                held = false;
                                life = Some(reboot(&script, &mut events));
                            }
                        }
                    }
                    HostToBridge::Update(bytes) if life.is_none() => {
                        note(Took::Op(bytes[0]));
                        let op = Op::decode(&bytes).expect("an op");
                        match receive(&mut receiver, op, Some(&mut slot), false) {
                            Received::Status(Some(status)) => {
                                if bytes[0] == 6 {
                                    note(Took::Idle);
                                }
                                if status.state == UpdateState::Verified
                                    && let Some(verified) = &script.verified
                                {
                                    verified.notify_one();
                                }
                                let mut buf = [0u8; UPDATE_STATUS_MAX];
                                let len = Op::Status(status).encode(&mut buf);
                                let up = UpdateStatusBytes::from_slice(&buf[..len]).unwrap();
                                events.push(LinkEvent::Message(BridgeToHost::Update(up)));
                                if bytes[0] == 1 {
                                    let lines = script.sending_lines.iter().cloned();
                                    events.extend(lines.map(LinkEvent::Message));
                                }
                            }
                            Received::Status(None) => {}
                            Received::Install(_) if answered < script.old_answers => held = true,
                            Received::Install(_) => life = Some(reboot(&script, &mut events)),
                        }
                    }
                    // The rebooted P4 has no session, and the sender no longer listens.
                    _ => {}
                }
                for event in events {
                    if plumbing.events.send(event).await.is_err() {
                        return;
                    }
                }
                if script.closes && life.is_some() {
                    return;
                }
            }
        });
        took
    }

    /// The scripted P4's reboot into its new life, as the link reports it: the new life's start
    /// and the one `Ready` it has sent.
    fn reboot(script: &Script, events: &mut Vec<LinkEvent>) -> (tokio::time::Instant, usize) {
        let trial = script.trials[0];
        if script.over == Link::Bluetooth {
            let reason = "the bridge disconnected".to_owned();
            let cause = DisconnectCause::Other;
            events.push(LinkEvent::Disconnected { reason, cause });
            events.push(LinkEvent::Connected(ready(core(script.boots, trial, 0))));
        } else {
            events.push(LinkEvent::Connected(ready(core(script.boots, trial, 800))));
        }
        events.extend(script.lines.iter().cloned().map(LinkEvent::Message));
        (tokio::time::Instant::now(), 1)
    }

    /// The P4 bridge's app image, a few sectors long.
    fn bridge_image() -> BridgeImage {
        let bytes = fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 3 * SECTOR + 1_024);
        BridgeImage::parse(bytes).unwrap()
    }

    /// What an update said, every `Sending` aside, and how many `Sending`s there were.
    #[derive(Debug, Default)]
    struct Heard {
        said: Vec<Progress>,
        sending: usize,
    }

    /// Update from `OLD` to [`bridge_image`] against a P4 that `script` makes of the image, until
    /// `stop`.
    async fn update_against(
        script: impl FnOnce(&BridgeImage) -> Script,
        stop: impl Future<Output = ()> + Send,
    ) -> (Result<Updated, Failed>, Heard, TookLog) {
        let image = bridge_image();
        let script = script(&image);
        let over = script.over;
        let (mut link, plumbing) = link_pair();
        let took = scripted_p4(plumbing, script);
        let mut heard = Heard::default();
        let updated = update(&mut link, over, Some(OLD), &image, stop, |progress| match progress {
            Progress::Sending { .. } => heard.sending += 1,
            progress => heard.said.push(progress),
        })
        .await;
        (updated, heard, took)
    }

    /// The same, never stopped.
    async fn update_with(
        script: impl FnOnce(&BridgeImage) -> Script,
    ) -> (Result<Updated, Failed>, Heard, TookLog) {
        update_against(script, std::future::pending()).await
    }

    /// How many `Identify`s the P4 took after its reboot.
    fn identifies_after_reboot(took: &TookLog) -> usize {
        let took = took.lock().unwrap();
        took.iter().filter(|(what, _)| *what == Took::Identify { rebooted: true }).count()
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_settles_installed_when_ready_names_new_image_settled() {
        let (updated, heard, _) = update_with(|image| Script::new(Link::Usb, image.digest())).await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert!(heard.sending > 0, "the send was said");
        assert_eq!(heard.said, [Progress::Installing, Progress::Confirmed]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_asks_again_when_ready_names_new_image_on_trial() {
        let trials = vec![Trial::OnTrial, Trial::OnTrial, Trial::Settled];
        let (updated, heard, took) =
            update_with(|image| Script { trials, ..Script::new(Link::Usb, image.digest()) }).await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert_eq!(heard.said, [Progress::Installing, Progress::Confirmed]);
        assert!(identifies_after_reboot(&took) >= 2, "asked until the image settled");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_fails_no_confirm_when_new_image_stays_on_trial() {
        let trials = vec![Trial::OnTrial];
        let (updated, heard, _) =
            update_with(|image| Script { trials, ..Script::new(Link::Usb, image.digest()) }).await;
        assert_eq!(updated, Err(Failed::NoConfirm { secs: Some(CONFIRM_WAIT.as_secs()) }));
        assert_eq!(heard.said, [Progress::Installing]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_fails_not_confirmed_when_ready_says_confirm_failed() {
        // The P4's line saying why reaches the operator as a line.
        let trials = vec![Trial::ConfirmFailed];
        let lines = vec![error("could not confirm the image: flash write failed")];
        let (updated, heard, _) =
            update_with(|image| Script { trials, lines, ..Script::new(Link::Usb, image.digest()) })
                .await;
        assert_eq!(updated, Err(Failed::NotConfirmed));
        assert_eq!(heard.said, [Progress::Installing]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_judges_rolled_back_when_new_life_names_old_image() {
        let (updated, heard, _) = update_with(|_| Script::new(Link::Usb, OLD)).await;
        assert_eq!(updated, Err(Failed::RolledBack));
        assert_eq!(heard.said, [Progress::Installing]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_keeps_waiting_when_same_life_names_old_image() {
        // The P4 holds the `Install` while the life before answers polls with the old image: it
        // has not rebooted yet.
        let (updated, heard, took) = update_with(|image| Script {
            old_answers: 4,
            ..Script::new(Link::Usb, image.digest())
        })
        .await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert_eq!(heard.said, [Progress::Installing, Progress::Confirmed]);
        let took = took.lock().unwrap();
        let before = took.iter().filter(|(what, _)| *what == Took::Identify { rebooted: false });
        assert_eq!(before.count(), 4, "the life before answered");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_says_each_line_once_when_announcement_repeats() {
        let trials = vec![Trial::OnTrial, Trial::OnTrial, Trial::Settled];
        let lines = vec![error("no GPS receiver answered"), log("carrier: 14 slots")];
        let (updated, heard, took) =
            update_with(|image| Script { trials, lines, ..Script::new(Link::Usb, image.digest()) })
                .await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert!(identifies_after_reboot(&took) > 2, "the announcement repeated");
        let expected = [
            Progress::Installing,
            Progress::Line(Line::Fault("no GPS receiver answered".to_owned())),
            Progress::Line(Line::Log("carrier: 14 slots".to_owned())),
            Progress::Confirmed,
        ];
        assert_eq!(heard.said, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_sends_abort_and_waits_for_idle_when_stopped_while_sending() {
        // Each op takes 20 ms, so the image is part sent at the stop.
        let stop = tokio::time::sleep(Duration::from_millis(100));
        let started = tokio::time::Instant::now();
        let (updated, heard, took) = update_against(
            |image| Script {
                per_command: Duration::from_millis(20),
                ..Script::new(Link::Usb, image.digest())
            },
            stop,
        )
        .await;
        let ended = tokio::time::Instant::now();
        assert_eq!(updated, Err(Failed::Stopped));
        assert!(heard.sending > 0 && heard.said.is_empty(), "{heard:?}");
        let took = took.lock().unwrap();
        let ops: Vec<u8> = took
            .iter()
            .filter_map(|(what, _)| match what {
                Took::Op(code) => Some(*code),
                _ => None,
            })
            .collect();
        assert_eq!(ops.last(), Some(&6), "an `Abort` last: {ops:?}");
        assert!(!ops.contains(&4), "no `Install`");
        let idle = took.iter().find(|(what, _)| *what == Took::Idle).expect("answered `Idle`");
        assert!(idle.1 <= ended && idle.1 > started, "the update waited for the `Idle`");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_takes_stop_when_it_arrives_with_the_verified_status() {
        // The stop resolves as the status that lets `Install` go arrives. The operator has been
        // told it is stopping, so it wins.
        let verified = Arc::new(tokio::sync::Notify::new());
        let stop = {
            let verified = Arc::clone(&verified);
            async move { verified.notified().await }
        };
        let (updated, heard, took) = update_against(
            |image| Script { verified: Some(verified), ..Script::new(Link::Usb, image.digest()) },
            stop,
        )
        .await;
        assert_eq!(updated, Err(Failed::Stopped));
        assert!(heard.said.is_empty(), "{heard:?}");
        let took = took.lock().unwrap();
        assert!(!took.iter().any(|(what, _)| *what == Took::Op(4)), "no `Install`");
        assert!(took.iter().any(|(what, _)| *what == Took::Idle), "the session was aborted");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_says_repeated_line_twice_when_image_is_still_being_sent() {
        // No poll runs while the image is sent, so a line that arrives twice was sent twice.
        let line = error("the radio coprocessor is not answering");
        let (updated, heard, _) = update_with(|image| Script {
            sending_lines: vec![line.clone(), line],
            ..Script::new(Link::Usb, image.digest())
        })
        .await;
        assert_eq!(updated, Ok(Updated::Installed));
        let said = Progress::Line(Line::Fault("the radio coprocessor is not answering".to_owned()));
        assert_eq!(heard.said, [said.clone(), said, Progress::Installing, Progress::Confirmed]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_runs_to_outcome_when_stopped_after_install() {
        let image = bridge_image();
        let (mut link, plumbing) = link_pair();
        let took = scripted_p4(plumbing, Script::new(Link::Usb, image.digest()));
        let (stopped, stop) = tokio::sync::oneshot::channel::<()>();
        let mut stopped = Some(stopped);
        let mut said = Vec::new();
        let stop = async {
            let _ = stop.await;
        };
        let updated = update(&mut link, Link::Usb, Some(OLD), &image, stop, |progress| {
            if progress == Progress::Installing {
                // The caller stops as soon as the install goes out.
                let _ = stopped.take().map(|stopped| stopped.send(()));
            }
            if !matches!(progress, Progress::Sending { .. }) {
                said.push(progress);
            }
        })
        .await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert_eq!(said, [Progress::Installing, Progress::Confirmed]);
        assert!(!took.lock().unwrap().iter().any(|(what, _)| *what == Took::Op(6)), "no `Abort`");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_installs_image_when_bluetooth_link_drops_and_comes_back_confirmed() {
        // `Settling` reads `std::time::Instant`, which does not advance under a paused clock. So
        // the new life's C6 is up 0 ms, since any more reads as older than the install and ends
        // in `NeverTookInstall`, and the link is never quiet for `QUIET`: the update ends at the
        // tokio deadline, `SETTLE_BLUETOOTH` after the new life, through `settled(now, true)`,
        // which is `Installed` because the `Ready` says the image is settled. The script answers
        // no `StopScan`, so the scan is not reported stopped first.
        let (updated, heard, took) =
            update_with(|image| Script::new(Link::Bluetooth, image.digest())).await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert!(heard.sending > 0, "the send was said");
        let expected = [
            Progress::ScanNotStopped,
            Progress::Installing,
            Progress::LinkDropped,
            Progress::Confirmed,
        ];
        assert_eq!(heard.said, expected);
        assert!(identifies_after_reboot(&took) > 0, "asked once reconnected");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_fails_closed_when_link_closes_before_the_confirm() {
        let (updated, heard, _) = update_with(|image| Script {
            trials: vec![Trial::OnTrial],
            closes: true,
            ..Script::new(Link::Usb, image.digest())
        })
        .await;
        assert_eq!(updated, Err(Failed::Closed));
        assert_eq!(heard.said, [Progress::Installing], "never confirmed");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_sends_nothing_when_bridge_already_runs_the_image() {
        let image = bridge_image();
        let (mut link, mut plumbing) = link_pair();
        let running = Some(image.digest());
        let pending = std::future::pending();
        let updated =
            update(&mut link, Link::Bluetooth, running, &image, pending, |_| panic!("said")).await;
        assert_eq!(updated, Ok(Updated::AlreadyCurrent));
        assert!(plumbing.commands.try_recv().is_none());
    }
}
