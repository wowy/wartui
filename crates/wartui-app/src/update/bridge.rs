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
//! **A `Ready` is not always the P4 rebooting.** The P4 announces a new life when only its C6
//! restarts, since `Ready.uptime_ms` is the C6's. While sending, a `Ready` naming the image the
//! P4 ran before is that, and the send goes on: a P4 that did reboot dropped the session, which
//! the sender notices. One naming another image means the P4 is no longer running the image the
//! send began against, so the send stops. After `Install`, the next new life is the P4's
//! (`on_ready`): over USB its CH343 stays on the bus, so the connection holds through the
//! reboot, and the transport reports the new life by its uptime falling.
//!
//! **`Ready.image` names the outcome.** The P4 installs and reboots into the new image on trial,
//! which writes nothing until a host frame arrives. So from the install on, the host sends a
//! `GetStatus` every `POLL`; the first frame after the C6's hello confirms the image. A `Ready`
//! carrying the new digest is installed. One carrying the old is a rollback: the P4's trial
//! watchdog resets a hung image twice, and the third boot returns it to the old one, which is why
//! the wait is `COME_BACK`. The bootloader's log after each reset arrives as garbage, which a
//! held connection passes over.
//!
//! **The confirm is reported, not assumed.** The P4 says whether a host frame confirmed its
//! image in a line after its `Ready`, in the text `wartui_proto::link` holds for both ends. Only
//! that line is "confirmed": an image whose confirm failed, or whose line never came, is still on
//! trial and rolls back unless a host confirms it. The polls go on until the C6's update is
//! followed, so the P4's gate stays open for its lines.
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
//! - Any other `Ready` is a life after the install, and the update is judged on the latest one's
//!   image, so trial boots ending on the old image read as a rollback. Each reconnect is answered
//!   with a `GetStatus`, the frame that confirms a trial image. The sender is pumped, `Install`
//!   again, only while the link is up and no new life has answered.
//! - The update ends once the image is confirmed and the link has stayed up for `QUIET` with no
//!   line about the C6's update. Neither `Ready` nor `RadioInfo` names the C6's image, so that
//!   quiet is the C6's outcome. The confirm's line can be lost to a drop, so a bridge on the new
//!   image whose uptime passes its trial watchdog (`OUTLIVED_TRIAL_MS`) counts as confirmed: an
//!   image no host confirmed would have reset.
//! - It waits `COME_BACK_BLUETOOTH` for a new life, then `SETTLE_BLUETOOTH` after the first
//!   one, long enough for three trial boots to end in a rollback.
//!
//! A failed update over Bluetooth is recovered over USB, by updating again there; nothing resumes
//! a partly sent image.
//!
//! **Dropping the future stops the update.** The bridge drops its update session when the link
//! goes, and keeps its image.

use std::time::{Duration, Instant};

use wartui_bridge::pace::PacedSender;
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle, SendError};
use wartui_core::runtime::stop_scan;
use wartui_proto::link::{
    BridgeToHost, HostToBridge, IMAGE_CONFIRMED, IMAGE_NOT_CONFIRMED, RADIO_UPDATING,
};
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::radio_link::update::{Failure, Op, Sender, Stage};

use super::image::BridgeImage;
use crate::settings::Link;

/// How often the sender is asked for its next op, besides on each status.
const TICK: Duration = Duration::from_millis(50);

/// How often the host sends a `GetStatus` from the install on: a rebooted P4 writes nothing
/// until a host frame arrives, and its gate closes after 15 s without one.
const POLL: Duration = Duration::from_secs(1);

/// How long to wait for the P4 to come back after the install: two trial boots that the 30 s
/// watchdog ends and the third that rolls back, with room for each boot.
const COME_BACK: Duration = Duration::from_secs(120);

/// How long after the new image's `Ready` to wait for the line saying whether a host frame
/// confirmed it. The P4 sends it behind the `Ready`, or behind the next poll when its C6 had not
/// yet said hello.
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

/// The P4's trial watchdog, 30 s (`firmware/p4-bridge/src/update.rs`, `TRIAL_WATCHDOG`), with
/// room: a bridge up this long on the new image has outlived it, so a host confirmed the image.
/// `Ready.uptime_ms` and `Status.uptime_ms` are the C6's, which is never older than the P4's life.
const OUTLIVED_TRIAL_MS: u32 = 35_000;

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
    /// The new image is running and confirmed, known this way.
    Confirmed(Proof),
    /// The radio restarted on its bundled image.
    RadioRestarted,
    /// The radio's update is still running when the wait for it ends.
    RadioStillUpdating,
    /// A line the bridge sent meanwhile.
    Line(Line),
}

