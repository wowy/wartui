//! `wartui flash-bridge` on a P4 bridge: its app image, sent over the link it bridges on.
//!
//! **The link the host already reaches the bridge over carries the update.** Over USB the board's
//! Type-C reaches the P4's UART0 through a CH343, the port the first flash also uses
//! (`firmware/p4-bridge/README.md` § "First flash"); over Bluetooth the C6 forwards the host's
//! frames to the P4. After the first flash the P4 takes its app image as
//! `HostToBridge::Update` ops: the `radio_link::update` layout it sends its own C6, driven by the
//! same `Sender`, on a [`TICK`], on each status and as the link writes what it queued. Host bytes
//! the P4 loses while it writes a sector cost nothing: the sender waits for each sector's status
//! and rewinds on what it says. The sender is paced to the link (`wartui_bridge::pace`), so at
//! most a sector's ops are queued, and a full queue is an error.
//!
//! **The image is checked before it is sent**: the P4's chip, an app-only layout, the
//! descriptor's project name ([`PROJECT`]), the segment checksum and appended SHA-256
//! (`app::check_integrity`), and the size of the P4's app slot ([`SLOT_LEN`]). The P4's receiver
//! checks only that the bytes arrived whole, so a wrong or damaged file would otherwise cost a
//! transfer, a reboot and a rollback.
//!
//! **A `Ready` is not always the P4 rebooting.** The P4 announces a new life when only its C6
//! restarts, since `Ready.uptime_ms` is the C6's. While sending, a `Ready` naming the image the
//! P4 ran before is that, and the send goes on: a P4 that did reboot dropped the session, which
//! the sender notices. One naming another image means the P4 is no longer running the image the
//! send began against, so the send stops. After `Install`, the next new life is the P4's
//! ([`on_ready`]): over USB its CH343 stays on the bus, so the connection holds through the
//! reboot, and the transport reports the new life by its uptime falling.
//!
//! **`Ready.image` names the outcome.** The P4 installs and reboots into the new image on trial,
//! which writes nothing until a host frame arrives. So from the install on, the host sends a
//! `GetStatus` every [`POLL`]; the first frame after the C6's hello confirms the image. A `Ready`
//! carrying the new digest is installed. One carrying the old is a rollback: the P4's trial
//! watchdog resets a hung image twice, and the third boot returns it to the old one, which is why
//! the wait is [`COME_BACK`]. The bootloader's log after each reset arrives as garbage, which a
//! held connection passes over.
//!
//! **The confirm is reported, not assumed.** The P4 says whether a host frame confirmed its
//! image in a line after its `Ready`, in the text `wartui_proto::link` holds for both ends. Only
//! that line is "confirmed": an image whose confirm failed, or whose line never came, is still on
//! trial and rolls back unless a host confirms it. The polls go on until the C6's update is
//! followed, so the P4's gate stays open for its lines.
//!
//! **Then the C6.** A new P4 image carries its C6's image, which the P4 sends the C6 at boot.
//! Its progress arrives as error lines, printed until the C6's new life, or until [`QUIET`]
//! passes without one.
//!
//! **Over Bluetooth the link drops at each restart, and the outcome is read across the
//! reconnects** ([`Settling`]). The P4 resets its C6 at boot, so its reboot ends the connection,
//! and the C6's own update ends it again; the transport reconnects on its own each time. Before
//! `Begin` the scan is stopped, as `ota` stops it, so the stream toward the host leaves the C6's
//! small packet pool to the writes; the P4 boots stopped anyway. A drop while the image is sent
//! ends the run, since the P4 drops its update session when its host goes, and the bridge keeps
//! its image. After `Install`:
//!
//! - A `Ready` whose C6 is older than the install is the P4's life before it: the install never
//!   reached the P4, which dropped its session with the link. The run ends at once. A C6 that
//!   restarts alone at that moment comes back younger than the install, and the run reads that
//!   as a rollback.
//! - Any other `Ready` is a life after the install, and the run is judged on the latest one's
//!   image, so trial boots ending on the old image read as a rollback. Each reconnect is answered
//!   with a `GetStatus`, the frame that confirms a trial image. The sender is pumped, `Install`
//!   again, only while the link is up and no new life has answered.
//! - The run ends once the image is confirmed and the link has stayed up for [`QUIET`] with no
//!   line about the C6's update. Neither `Ready` nor `RadioInfo` names the C6's image, so that
//!   quiet is the C6's outcome. The confirm's line can be lost to a drop, so a bridge on the new
//!   image whose uptime passes its trial watchdog ([`OUTLIVED_TRIAL_MS`]) counts as confirmed: an
//!   image no host confirmed would have reset.
//! - It waits [`COME_BACK_BLUETOOTH`] for a new life, then [`SETTLE_BLUETOOTH`] after the first
//!   one, long enough for three trial boots to end in a rollback.
//!
//! A failed update over Bluetooth is recovered over USB, by running `flash-bridge` there; nothing
//! resumes a partly sent image.
//!
//! **A P4 that cannot take an update names the fix.** One running firmware from another tree
//! announces a `Ready` this build cannot decode, and one on espflash's default partition table
//! refuses the `Begin`. Both need one last wired flash. A P4 that sends nothing at all is more
//! likely held by another program, or still starting, so that advice is kept for a `Ready` that
//! arrived garbled.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wartui_bridge::pace::PacedSender;
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::SerialTransport;
use wartui_bridge::spec::BridgeSpec;
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};
use wartui_core::runtime::stop_scan;
use wartui_proto::bus::UpdateState;
use wartui_proto::link::{
    BridgeToHost, HostToBridge, IMAGE_CONFIRMED, IMAGE_NOT_CONFIRMED, RADIO_UPDATING,
};
use wartui_proto::mac::{self, Mac};
use wartui_proto::ota::image::{DIGEST_LEN, appended_digest_of};
use wartui_proto::radio_link::update::{Failure, Op, Sender, Stage};