/// A line the bridge sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// An `Error` message.
    Fault(String),
    /// A `Log` message.
    Log(String),
}

/// How a new image is known to be confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// The P4's line said so.
    Confirmed,
    /// The line was lost, and the bridge outlived its trial watchdog.
    OutlivedTrial,
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
    /// The bridge runs the new image, and says confirming it failed, for this reason.
    #[error("installed, not confirmed: {why}")]
    NotConfirmed { why: String },
    /// The bridge runs the new image, and no confirm was seen: within `secs` of its `Ready` over
    /// USB, or before the wait over Bluetooth ended (`None`).
    #[error("installed, no confirm seen")]
    NoConfirm { secs: Option<u64> },
    /// Over Bluetooth, the wait ended with no new life judged.
    #[error("the bridge did not come back")]
    NotBack,
}

/// Update the bridge on `link`, reached over `over`, to `image`. `running` is the image the
/// bridge's `Ready` named (`BridgeInfo::image`). Opening the link and waiting for that `Ready`
/// are the caller's. `say` is `Send`, so a phone's binding runs the update as a task of its own
/// runtime.
///
/// # Errors
/// How the update failed.
pub async fn update(
    link: &mut LinkHandle,
    over: Link,
    running: Option<[u8; DIGEST_LEN]>,
    image: &BridgeImage,
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
    update.send(link, say).await?;
    say(Progress::Installing);
    if bluetooth {
        update.settle_bluetooth(link, say).await?;
        return Ok(Updated::Installed);
    }
    let back = update.come_back(link, say).await?;
    match judge(&digest, running, back.image) {
        Outcome::Installed => {}
        Outcome::RolledBack => return Err(Failed::RolledBack),
        Outcome::Other => return Err(Failed::OtherImage),
    }
    match await_confirm(link, say).await? {
        Confirm::Confirmed => say(Progress::Confirmed(Proof::Confirmed)),
        Confirm::Failed(why) => return Err(Failed::NotConfirmed { why }),
        Confirm::Unseen => return Err(Failed::NoConfirm { secs: Some(CONFIRM_WAIT.as_secs()) }),
    }
    follow_radio(link, say).await;
    Ok(Updated::Installed)
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
}

impl<'a> Update<'a> {
    fn new(
        image: &'a [u8],
        sha256: [u8; DIGEST_LEN],
        digest: &[u8; DIGEST_LEN],
        before: Option<[u8; DIGEST_LEN]>,
    ) -> Self {
        let sender = PacedSender::new(Sender::new(image, sha256, digest));
        Self { sender, start: Instant::now(), sent: *digest, before }
    }

    /// Why the update stopped, with whether the bridge named an image before it.
    fn failed(&self, failure: Failure) -> Failed {
        Failed::Sender { failure, image_known: self.before.is_some() }
    }

    /// Send the image until the P4 is told to install.
    async fn send(
        &mut self,
        link: &mut LinkHandle,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<(), Failed> {
        let mut tick = tokio::time::interval(TICK);
        let mut written = link.written();
        loop {
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
                _ = tick.tick() => {}
                () = written.changed(), if self.sender.is_waiting() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(message)) => self.take(message, say),
                    Some(LinkEvent::Disconnected { reason, .. }) => {
                        return Err(Failed::Dropped { reason });
                    }
                    Some(LinkEvent::Connected(info)) => {
                        if on_ready(Phase::Sending, self.before, info.image) == OnReady::Abort {
                            return Err(Failed::RestartedIntoOther);
                        }
                    }
                    Some(LinkEvent::Garbled(_)) => {}
                    None => return Err(Failed::Closed),
                },
            }
        }
    }

    /// Wait out the reboot: `Install` again until the new life's `Ready`, with a poll every
    /// [`POLL`] so the rebooted P4 hears a host.
    async fn come_back(
        &mut self,
        link: &mut LinkHandle,
        say: &mut (dyn FnMut(Progress) + Send),
    ) -> Result<BridgeInfo, Failed> {
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
                    _ = poll.tick() => poll_status(link),
                    event = link.recv() => match event {
                        Some(LinkEvent::Connected(info)) => {
                            if on_ready(Phase::Installing, self.before, info.image)
                                == OnReady::Back
                            {
                                return Ok(info);
                            }
                        }
                        Some(LinkEvent::Message(message)) => self.take(message, say),
                        // The transport reopens the port on its own.
                        Some(LinkEvent::Disconnected { .. } | LinkEvent::Garbled(_)) => {}
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
                _ = poll.tick(), if settling.up() => poll_status(link),
                event = link.recv() => match event {
                    Some(LinkEvent::Connected(info)) => {
                        // A trial image is confirmed by the first host frame it decodes.
                        poll_status(link);
                        let back = settling.came_back();
                        let radio = settling.radio_updated();
                        let ready = Seen::Ready { image: info.image, uptime_ms: info.uptime_ms };
                        settling.see(ready, Instant::now());
                        if !back && settling.came_back() {
                            deadline = tokio::time::Instant::now() + SETTLE_BLUETOOTH;
                        } else if back && radio {
                            say(Progress::RadioRestarted);
                        }
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
                        let now = Instant::now();
                        if let Some(confirm) = confirm_of(&message) {
                            settling.see(Seen::Confirm(confirm), now);
                            continue;
                        }
                        if is_radio_update(&message) {
                            settling.see(Seen::RadioUpdating, now);
                        }
                        if let BridgeToHost::Status { uptime_ms, .. } = &message {
                            settling.see(Seen::Uptime(*uptime_ms), now);
                        }
                        line(&message, say);
                    }
                    Some(LinkEvent::Garbled(_)) => {}
                    None => return Err(Failed::Closed),
                },
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
            message => line(&message, say),
        }
    }
}

/// Hand on a bridge's log or error line; anything else is not a line.
fn line(message: &BridgeToHost, say: &mut (dyn FnMut(Progress) + Send)) {
    match message {
        BridgeToHost::Error { message } => say(Progress::Line(Line::Fault(message.to_string()))),
        BridgeToHost::Log { message } => say(Progress::Line(Line::Log(message.to_string()))),
        _ => {}
    }
}

/// Ask for a status, so the P4 hears a host. Dropped when the queue is full: the next poll
/// asks again.
fn poll_status(link: &LinkHandle) {
    let _ = link.send_bulk(HostToBridge::GetStatus);
}

/// The next event, polling every [`POLL`] while waiting, or `None` at `until`.
async fn next_event(
    link: &mut LinkHandle,
    poll: &mut tokio::time::Interval,
    until: tokio::time::Instant,
) -> Option<Option<LinkEvent>> {
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(until) => return None,
            _ = poll.tick() => poll_status(link),
            event = link.recv() => return Some(event),
        }
    }
}

/// Hand on what the C6's update says, until its new life or [`QUIET`] without a line about it.
async fn follow_radio(link: &mut LinkHandle, say: &mut (dyn FnMut(Progress) + Send)) {
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
                line(&message, say);
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

/// Whether `message` is the P4's line about updating its C6 (`wartui_proto::split`,
/// `Pair::fault`).
fn is_radio_update(message: &BridgeToHost) -> bool {
    matches!(message, BridgeToHost::Error { message } if message.starts_with(RADIO_UPDATING))
}

/// What the P4 said about confirming its new image.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Confirm {
    /// A host frame confirmed it.
    Confirmed,
    /// Confirming failed, for this reason.
    Failed(String),
    /// No line about it arrived within [`CONFIRM_WAIT`].
    Unseen,
}

/// Wait up to [`CONFIRM_WAIT`] for the P4's line about confirming its image, handing on any
/// other line on the way.
async fn await_confirm(
    link: &mut LinkHandle,
    say: &mut (dyn FnMut(Progress) + Send),
) -> Result<Confirm, Failed> {
    let deadline = tokio::time::Instant::now() + CONFIRM_WAIT;
    let mut poll = tokio::time::interval(POLL);
    loop {
        let Some(event) = next_event(link, &mut poll, deadline).await else {
            return Ok(Confirm::Unseen);
        };
        match event {
            Some(LinkEvent::Message(message)) => match confirm_of(&message) {
                Some(confirm) => return Ok(confirm),
                None => line(&message, say),
            },
            Some(_) => {}
            None => return Err(Failed::Closed),
        }
    }
}

/// The P4's line about confirming its image, if `message` is one.
fn confirm_of(message: &BridgeToHost) -> Option<Confirm> {
    match message {
        BridgeToHost::Log { message, .. } if message.as_str() == IMAGE_CONFIRMED => {
            Some(Confirm::Confirmed)
        }
        BridgeToHost::Error { message } if message.starts_with(IMAGE_NOT_CONFIRMED) => {
            Some(Confirm::Failed(message.as_str().to_owned()))
        }
        _ => None,
    }
}