use super::bridge::Args;
use super::{
    Chip, Firmware, RELEASE_TAG, Source, app, check_image, espflash_present, obtain, read, source,
};
use crate::config::Link;

/// The P4 bridge's app-only image, which `flash-bridge` sends it over its link. Its merged image
/// is for the first flash by hand (`firmware/p4-bridge/README.md`), so nothing here fetches it.
const P4_BRIDGE_APP: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/p4-bridge"),
    elf: "target/riscv32imafc-unknown-none-elf/release/wartui-p4-bridge-fw",
    what: "P4 bridge",
    chip: Chip::Esp32p4,
    partition_table: "partitions.csv",
    merged: false,
    asset: "wartui-p4-bridge-app-esp32p4.bin",
};

/// The project name the P4 bridge's app descriptor carries: its crate's name.
const PROJECT: &str = "wartui-p4-bridge-fw";

/// The largest image the P4's app slot holds: `ota_0` and `ota_1` in
/// `firmware/p4-bridge/partitions.csv`.
const SLOT_LEN: usize = 0x1E_0000;

/// How often the sender is asked for its next op, besides on each status.
const TICK: Duration = Duration::from_millis(50);

/// How often the host sends a `GetStatus` from the install on: a rebooted P4 writes nothing
/// until a host frame arrives, and its gate closes after 15 s without one.
const POLL: Duration = Duration::from_secs(1);

/// The longest to wait for the P4's `Ready`. It announces once its C6 answers, or after 2 s
/// without. The transport's first failure ends the wait sooner: a held port at once, and a silent
/// lone board after the transport's 6 s.
const READY_WAIT: Duration = Duration::from_secs(10);

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

/// The README section that covers a wired flash.
pub(crate) const FIRST_FLASH: &str = "firmware/p4-bridge/README.md § \"First flash\"";

/// Which bridge to update, and over which link.
pub(super) enum Target {
    /// The P4 bridge on `board`, which answered as `expected`, over its CH343.
    Usb { board: PortCandidate, expected: Mac },
    /// The bridge `bridge` names, over Bluetooth, with the remembered bridge in `memory`.
    Bluetooth { bridge: BridgeSpec, memory: BridgeMemory },
}

impl Target {
    /// The bridge, as the lines name it before it has answered.
    fn label(&self) -> String {
        match self {
            Self::Usb { expected, .. } => mac::short(expected).to_string(),
            Self::Bluetooth { bridge: BridgeSpec::Mac(mac), .. } => mac::short(mac).to_string(),
            Self::Bluetooth { bridge, .. } => bridge.to_string(),
        }
    }
}

/// Update the P4 bridge `target` names, over its link.
pub(super) async fn run(args: &Args, target: Target) -> Result<()> {
    let label = target.label();
    let source =
        source(&P4_BRIDGE_APP, args.image.as_deref(), args.firmware_dir.as_deref(), RELEASE_TAG);
    println!("image  {}", source.describe(args.dry_run));
    if args.dry_run {
        if let Source::File(path) = &source {
            check_app(&read(path)?).with_context(|| format!("checking {}", path.display()))?;
        }
        match target {
            Target::Usb { .. } => println!("{label:<6} would update over its link"),
            Target::Bluetooth { .. } => println!("{label:<6} would update over Bluetooth"),
        }
        return Ok(());
    }
    if matches!(source, Source::Build(_)) {
        // It saves the built ELF as an image.
        espflash_present()?;
    }
    let path = obtain(&P4_BRIDGE_APP, &source)?;
    let image = read(&path)?;
    check_app(&image).with_context(|| format!("checking {}", path.display()))?;
    let sha256: [u8; DIGEST_LEN] = Sha256::digest(&image).into();
    let Some(digest) = appended_digest_of(&image) else {
        bail!("{} carries no appended digest, which names the image to the P4", path.display());
    };
    crate::interruptible(update_over_link(target, label, &image, sha256, &digest)).await
}