/// What [`Settling`] has come to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Settled {
    /// Not yet.
    Waiting,
    /// A `Ready` from the P4's life before the install: the install never reached it.
    Lost,
    /// On the new image, confirmed as this says.
    Installed(Proof),
    /// Back on another image than the one sent: rolled back, or neither.
    Not(Outcome),
    /// On the new image, which the P4 failed to confirm, for this reason.
    NotConfirmed(String),
    /// On the new image, at the deadline, with no sign it was confirmed.
    Unconfirmed,
}

/// One thing the link showed, for [`Settling`].
#[derive(Debug, Clone, PartialEq, Eq)]
enum Seen {
    /// The link dropped.
    Down,
    /// A `Ready`: the P4's image and the C6's uptime.
    Ready { image: Option<[u8; DIGEST_LEN]>, uptime_ms: u32 },
    /// The P4's line about confirming its image.
    Confirm(Confirm),
    /// A line about the C6's update.
    RadioUpdating,
    /// A `Status`'s uptime, the C6's.
    Uptime(u32),
}

/// What an update over Bluetooth has shown since the install, read across reconnects. Pure: the
/// caller passes the time each thing was seen.
#[derive(Debug)]
struct Settling {
    sent: [u8; DIGEST_LEN],
    before: Option<[u8; DIGEST_LEN]>,
    installed_at: Instant,
    /// The image the latest `Ready` from a life after the install named, once one has.
    image: Option<Option<[u8; DIGEST_LEN]>>,
    /// A `Ready` from the life before the install came first.
    lost: bool,
    /// The link is connected.
    up: bool,
    /// What the P4 said about confirming the new image.
    confirm: Option<Confirm>,
    /// When the link last came up or dropped, or a line about the C6's update arrived.
    news: Instant,
    /// A line about the C6's update arrived since the latest `Ready`.
    radio: bool,
    /// The latest `Status` uptime since that `Ready`.
    uptime_ms: Option<u32>,
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
            image: None,
            lost: false,
            up: true,
            confirm: None,
            news: installed_at,
            radio: false,
            uptime_ms: None,
        }
    }

    /// Whether a life after the install has announced itself.
    fn came_back(&self) -> bool {
        self.image.is_some()
    }

    fn up(&self) -> bool {
        self.up
    }

    /// Whether the link has been up, with no line about the C6's update, for [`QUIET`].
    fn quiet(&self, now: Instant) -> bool {
        self.up && now.duration_since(self.news) >= QUIET
    }

    /// Whether a line about the C6's update arrived since the latest `Ready`.
    fn radio_updated(&self) -> bool {
        self.radio
    }

    /// Whether the C6's update may still be running at `now`: a line about it arrived since the
    /// latest `Ready`, and the link has not been quiet since.
    fn radio_running(&self, now: Instant) -> bool {
        self.radio && !self.quiet(now)
    }

    fn see(&mut self, seen: Seen, now: Instant) {
        match seen {
            Seen::Down => {
                self.up = false;
                self.news = now;
            }
            Seen::Ready { image, uptime_ms } => {
                self.up = true;
                self.news = now;
                self.radio = false;
                // The P4 resets its C6 at boot, so a C6 older than the install is the P4's life
                // before it, which dropped its update session when the link went.
                let older =
                    u128::from(uptime_ms) > now.duration_since(self.installed_at).as_millis();
                if older && !self.came_back() {
                    self.lost = true;
                    return;
                }
                self.image = Some(image);
                self.uptime_ms = None;
            }
            Seen::Confirm(confirm) => self.confirm = Some(confirm),
            Seen::RadioUpdating => {
                self.radio = true;
                self.news = now;
            }
            Seen::Uptime(ms) => {
                if self.came_back() {
                    self.uptime_ms = Some(ms);
                }
            }
        }
    }

    /// Where the update stands at `now`, or at the deadline when `deadline`, which no longer
    /// waits for the link to settle.
    fn settled(&self, now: Instant, deadline: bool) -> Settled {
        if self.lost {
            return Settled::Lost;
        }
        let Some(image) = self.image else { return Settled::Waiting };
        match judge(&self.sent, self.before, image) {
            Outcome::Installed => {}
            other => return Settled::Not(other),
        }
        if let Some(Confirm::Failed(why)) = &self.confirm {
            return Settled::NotConfirmed(why.clone());
        }
        if !deadline && !self.quiet(now) {
            return Settled::Waiting;
        }
        match (&self.confirm, self.uptime_ms) {
            (Some(Confirm::Confirmed), _) => Settled::Installed(Proof::Confirmed),
            (_, Some(ms)) if ms >= OUTLIVED_TRIAL_MS => Settled::Installed(Proof::OutlivedTrial),
            _ if deadline => Settled::Unconfirmed,
            _ => Settled::Waiting,
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
        Settled::Installed(proof) => {
            say(Progress::Confirmed(proof));
            if radio_running {
                say(Progress::RadioStillUpdating);
            }
            Ok(())
        }
        Settled::Lost => Err(Failed::NeverTookInstall),
        Settled::Not(Outcome::RolledBack) => Err(Failed::RolledBack),
        Settled::Not(_) => Err(Failed::OtherImage),
        Settled::NotConfirmed(why) => Err(Failed::NotConfirmed { why }),
        Settled::Unconfirmed => Err(Failed::NoConfirm { secs: None }),
        Settled::Waiting => Err(Failed::NotBack),
    }
}

/// What the P4 came back running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    /// The new image.
    Installed,
    /// The image it ran before the update.
    RolledBack,
    /// Neither, or none it could name.
    Other,
}

/// Judge the life that followed the install by its `Ready.image`, against the image sent and the
/// one the P4 ran before.
fn judge(
    sent: &[u8; DIGEST_LEN],
    before: Option<[u8; DIGEST_LEN]>,
    after: Option<[u8; DIGEST_LEN]>,
) -> Outcome {
    match after {
        Some(after) if after == *sent => Outcome::Installed,
        Some(after) if Some(after) == before => Outcome::RolledBack,
        _ => Outcome::Other,
    }
}

#[cfg(test)]
mod tests {
    use wartui_bridge::{DisconnectCause, LinkPlumbing, link_pair};
    use wartui_proto::bus::{Board, NodeBoard};
    use wartui_proto::link::{LogStr, LoopPhase, UPDATE_STATUS_MAX, UpdateStatusBytes};
    use wartui_proto::ota::{Receiver, SECTOR, SectorSink};
    use wartui_proto::radio_link::update::{Received, receive};
    use wartui_proto::reset::ResetCause;

    use super::super::image::BRIDGE_PROJECT;
    use super::super::testing::{P4_APP_HEADER, fixture_with};
    use super::*;

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

    const NEW: [u8; DIGEST_LEN] = [0x11; DIGEST_LEN];
    const OLD: [u8; DIGEST_LEN] = [0x22; DIGEST_LEN];

    #[test]
    fn update_bridge_judges_installed_when_ready_names_the_sent_image() {
        assert_eq!(judge(&NEW, Some(OLD), Some(NEW)), Outcome::Installed);
    }

    #[test]
    fn update_bridge_judges_rolled_back_when_ready_names_the_old_image() {
        assert_eq!(judge(&NEW, Some(OLD), Some(OLD)), Outcome::RolledBack);
    }

    #[test]
    fn update_bridge_judges_other_when_ready_names_neither_image() {
        assert_eq!(judge(&NEW, Some(OLD), Some([0x33; DIGEST_LEN])), Outcome::Other);
        assert_eq!(judge(&NEW, Some(OLD), None), Outcome::Other);
        assert_eq!(judge(&NEW, None, None), Outcome::Other);
    }

    /// An update to `NEW` over Bluetooth from `OLD`, installed at `at`.
    fn settling(at: Instant) -> Settling {
        Settling::new(NEW, Some(OLD), at)
    }

    /// `ms` after `at`.
    fn after(at: Instant, ms: u64) -> Instant {
        at + Duration::from_millis(ms)
    }