/// Send `image` to the bridge `target` names, from opening its link to the outcome. A ctrl-c
/// stops it and `main` closes the link. The download or build before it blocks the task, so it
/// runs with no ctrl-c handler, and a ctrl-c there ends the process.
async fn update_over_link(
    target: Target,
    label: String,
    image: &[u8],
    sha256: [u8; DIGEST_LEN],
    digest: &[u8; DIGEST_LEN],
) -> Result<()> {
    let bluetooth = matches!(target, Target::Bluetooth { .. });
    let (mut link, info, label) = match target {
        Target::Usb { board, expected } => {
            let (link, info) = open_usb(&label, &board.path).await?;
            ensure!(info.mac == expected, "{label} answered with another address");
            (link, info, label)
        }
        Target::Bluetooth { bridge, memory } => {
            let (link, info) = open_bluetooth(&label, &bridge, memory).await?;
            // The transport checked the `Ready` against the name; its address names the lines.
            let label = mac::short(&info.mac).to_string();
            (link, info, label)
        }
    };
    if info.image == Some(*digest) {
        println!("{label:<6} already runs this image");
        return Ok(());
    }
    if bluetooth && !stop_scan(&mut link, SCAN_STOP_WAIT).await {
        println!("{label:<6} did not report its scan stopped; updating anyway");
    }

    let mut progress = ProgressLine::new(label.clone());
    let mut update = Update::new(image, sha256, digest, info.image);
    update.send(&mut link, &mut progress).await?;
    progress.say("installing; waiting for the bridge to come back");
    if bluetooth {
        return update.settle_bluetooth(&mut link, &mut progress, &label).await;
    }
    let back = update.come_back(&mut link, &mut progress).await?;
    match judge(digest, info.image, back.image) {
        Outcome::Installed => {}
        Outcome::RolledBack => bail!("{label} did not boot the new image; rolled back"),
        Outcome::Other => bail!("{label} came back running another image"),
    }
    let confirm = await_confirm(&mut link, &mut progress).await;
    match confirm_text(&confirm) {
        Ok(text) => progress.say(text),
        Err(text) => bail!("{label} {text}"),
    }
    follow_radio(&mut link, &mut progress).await;
    Ok(())
}

/// Refuse an image that is not the P4 bridge's app, or that is damaged or too large for its
/// slot.
fn check_app(image: &[u8]) -> Result<()> {
    check_image(image, P4_BRIDGE_APP.chip, false)?;
    let project = app::descriptor(image)?.project;
    ensure!(project == PROJECT, "built for project '{project}', and a P4 bridge runs {PROJECT}");
    app::check_integrity(image)?;
    ensure!(
        image.len() <= SLOT_LEN,
        "{} bytes, and the P4's app slot holds {SLOT_LEN}",
        image.len()
    );
    Ok(())
}

/// Open the P4 bridge on `path` over its CH343 and wait for its `Ready`.
async fn open_usb(label: &str, path: &str) -> Result<(LinkHandle, BridgeInfo)> {
    // By path: the probe that chose it has already asked, and the reboot leaves the CH343 where
    // it is.
    let mut link = SerialTransport::with_spec(BridgeSpec::Path(path.to_owned()))
        .remember_in(BridgeMemory::none())
        .start()
        .context("opening the link")?;
    let mut garbled = false;
    match tokio::time::timeout(READY_WAIT, wait_for_ready(&mut link, &mut garbled)).await {
        Ok(Ok(info)) => Ok((link, info)),
        Ok(Err(why)) => bail!("{}", no_ready_text(label, path, &why, garbled)),
        Err(_) => {
            let why = format!("no answer in {} s", READY_WAIT.as_secs());
            bail!("{}", no_ready_text(label, path, &why, garbled))
        }
    }
}

/// Open the bridge `bridge` names over Bluetooth and wait for its `Ready`, through the failed
/// tries the transport makes again on its own, as `wartui status` waits.
async fn open_bluetooth(
    label: &str,
    bridge: &BridgeSpec,
    memory: BridgeMemory,
) -> Result<(LinkHandle, BridgeInfo)> {
    let named = bridge.to_string();
    let mut link = crate::open(Some(&named), None, Link::Bluetooth, memory)?;
    let within = crate::connect_wait(Link::Bluetooth);
    let waited = crate::status::wait_for_ready_or_why(&mut link, Link::Bluetooth, within).await;
    match waited.ready {
        Ok(Some(info)) => Ok((link, info)),
        Err(why) => bail!("{}", no_ready_over_bluetooth(label, &why, waited.garbled)),
        Ok(None) if waited.garbled => {
            let why = format!("no answer in {} s", within.as_secs());
            bail!("{}", no_ready_over_bluetooth(label, &why, waited.garbled))
        }
        Ok(None) => bail!("{}", crate::no_bridge_notice(Some(&named), Link::Bluetooth).await),
    }
}

/// Why no `Ready` arrived from `board` over Bluetooth, and what to do. A refusal or a scan that
/// found nothing arrives as the transport's own reason.
fn no_ready_over_bluetooth(board: &str, why: &str, garbled: bool) -> String {
    if garbled {
        format!(
            "{board} answered over Bluetooth in a form this build cannot read ({why}): it runs \
             firmware from another tree. Flash it once by hand, per {FIRST_FLASH}"
        )
    } else {
        format!("{board} did not announce itself over Bluetooth: {why}")
    }
}

/// Wait for the link's `Ready`, setting `garbled` when a frame arrives that does not decode.
async fn wait_for_ready(link: &mut LinkHandle, garbled: &mut bool) -> Result<BridgeInfo, String> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason, .. }) => return Err(reason),
            Some(LinkEvent::Garbled(_)) => *garbled = true,
            Some(LinkEvent::Message(_)) => {}
            None => return Err("the link closed".to_owned()),
        }
    }
}