    #[test]
    fn update_bridge_settles_installed_when_new_image_is_back_confirmed_and_quiet() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Confirm(Confirm::Confirmed), after(at, 4_100));
        assert_eq!(s.settled(after(at, 6_000), false), Settled::Waiting, "not yet quiet");
        assert_eq!(s.settled(after(at, 9_000), false), Settled::Installed(Proof::Confirmed));
    }

    #[test]
    fn update_bridge_keeps_waiting_when_radio_update_drops_the_link_after_confirm() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Confirm(Confirm::Confirmed), after(at, 4_100));
        s.see(Seen::RadioUpdating, after(at, 8_000));
        assert_eq!(s.settled(after(at, 12_000), false), Settled::Waiting, "a line 4 s ago");
        s.see(Seen::Down, after(at, 20_000));
        assert_eq!(s.settled(after(at, 30_000), false), Settled::Waiting, "the link is down");
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 2_000 }, after(at, 33_000));
        assert_eq!(s.settled(after(at, 38_000), false), Settled::Installed(Proof::Confirmed));
    }

    #[test]
    fn update_bridge_omits_radio_line_when_link_is_down_at_the_deadline_with_no_radio_update() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Confirm(Confirm::Confirmed), after(at, 4_100));
        s.see(Seen::Down, after(at, 6_000));
        let deadline = after(at, 154_000);
        assert_eq!(s.settled(deadline, true), Settled::Installed(Proof::Confirmed));
        assert!(!s.radio_running(deadline), "nothing said the radio was updating");
    }

    #[test]
    fn update_bridge_says_radio_line_when_link_is_down_at_the_deadline_after_a_radio_update() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Confirm(Confirm::Confirmed), after(at, 4_100));
        s.see(Seen::RadioUpdating, after(at, 5_000));
        s.see(Seen::Down, after(at, 6_000));
        assert!(s.radio_running(after(at, 154_000)));
        // The C6's new life ends it.
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_000 }, after(at, 154_000));
        assert!(!s.radio_running(after(at, 154_000)));
    }

    #[test]
    fn update_bridge_says_confirmed_before_radio_line_when_settled_with_radio_running() {
        let mut said = Vec::new();
        let ended = settled_end(Settled::Installed(Proof::OutlivedTrial), true, &mut |progress| {
            said.push(progress);
        });
        assert_eq!(ended, Ok(()));
        let expected = [Progress::Confirmed(Proof::OutlivedTrial), Progress::RadioStillUpdating];
        assert_eq!(said, expected);
    }

    #[test]
    fn update_bridge_ends_in_each_failure_when_settled_short_of_installed() {
        let mut say = |_: Progress| panic!("a failure says nothing");
        let mut ended = |settled| settled_end(settled, false, &mut say);
        assert_eq!(ended(Settled::Lost), Err(Failed::NeverTookInstall));
        assert_eq!(ended(Settled::Not(Outcome::RolledBack)), Err(Failed::RolledBack));
        assert_eq!(ended(Settled::Not(Outcome::Other)), Err(Failed::OtherImage));
        let why = "why".to_owned();
        assert_eq!(ended(Settled::NotConfirmed(why.clone())), Err(Failed::NotConfirmed { why }));
        assert_eq!(ended(Settled::Unconfirmed), Err(Failed::NoConfirm { secs: None }));
        assert_eq!(ended(Settled::Waiting), Err(Failed::NotBack));
    }

    #[test]
    fn update_bridge_judges_from_latest_ready_when_bridge_reconnects_on_old_image() {
        // The new image announced itself, then its trial watchdog rolled it back.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Down, after(at, 34_000));
        s.see(Seen::Ready { image: Some(OLD), uptime_ms: 1_500 }, after(at, 70_000));
        assert_eq!(s.settled(after(at, 70_000), false), Settled::Not(Outcome::RolledBack));
    }

    #[test]
    fn update_bridge_reports_install_lost_when_ready_predates_the_install() {
        // The link dropped before the P4 took the `Install`, so it dropped its session and
        // never rebooted: its C6 has been up since before the install.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        s.see(Seen::Ready { image: Some(OLD), uptime_ms: 600_000 }, after(at, 3_000));
        assert!(!s.came_back());
        assert_eq!(s.settled(after(at, 3_000), false), Settled::Lost, "at once");
    }

    #[test]
    fn update_bridge_reports_rolled_back_when_trial_boots_end_on_old_image() {
        // Each trial boot runs until the P4's 30 s watchdog, then the link takes a while to
        // drop and come back. No confirm is seen, and the third boot is the old image.
        const TRIAL_MS: u64 = 30_000;
        const RECONNECT_MS: u64 = 15_000;
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        let first_back = 20_000;
        let mut ready_at = first_back;
        for _ in 0..2 {
            s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, ready_at));
            s.see(Seen::Uptime(20_000), after(at, ready_at + 20_000));
            assert_eq!(s.settled(after(at, ready_at + 20_000), false), Settled::Waiting);
            s.see(Seen::Down, after(at, ready_at + TRIAL_MS));
            ready_at += TRIAL_MS + RECONNECT_MS;
        }
        s.see(Seen::Ready { image: Some(OLD), uptime_ms: 1_500 }, after(at, ready_at));
        assert_eq!(s.settled(after(at, ready_at), false), Settled::Not(Outcome::RolledBack));
        // Within the wait that follows the first new life, so it is never reported unconfirmed.
        assert!(Duration::from_millis(ready_at - first_back) + QUIET < SETTLE_BLUETOOTH);
    }

    #[test]
    fn update_bridge_counts_image_confirmed_when_line_was_lost_and_bridge_outlived_its_trial() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Uptime(20_000), after(at, 23_000));
        assert_eq!(s.settled(after(at, 23_000), false), Settled::Waiting);
        s.see(Seen::Uptime(OUTLIVED_TRIAL_MS), after(at, 38_000));
        assert_eq!(s.settled(after(at, 38_000), false), Settled::Installed(Proof::OutlivedTrial));
    }

    #[test]
    fn update_bridge_reports_unconfirmed_at_deadline_when_no_sign_of_confirm_arrived() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Uptime(9_000), after(at, 12_000));
        assert_eq!(s.settled(after(at, 154_000), true), Settled::Unconfirmed);
    }

    #[test]
    fn update_bridge_reports_failed_confirm_at_once_when_bridge_says_so() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        let why = format!("{IMAGE_NOT_CONFIRMED}: flash write failed");
        s.see(Seen::Confirm(Confirm::Failed(why.clone())), after(at, 4_100));
        assert_eq!(s.settled(after(at, 4_100), false), Settled::NotConfirmed(why));
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

    #[test]
    fn update_bridge_reads_confirm_when_bridge_says_image_confirmed() {
        assert_eq!(confirm_of(&log(IMAGE_CONFIRMED)), Some(Confirm::Confirmed));
    }

    #[test]
    fn update_bridge_reads_failed_confirm_when_bridge_reports_error() {
        let line = format!("{IMAGE_NOT_CONFIRMED}: flash write failed");
        assert_eq!(confirm_of(&error(&line)), Some(Confirm::Failed(line)));
    }

    #[test]
    fn update_bridge_ignores_line_when_it_is_not_about_the_confirm() {
        assert_eq!(confirm_of(&log("trial boot 1 of 3")), None);
        // The confirm's text as an error, or its failure as a log, is not the line.
        assert_eq!(confirm_of(&error(IMAGE_CONFIRMED)), None);
        assert_eq!(confirm_of(&log(IMAGE_NOT_CONFIRMED)), None);
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

    /// The P4 bridge's `Ready` naming `image`, from a C6 up `uptime_ms`.
    fn ready(image: Option<[u8; DIGEST_LEN]>, uptime_ms: u32) -> BridgeInfo {
        BridgeInfo {
            mac: [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0],
            fw_version: "0.1.0".to_owned(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms,
            node_board: NodeBoard::Carrier14,
            radio: None,
            image,
            version: wartui_proto::link::BRIDGE_VERSION,
            trial: wartui_proto::link::Trial::Settled,
        }
    }

    /// A P4 at the bridge's end of the link: it takes the update into a slot as its receiver
    /// does, and on `Install` reboots into `boots`, announcing it and then saying it confirmed
    /// the image. Over Bluetooth the link drops first, and the new life's C6 is just up. Unless it
    /// `confirms`, it drops its end of the link after the new life instead of saying so. Every
    /// command counts done.
    fn scripted_p4(
        mut plumbing: LinkPlumbing,
        over: Link,
        boots: [u8; DIGEST_LEN],
        confirms: bool,
    ) {
        tokio::spawn(async move {
            let mut receiver = Receiver::new(Board::Esp32p4);
            let mut slot = Slot(vec![0xFF; 64 * SECTOR]);
            let mut rebooted = false;
            while let Some(cmd) = plumbing.commands.recv().await {
                plumbing.commands.done(1);
                let HostToBridge::Update(bytes) = cmd else { continue };
                if rebooted {
                    continue;
                }
                let op = Op::decode(&bytes).expect("an op");
                let status = match receive(&mut receiver, op, Some(&mut slot), false) {
                    Received::Status(status) => status,
                    Received::Install(_) => {
                        rebooted = true;
                        let confirmed = BridgeToHost::Log {
                            message: LogStr::try_from(IMAGE_CONFIRMED).unwrap(),
                        };
                        let mut events = Vec::new();
                        if over == Link::Bluetooth {
                            let reason = "the bridge disconnected".to_owned();
                            let cause = DisconnectCause::Other;
                            events.push(LinkEvent::Disconnected { reason, cause });
                            events.push(LinkEvent::Connected(ready(Some(boots), 0)));
                        } else {
                            events.push(LinkEvent::Connected(ready(Some(boots), 800)));
                        }
                        if confirms {
                            events.push(LinkEvent::Message(confirmed));
                        }
                        for event in events {
                            if plumbing.events.send(event).await.is_err() {
                                return;
                            }
                        }
                        if !confirms {
                            return;
                        }
                        continue;
                    }
                };
                if let Some(status) = status {
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
    }

    /// The P4 bridge's app image, a few sectors long.
    fn bridge_image() -> BridgeImage {
        let bytes = fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 3 * SECTOR + 1_024);
        BridgeImage::parse(bytes).unwrap()
    }

    /// Update over USB from `OLD` to [`bridge_image`], against a P4 that boots into `boots`, and
    /// what was said besides each `Sending`.
    async fn update_over_usb(
        boots: impl FnOnce(&BridgeImage) -> [u8; DIGEST_LEN],
    ) -> (Result<Updated, Failed>, Vec<Progress>, usize) {
        let image = bridge_image();
        let (mut link, plumbing) = link_pair();
        scripted_p4(plumbing, Link::Usb, boots(&image), true);
        let mut said = Vec::new();
        let mut sending = 0;
        let updated = update(&mut link, Link::Usb, Some(OLD), &image, |progress| match progress {
            Progress::Sending { .. } if said.is_empty() => sending += 1,
            progress => said.push(progress),
        })
        .await;
        (updated, said, sending)
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_installs_image_when_bridge_comes_back_on_it_confirmed() {
        let (updated, said, sending) = update_over_usb(BridgeImage::digest).await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert!(sending > 0, "the send was said");
        assert_eq!(said, [Progress::Installing, Progress::Confirmed(Proof::Confirmed)]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_fails_rolled_back_when_bridge_comes_back_on_the_old_image() {
        let (updated, said, _) = update_over_usb(|_| OLD).await;
        assert_eq!(updated, Err(Failed::RolledBack));
        assert_eq!(said, [Progress::Installing]);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_installs_image_when_bluetooth_link_drops_and_comes_back_confirmed() {
        // `Settling` reads `std::time::Instant`, which does not advance under a paused clock. So
        // the new life's C6 is up 0 ms, since any more reads as older than the install and ends
        // in `NeverTookInstall`, and the link is never quiet for `QUIET`: the update ends at the
        // tokio deadline, `SETTLE_BLUETOOTH` after the new life, through `settled(now, true)`,
        // which is `Installed(Confirmed)` because the confirm line was seen. The script answers
        // no `StopScan`, so the scan is not reported stopped first.
        let image = bridge_image();
        let (mut link, plumbing) = link_pair();
        scripted_p4(plumbing, Link::Bluetooth, image.digest(), true);
        let mut said = Vec::new();
        let mut sending = 0;
        let updated =
            update(&mut link, Link::Bluetooth, Some(OLD), &image, |progress| match progress {
                Progress::Sending { .. } => sending += 1,
                progress => said.push(progress),
            })
            .await;
        assert_eq!(updated, Ok(Updated::Installed));
        assert!(sending > 0, "the send was said");
        let expected = [
            Progress::ScanNotStopped,
            Progress::Installing,
            Progress::LinkDropped,
            Progress::Confirmed(Proof::Confirmed),
        ];
        assert_eq!(said, expected);
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_fails_closed_when_link_closes_before_the_confirm() {
        let image = bridge_image();
        let (mut link, plumbing) = link_pair();
        scripted_p4(plumbing, Link::Usb, image.digest(), false);
        let mut said = Vec::new();
        let updated = update(&mut link, Link::Usb, Some(OLD), &image, |progress| {
            if !matches!(progress, Progress::Sending { .. }) {
                said.push(progress);
            }
        })
        .await;
        assert_eq!(updated, Err(Failed::Closed));
        assert_eq!(said, [Progress::Installing], "never confirmed");
    }

    #[tokio::test(start_paused = true)]
    async fn update_bridge_sends_nothing_when_bridge_already_runs_the_image() {
        let image = bridge_image();
        let (mut link, mut plumbing) = link_pair();
        let running = Some(image.digest());
        let updated = update(&mut link, Link::Bluetooth, running, &image, |_| panic!("said")).await;
        assert_eq!(updated, Ok(Updated::AlreadyCurrent));
        assert!(plumbing.commands.try_recv().is_none());
    }
}