/// Why no `Ready` arrived from `board` on `path`, and what to do. Frames that did not decode
/// are a `Ready` from firmware built from another tree; silence is a held port or a bridge
/// still starting.
fn no_ready_text(board: &str, path: &str, why: &str, garbled: bool) -> String {
    if garbled {
        format!(
            "{board} on {path} answered in a form this build cannot read ({why}): it runs \
             firmware from another tree. Flash it once by hand, per {FIRST_FLASH}"
        )
    } else {
        format!(
            "{board} on {path} did not announce itself ({why}). Another program may hold the \
             port, such as a running `wartui run`, or the bridge is still starting; try again"
        )
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

    /// Why the update stopped, and what fixes it.
    fn failed(&self, failure: Failure) -> String {
        failure_text(failure, self.before.is_some())
    }

    /// Send the image until the P4 is told to install.
    async fn send(&mut self, link: &mut LinkHandle, progress: &mut ProgressLine) -> Result<()> {
        let mut tick = tokio::time::interval(TICK);
        let mut written = link.written();
        loop {
            self.pump(link)?;
            match self.sender.stage() {
                Stage::Sending => progress.show(self.sender.percent()),
                Stage::Installed => return Ok(()),
                // The link wrote nothing for the pacing's stall bound: over USB, a port that
                // stopped draining, not a receiver that stopped answering.
                Stage::Failed(Failure::Silent) if self.sender.is_stalled() => {
                    bail!("{}", port_stall_text(self.sender.percent()))
                }
                Stage::Failed(failure) => bail!("{}", self.failed(failure)),
            }
            tokio::select! {
                _ = tick.tick() => {}
                () = written.changed(), if self.sender.is_waiting() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(message)) => self.take(message, progress),
                    Some(LinkEvent::Disconnected { reason, .. }) => bail!(
                        "the link dropped before the update finished ({reason}); the bridge \
                         keeps its image, and flash-bridge can be run again"
                    ),
                    Some(LinkEvent::Connected(info)) => {
                        if on_ready(Phase::Sending, self.before, info.image) == OnReady::Abort {
                            bail!(
                                "the bridge restarted into another image before the update \
                                 finished; run flash-bridge again"
                            );
                        }
                    }
                    Some(LinkEvent::Garbled(_)) => {}
                    None => bail!("the link closed"),
                },
            }
        }
    }

    /// Wait out the reboot: `Install` again until the new life's `Ready`, with a poll every
    /// [`POLL`] so the rebooted P4 hears a host.
    async fn come_back(
        &mut self,
        link: &mut LinkHandle,
        progress: &mut ProgressLine,
    ) -> Result<BridgeInfo> {
        let wait = async {
            let mut tick = tokio::time::interval(TICK);
            let mut poll = tokio::time::interval(POLL);
            let mut written = link.written();
            loop {
                self.pump(link)?;
                if let Stage::Failed(failure) = self.sender.stage() {
                    bail!("{}", self.failed(failure));
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
                        Some(LinkEvent::Message(message)) => self.take(message, progress),
                        // The transport reopens the port on its own.
                        Some(LinkEvent::Disconnected { .. } | LinkEvent::Garbled(_)) => {}
                        None => bail!("the link closed"),
                    },
                }
            }
        };
        match tokio::time::timeout(COME_BACK, wait).await {
            Ok(back) => back,
            Err(_) => bail!(
                "the bridge did not come back within {} s of the install; check its CH343 log",
                COME_BACK.as_secs()
            ),
        }
    }

    /// Over Bluetooth, from the install until the outcome: the link drops as the P4 reboots and
    /// again as its C6 restarts, and [`Settling`] reads the outcome across the reconnects. The
    /// sender is pumped, `Install` again, while the link is up and until a new life answers.
    async fn settle_bluetooth(
        &mut self,
        link: &mut LinkHandle,
        progress: &mut ProgressLine,
        label: &str,
    ) -> Result<()> {
        let mut settling = Settling::new(self.sent, self.before, Instant::now());
        let mut tick = tokio::time::interval(TICK);
        let mut poll = tokio::time::interval(POLL);
        let mut written = link.written();
        let mut deadline = tokio::time::Instant::now() + COME_BACK_BLUETOOTH;
        let mut dropped = false;
        loop {
            match settling.settled(Instant::now(), false) {
                Settled::Waiting | Settled::Unconfirmed => {}
                settled => return settled_text(&settled, label, false, progress),
            }
            let pumping = settling.up() && !settling.came_back();
            if pumping {
                self.pump(link)?;
                if let Stage::Failed(failure) = self.sender.stage() {
                    bail!("{}", self.failed(failure));
                }
            }
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => {
                    if !settling.came_back() {
                        bail!(
                            "the bridge did not come back over Bluetooth within {} s of the \
                             install; flash-bridge --link usb recovers it, and its CH343 shows \
                             its log",
                            COME_BACK_BLUETOOTH.as_secs()
                        );
                    }
                    let now = Instant::now();
                    let settled = settling.settled(now, true);
                    return settled_text(&settled, label, settling.radio_running(now), progress);
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
                            progress.say("the radio restarted on its bundled image");
                        }
                    }
                    Some(LinkEvent::Disconnected { .. }) => {
                        settling.see(Seen::Down, Instant::now());
                        if !dropped {
                            dropped = true;
                            progress.say("the link dropped as the bridge restarts; reconnecting");
                        }
                    }
                    Some(LinkEvent::Message(message)) if !settling.came_back() => {
                        self.take(message, progress);
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
                        progress.line(&message);
                    }
                    Some(LinkEvent::Garbled(_)) => {}
                    None => bail!("the link closed"),
                },
            }
        }
    }

    /// Queue every op the sender has for now, once the link has written the last.
    fn pump(&mut self, link: &LinkHandle) -> Result<()> {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.sender.pump(link, now, HostToBridge::Update).context("queueing an update op")
    }

    /// A message from the P4: a status for the sender, or a line to print.
    fn take(&mut self, message: BridgeToHost, progress: &mut ProgressLine) {
        match message {
            BridgeToHost::Update(bytes) => {
                if let Some(Op::Status(status)) = Op::decode(&bytes) {
                    self.sender.on_status(&status);
                }
            }
            message => progress.line(&message),
        }
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

/// Print what the C6's update says, until its new life or [`QUIET`] without a line about it.
async fn follow_radio(link: &mut LinkHandle, progress: &mut ProgressLine) {
    let deadline = tokio::time::Instant::now() + FOLLOW;
    let mut quiet_until = tokio::time::Instant::now() + QUIET;
    let mut poll = tokio::time::interval(POLL);
    loop {
        let until = quiet_until.min(deadline);
        let Some(event) = next_event(link, &mut poll, until).await else {
            if until == deadline {
                progress.say("the radio's update is still running; `wartui status` shows its end");
            }
            return;
        };
        match event {
            Some(LinkEvent::Message(message)) => {
                if is_radio_update(&message) {
                    quiet_until = tokio::time::Instant::now() + QUIET;
                }
                progress.line(&message);
            }
            Some(LinkEvent::Connected(_)) => {
                progress.say("the radio restarted on its bundled image");
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

/// Wait up to [`CONFIRM_WAIT`] for the P4's line about confirming its image, printing any other
/// line on the way.
async fn await_confirm(link: &mut LinkHandle, progress: &mut ProgressLine) -> Confirm {
    let deadline = tokio::time::Instant::now() + CONFIRM_WAIT;
    let mut poll = tokio::time::interval(POLL);
    loop {
        let Some(event) = next_event(link, &mut poll, deadline).await else {
            return Confirm::Unseen;
        };
        match event {
            Some(LinkEvent::Message(message)) => match confirm_of(&message) {
                Some(confirm) => return confirm,
                None => progress.line(&message),
            },
            Some(_) => {}
            None => return Confirm::Unseen,
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

/// What follows a confirm that was not seen.
const ROLLS_BACK: &str = "It rolls back to its old image unless a host confirms this one: run \
                          flash-bridge again, or any wartui command that opens the bridge";

/// The line for a new image's confirm: `Ok` once confirmed, `Err` with what to do otherwise.
fn confirm_text(confirm: &Confirm) -> Result<&'static str, String> {
    match confirm {
        Confirm::Confirmed => Ok("installed and confirmed"),
        Confirm::Failed(why) => Err(format!("installed the new image, but {why}. {ROLLS_BACK}")),
        Confirm::Unseen => Err(format!(
            "installed the new image, but no confirm was seen within {} s. {ROLLS_BACK}",
            CONFIRM_WAIT.as_secs()
        )),
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

/// How a new image is known to be confirmed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Proof {
    /// The P4's line said so.
    Confirmed,
    /// The line was lost, and the bridge outlived its trial watchdog.
    OutlivedTrial,
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

/// Say how an update over Bluetooth ended, `Ok` once installed. `radio_running` is whether the
/// C6's update may still be running ([`Settling::radio_running`]).
fn settled_text(
    settled: &Settled,
    label: &str,
    radio_running: bool,
    progress: &mut ProgressLine,
) -> Result<()> {
    match settled {
        Settled::Installed(proof) => {
            progress.say(match proof {
                Proof::Confirmed => "installed and confirmed",
                Proof::OutlivedTrial => "installed, and confirmed: it outlived its trial watchdog",
            });
            if radio_running {
                progress.say("the radio's update is still running; `wartui status` shows its end");
            }
            Ok(())
        }
        Settled::Lost => {
            bail!("{label} never took the install, and keeps its image; run flash-bridge again")
        }
        Settled::Not(Outcome::RolledBack) => {
            bail!("{label} did not boot the new image; rolled back")
        }
        Settled::Not(_) => bail!("{label} came back running another image"),
        Settled::NotConfirmed(why) => {
            let text = confirm_text(&Confirm::Failed(why.clone())).err().unwrap_or_default();
            bail!("{label} {text}")
        }
        Settled::Unconfirmed => bail!(
            "{label} installed the new image, but no confirm was seen over Bluetooth. {ROLLS_BACK}"
        ),
        Settled::Waiting => bail!("{label} did not come back"),
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

/// Why the update stopped when the link stopped writing it at `percent`, and what fixes it.
fn port_stall_text(percent: u8) -> String {
    format!(
        "the bridge's port stopped taking the image at {percent}%. The bridge keeps its image. \
         If this command does not return, unplug and replug the bridge's USB data cable. \
         `flash-bridge --link bluetooth` carries the update"
    )
}

/// Why the update stopped, and what fixes it.
fn failure_text(failure: Failure, image_known: bool) -> String {
    match failure {
        Failure::Silent if !image_known => format!(
            "the bridge did not answer the update, and its image carries no digest: it runs \
             firmware from another tree. Flash it once by hand, per {FIRST_FLASH}"
        ),
        Failure::Silent => "the bridge stopped answering the update".to_owned(),
        Failure::Refused(UpdateState::FlashError) => format!(
            "the bridge refused the update: it has no second app slot or trial sector, or is \
             still on trial. A board on espflash's default partition table needs one flash by \
             hand with the bridge's own, per {FIRST_FLASH}"
        ),
        Failure::Refused(UpdateState::HashMismatch) => {
            "the bridge received the image damaged; run flash-bridge again".to_owned()
        }
        Failure::Refused(UpdateState::TooLarge) => {
            "the image is larger than the bridge's update slot".to_owned()
        }
        Failure::Refused(state) => format!("the bridge refused the update: {state:?}"),
        Failure::SwitchFailed(_) => "the bridge received the image whole, but switching to it \
                                     failed; run flash-bridge again"
            .to_owned(),
        Failure::LostSession => {
            "the bridge kept dropping the update session; run flash-bridge again".to_owned()
        }
    }
}

/// The update's progress, redrawn in place on a terminal and a line per tenth otherwise, and
/// the bridge's lines between.
struct ProgressLine {
    board: String,
    tty: bool,
    /// The last percentage drawn, and a line is half-drawn on the terminal.
    drawn: Option<u8>,
}

impl ProgressLine {
    fn new(board: String) -> Self {
        Self { board, tty: std::io::stdout().is_terminal(), drawn: None }
    }

    fn show(&mut self, percent: u8) {
        let due = match self.drawn {
            None => true,
            Some(last) if self.tty => last != percent,
            Some(last) => last / 10 != percent / 10,
        };
        if !due {
            return;
        }
        self.drawn = Some(percent);
        let text = format!("{:<6} {percent:>3}%  sending", self.board);
        if self.tty {
            print!("\r\x1b[K{text}");
            let _ = std::io::stdout().flush();
        } else {
            println!("{text}");
        }
    }

    /// A line of its own, below the progress.
    fn say(&mut self, text: &str) {
        if self.tty && self.drawn.is_some() {
            print!("\r\x1b[K");
        }
        self.drawn = None;
        println!("{:<6} {text}", self.board);
    }

    /// A bridge's log or error line; anything else is not printed.
    fn line(&mut self, message: &BridgeToHost) {
        match message {
            BridgeToHost::Error { message } => self.say(&format!("fault  {message}")),
            BridgeToHost::Log { message } => self.say(&format!("Info  {message}")),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wartui_proto::link::LogStr;

    use super::super::normalize;
    use super::super::testing::{C6_HEADER, P4_APP_HEADER};
    use super::*;

    /// A P4 bridge app image behind the header espflash wrote for one, `len` bytes long.
    fn p4_app(project: &str, len: usize) -> Vec<u8> {
        app::fixture_with(P4_APP_HEADER, project, "0.1.0", len)
    }

    #[test]
    fn flash_p4_chooses_source_when_given_each_combination() {
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        assert_eq!(
            source(&P4_BRIDGE_APP, Some(image), None, Some("v1")),
            Source::File(image.to_owned())
        );
        assert_eq!(
            source(&P4_BRIDGE_APP, None, Some(dir), Some("v1")),
            Source::Build(dir.to_owned())
        );
        let found = source(&P4_BRIDGE_APP, None, None, None);
        assert_eq!(found, Source::Build(normalize(Path::new(P4_BRIDGE_APP.dir))));
        let Source::Build(default) = found else { panic!("not a build") };
        assert!(default.ends_with("firmware/p4-bridge"), "{}", default.display());
    }

    #[test]
    fn flash_p4_fetches_release_asset_when_built_for_a_release() {
        // The name `release.yml` publishes the P4 bridge's app image under.
        let asset = "wartui-p4-bridge-app-esp32p4.bin";
        assert_eq!(
            source(&P4_BRIDGE_APP, None, None, Some("v1")),
            Source::Release { tag: "v1".to_owned(), asset }
        );
    }

    #[test]
    fn flash_p4_accepts_image_when_it_is_the_p4_bridge_app() {
        check_app(&p4_app(PROJECT, 0x1000)).unwrap();
        check_app(&p4_app(PROJECT, SLOT_LEN)).unwrap();
    }

    #[test]
    fn flash_p4_refuses_image_when_project_is_not_the_p4_bridge() {
        let error = check_app(&p4_app("wartui-node-esp32c6", 0x1000)).unwrap_err().to_string();
        assert!(error.contains("'wartui-node-esp32c6'") && error.contains(PROJECT), "{error}");
    }

    #[test]
    fn flash_p4_refuses_image_when_truncated() {
        let image = p4_app(PROJECT, 0x1000);
        let error = check_app(&image[..0x800]).unwrap_err().to_string();
        assert!(error.contains("truncated"), "{error}");
    }

    #[test]
    fn flash_p4_refuses_image_when_segment_byte_is_flipped() {
        let mut image = p4_app(PROJECT, 0x1000);
        image[0x800] ^= 0x01;
        let error = check_app(&image).unwrap_err().to_string();
        assert!(error.contains("corrupt"), "{error}");
    }

    #[test]
    fn flash_p4_refuses_image_when_larger_than_its_slot() {
        let error = check_app(&p4_app(PROJECT, SLOT_LEN + 16)).unwrap_err().to_string();
        assert!(error.contains("app slot holds"), "{error}");
    }

    #[test]
    fn flash_p4_refuses_image_when_built_for_another_chip() {
        let image = app::fixture_with(C6_HEADER, "wartui-p4-radio", "0.1.0", 0x1000);
        let error = check_app(&image).unwrap_err().to_string();
        assert!(error.contains("built for esp32c6"), "{error}");
    }

    #[test]
    fn flash_p4_matches_slot_size_when_reading_partition_table() {
        let dir = Path::new(P4_BRIDGE_APP.dir);
        let table = std::fs::read_to_string(dir.join("partitions.csv")).unwrap();
        for slot in ["ota_0", "ota_1"] {
            let row = table.lines().find(|line| line.starts_with(slot)).unwrap();
            let size = row.split(',').map(str::trim).nth(4).unwrap();
            let size = usize::from_str_radix(size.trim_start_matches("0x"), 16).unwrap();
            assert_eq!(size, SLOT_LEN, "{slot}");
        }
    }

    #[test]
    fn flash_p4_keeps_sending_when_ready_names_the_old_image() {
        // The C6 restarted, and the P4 announced its new life.
        assert_eq!(on_ready(Phase::Sending, Some(OLD), Some(OLD)), OnReady::Ignore);
        assert_eq!(on_ready(Phase::Sending, None, None), OnReady::Ignore);
    }

    #[test]
    fn flash_p4_stops_sending_when_ready_names_another_image() {
        assert_eq!(on_ready(Phase::Sending, Some(OLD), Some(NEW)), OnReady::Abort);
        assert_eq!(on_ready(Phase::Sending, Some(OLD), None), OnReady::Abort);
    }

    #[test]
    fn flash_p4_takes_ready_as_new_life_when_it_follows_install() {
        // The connection holds through the reboot, so a new life is the only sign of it.
        assert_eq!(on_ready(Phase::Installing, Some(OLD), Some(NEW)), OnReady::Back);
        assert_eq!(on_ready(Phase::Installing, Some(OLD), Some(OLD)), OnReady::Back);
    }

    #[test]
    fn flash_p4_reads_radio_update_when_line_starts_with_shared_text() {
        assert!(is_radio_update(&error(&format!("{RADIO_UPDATING} to the bundled image (5%)"))));
        assert!(!is_radio_update(&error("the radio coprocessor is not answering")));
        assert!(!is_radio_update(&log(RADIO_UPDATING)));
    }

    const NEW: [u8; DIGEST_LEN] = [0x11; DIGEST_LEN];
    const OLD: [u8; DIGEST_LEN] = [0x22; DIGEST_LEN];

    #[test]
    fn flash_p4_judges_installed_when_ready_names_the_sent_image() {
        assert_eq!(judge(&NEW, Some(OLD), Some(NEW)), Outcome::Installed);
    }

    #[test]
    fn flash_p4_judges_rolled_back_when_ready_names_the_old_image() {
        assert_eq!(judge(&NEW, Some(OLD), Some(OLD)), Outcome::RolledBack);
    }

    #[test]
    fn flash_p4_judges_other_when_ready_names_neither_image() {
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
    fn flash_p4_settles_installed_when_new_image_is_back_confirmed_and_quiet() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Down, after(at, 100));
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Confirm(Confirm::Confirmed), after(at, 4_100));
        assert_eq!(s.settled(after(at, 6_000), false), Settled::Waiting, "not yet quiet");
        assert_eq!(s.settled(after(at, 9_000), false), Settled::Installed(Proof::Confirmed));
    }

    #[test]
    fn flash_p4_keeps_waiting_when_radio_update_drops_the_link_after_confirm() {
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
    fn flash_p4_omits_radio_line_when_link_is_down_at_the_deadline_with_no_radio_update() {
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
    fn flash_p4_says_radio_line_when_link_is_down_at_the_deadline_after_a_radio_update() {
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
    fn flash_p4_judges_from_latest_ready_when_bridge_reconnects_on_old_image() {
        // The new image announced itself, then its trial watchdog rolled it back.
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Down, after(at, 34_000));
        s.see(Seen::Ready { image: Some(OLD), uptime_ms: 1_500 }, after(at, 70_000));
        assert_eq!(s.settled(after(at, 70_000), false), Settled::Not(Outcome::RolledBack));
    }

    #[test]
    fn flash_p4_reports_install_lost_when_ready_predates_the_install() {
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
    fn flash_p4_reports_rolled_back_when_trial_boots_end_on_old_image() {
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
    fn flash_p4_counts_image_confirmed_when_line_was_lost_and_bridge_outlived_its_trial() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Uptime(20_000), after(at, 23_000));
        assert_eq!(s.settled(after(at, 23_000), false), Settled::Waiting);
        s.see(Seen::Uptime(OUTLIVED_TRIAL_MS), after(at, 38_000));
        assert_eq!(s.settled(after(at, 38_000), false), Settled::Installed(Proof::OutlivedTrial));
    }

    #[test]
    fn flash_p4_reports_unconfirmed_at_deadline_when_no_sign_of_confirm_arrived() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        s.see(Seen::Uptime(9_000), after(at, 12_000));
        assert_eq!(s.settled(after(at, 154_000), true), Settled::Unconfirmed);
    }

    #[test]
    fn flash_p4_reports_failed_confirm_at_once_when_bridge_says_so() {
        let at = Instant::now();
        let mut s = settling(at);
        s.see(Seen::Ready { image: Some(NEW), uptime_ms: 1_500 }, after(at, 4_000));
        let why = format!("{IMAGE_NOT_CONFIRMED}: flash write failed");
        s.see(Seen::Confirm(Confirm::Failed(why.clone())), after(at, 4_100));
        assert_eq!(s.settled(after(at, 4_100), false), Settled::NotConfirmed(why));
    }

    #[test]
    fn flash_p4_waits_for_the_new_life_when_nothing_came_back_by_the_deadline() {
        // The caller words this case itself: the bridge did not come back.
        let at = Instant::now();
        let s = settling(at);
        assert_eq!(s.settled(after(at, 180_000), true), Settled::Waiting);
    }

    #[test]
    fn flash_p4_names_wired_flash_when_bridge_reports_no_digest() {
        let text = failure_text(Failure::Silent, false);
        assert!(text.contains("another tree") && text.contains(FIRST_FLASH), "{text}");
        assert!(!failure_text(Failure::Silent, true).contains("another tree"));
    }

    #[test]
    fn flash_p4_names_port_stall_when_sender_goes_silent_on_unwritten_ops() {
        // A port that stops draining is not a receiver that stopped answering: the remedy is
        // the cable, and the command itself may not return.
        let text = port_stall_text(42);
        assert!(text.contains("stopped taking the image at 42%"), "{text}");
        assert!(text.contains("replug the bridge's USB data cable"), "{text}");
        assert!(text.contains("`flash-bridge --link bluetooth`"), "{text}");
    }

    #[test]
    fn flash_p4_names_wired_flash_when_bridge_has_no_update_slot() {
        let text = failure_text(Failure::Refused(UpdateState::FlashError), true);
        assert!(text.contains("partition table") && text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_says_run_again_when_switch_to_image_fails() {
        // A refusal after `Install`: the image arrived whole, so the table is not at fault.
        let text = failure_text(Failure::SwitchFailed(UpdateState::FlashError), true);
        assert!(text.contains("switching") && text.contains("run flash-bridge again"), "{text}");
        assert!(!text.contains("partition table") && !text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_names_wired_flash_when_ready_arrived_garbled() {
        let text = no_ready_text("71:F4", "/dev/ttyACM0", "no answer in 10 s", true);
        assert!(text.contains("another tree") && text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_names_held_port_when_nothing_arrived() {
        let text = no_ready_text("71:F4", "/dev/ttyACM0", "no answer in 10 s", false);
        assert!(text.contains("wartui run") && text.contains("still starting"), "{text}");
        assert!(!text.contains("another tree") && !text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_claims_no_wait_beyond_the_reason_when_port_fails_at_once() {
        // A held port fails at about 0 s, so the text's only figure is the one in the reason.
        let text = no_ready_text("71:F4", "/dev/ttyACM0", "could not open /dev/ttyACM0", false);
        assert!(text.contains("(could not open /dev/ttyACM0)"), "{text}");
        assert!(!text.contains(" s "), "{text}");
    }

    #[test]
    fn flash_p4_names_same_section_when_bridge_crate_describes_no_firmware() {
        // `wartui-bridge` sits below this crate, so it spells the section out.
        assert!(wartui_bridge::NO_FIRMWARE_P4.contains(FIRST_FLASH));
    }

    fn log(text: &str) -> BridgeToHost {
        BridgeToHost::Log { message: LogStr::try_from(text).unwrap() }
    }

    fn error(text: &str) -> BridgeToHost {
        BridgeToHost::Error { message: LogStr::try_from(text).unwrap() }
    }

    #[test]
    fn flash_p4_reads_confirm_when_bridge_says_image_confirmed() {
        assert_eq!(confirm_of(&log(IMAGE_CONFIRMED)), Some(Confirm::Confirmed));
        assert_eq!(confirm_text(&Confirm::Confirmed), Ok("installed and confirmed"));
    }

    #[test]
    fn flash_p4_reads_failed_confirm_when_bridge_reports_error() {
        let line = format!("{IMAGE_NOT_CONFIRMED}: flash write failed");
        let confirm = confirm_of(&error(&line)).unwrap();
        assert_eq!(confirm, Confirm::Failed(line.clone()));
        let text = confirm_text(&confirm).unwrap_err();
        assert!(text.contains(&line) && text.contains("rolls back"), "{text}");
    }

    #[test]
    fn flash_p4_ignores_line_when_it_is_not_about_the_confirm() {
        assert_eq!(confirm_of(&log("trial boot 1 of 3")), None);
        // The confirm's text as an error, or its failure as a log, is not the line.
        assert_eq!(confirm_of(&error(IMAGE_CONFIRMED)), None);
        assert_eq!(confirm_of(&log(IMAGE_NOT_CONFIRMED)), None);
    }

    #[test]
    fn flash_p4_says_rolls_back_when_confirm_is_unseen() {
        let text = confirm_text(&Confirm::Unseen).unwrap_err();
        assert!(text.contains("no confirm was seen") && text.contains("rolls back"), "{text}");
    }
}
