//! `wartui flash-bridge` on a P4 bridge: its app image, sent over the USB link it bridges on.
//!
//! **The cable the host already holds carries the update.** The P4's Type-C reaches a CH343
//! that needs BOOT held on one board, so only the first flash uses it
//! (`firmware/p4-bridge/README.md` § "First flash"). After that the P4 takes its app image as
//! `HostToBridge::Update` ops: the `spi::update` layout it sends its own C6, driven by the same
//! `Sender`, on a [`TICK`] and on each status.
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
//! send began against, so the send stops. After `Install`, only a `Ready` after the link drops is
//! the new life ([`on_ready`]).
//!
//! **`Ready.image` names the outcome.** The P4 installs, reboots into the new image on trial,
//! and the transport reconnects; its `Identify` is the first host frame, which confirms the
//! image. A `Ready` carrying the new digest is installed. One carrying the old is a rollback: the
//! P4's trial watchdog resets a hung image twice, and the third boot returns it to the old one,
//! which is why the wait is [`COME_BACK`].
//!
//! **The confirm is reported, not assumed.** The P4 says whether the `Identify` confirmed its
//! image in a line after its `Ready`, in the text `wartui_proto::link` holds for both ends. Only
//! that line is "confirmed": an image whose confirm failed, or whose line never came, is still on
//! trial and rolls back unless a host confirms it.
//!
//! **Then the C6.** A new P4 image carries its C6's image, which the P4 sends the C6 at boot.
//! Its progress arrives as error lines, printed until the C6's new life, or until [`QUIET`]
//! passes without one.
//!
//! **A P4 that cannot take an update names the fix.** One whose firmware predates link updates
//! announces a `Ready` this build cannot decode, and one on espflash's default partition table
//! refuses the `Begin`. Both need one last wired flash. A P4 that sends nothing at all is more
//! likely held by another program, or still starting, so that advice is kept for a `Ready` that
//! arrived garbled.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{BridgeSpec, SerialTransport};
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};
use wartui_proto::air::OtaState;
use wartui_proto::link::{
    BridgeToHost, Chip as LinkChip, HostToBridge, IMAGE_CONFIRMED, IMAGE_NOT_CONFIRMED,
    RADIO_UPDATING, UpdateOp,
};
use wartui_proto::ota::image::{DIGEST_LEN, appended_digest_of};
use wartui_proto::spi::update::{Failure, OP_MAX, Op, Sender, Stage};

use super::bridge::Args;
use super::{
    Chip, P4_BRIDGE_APP, RELEASE_TAG, Source, app, check_image, espflash_present, name, obtain,
    read, source,
};

/// The project name the P4 bridge's app descriptor carries: its crate's name.
const PROJECT: &str = "wartui-p4-bridge-fw";

/// The largest image the P4's app slot holds: `ota_0` and `ota_1` in
/// `firmware/p4-bridge/partitions.csv`.
const SLOT_LEN: usize = 0x1E_0000;

/// How often the sender is asked for its next op, besides on each status.
const TICK: Duration = Duration::from_millis(50);

/// How long to wait for the P4's `Ready`. It enumerates once its C6 answers, or after 2 s
/// without, and the transport waits 6 s for a lone board.
const READY_WAIT: Duration = Duration::from_secs(10);

/// How long to wait for the P4 to come back after the install: two trial boots that the 30 s
/// watchdog ends and the third that rolls back, with room for each boot.
const COME_BACK: Duration = Duration::from_secs(120);

/// How long after the new image's `Ready` to wait for the line saying whether the host's
/// `Identify` confirmed it. The P4 sends it right behind the `Ready`.
const CONFIRM_WAIT: Duration = Duration::from_secs(3);

/// The longest the C6's update is followed after the P4 comes back.
const FOLLOW: Duration = Duration::from_secs(30);

/// How long without a line about the C6's update before it is taken as done, or as not needed.
const QUIET: Duration = Duration::from_secs(5);

/// The README section that covers a wired flash.
const FIRST_FLASH: &str = "firmware/p4-bridge/README.md § \"First flash\"";

/// Update the P4 bridge `board` over its link.
pub(super) async fn run(args: &Args, features: &[&str], board: &PortCandidate) -> Result<()> {
    refuse_chip_features(features)?;
    let source = source(
        &P4_BRIDGE_APP,
        args.image.as_deref(),
        args.firmware_dir.as_deref(),
        features,
        false,
        RELEASE_TAG,
    )?;
    println!("image  {}", source.describe(args.dry_run));
    if args.dry_run {
        if let Source::File(path) = &source {
            check_app(&read(path)?).with_context(|| format!("checking {}", path.display()))?;
        }
        println!("{:<6} would update over its link", name(board));
        return Ok(());
    }
    if matches!(source, Source::Build(_)) {
        // It saves the built ELF as an image.
        espflash_present()?;
    }
    let path = obtain(&P4_BRIDGE_APP, &source, features, false, Chip::Esp32p4)?;
    let image = read(&path)?;
    check_app(&image).with_context(|| format!("checking {}", path.display()))?;
    let sha256: [u8; DIGEST_LEN] = Sha256::digest(&image).into();
    let Some(digest) = appended_digest_of(&image) else {
        bail!("{} carries no appended digest, which names the image to the P4", path.display());
    };

    let expected = board.mac().context("a P4 bridge reports its address")?;
    let mut link = SerialTransport::with_spec(BridgeSpec::Mac(expected))
        .remember_in(BridgeMemory::none())
        .start()
        .context("opening the link")?;
    let mut garbled = false;
    let info = match tokio::time::timeout(READY_WAIT, wait_for_ready(&mut link, &mut garbled)).await
    {
        Ok(Ok(info)) => info,
        Ok(Err(why)) => bail!("{}", no_ready_text(&name(board), &board.path, &why, garbled)),
        Err(_) => {
            let why = format!("no answer in {} s", READY_WAIT.as_secs());
            bail!("{}", no_ready_text(&name(board), &board.path, &why, garbled))
        }
    };
    ensure!(
        info.chip == LinkChip::Esp32P4,
        "{} answered as {:?}, not a P4",
        name(board),
        info.chip
    );
    ensure!(info.mac == expected, "{} answered with another address", name(board));
    if info.image == Some(digest) {
        println!("{:<6} already runs this image", name(board));
        return Ok(());
    }

    let mut progress = ProgressLine::new(name(board));
    let mut update = Update::new(&image, sha256, &digest, info.image);
    update.send(&mut link, &mut progress).await?;
    progress.say("installing; waiting for the bridge to come back");
    let back = update.come_back(&mut link, &mut progress).await?;
    match judge(&digest, info.image, back.image) {
        Outcome::Installed => {}
        Outcome::RolledBack => bail!("{} did not boot the new image; rolled back", name(board)),
        Outcome::Other => bail!("{} came back running another image", name(board)),
    }
    let confirm = await_confirm(&mut link, &mut progress).await;
    match confirm_text(&confirm) {
        Ok(text) => progress.say(text),
        Err(text) => bail!("{} {text}", name(board)),
    }
    follow_radio(&mut link, &mut progress).await;
    Ok(())
}

/// Refuse an image that is not the P4 bridge's app, or that is damaged or too large for its
/// slot.
fn check_app(image: &[u8]) -> Result<()> {
    check_image(image, Chip::Esp32p4, false)?;
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

/// Refuse features that name a single-chip bridge's chip: they build another firmware.
fn refuse_chip_features(features: &[&str]) -> Result<()> {
    if let Some(chip) = Chip::FEATURED.into_iter().find(|chip| features.contains(&chip.name())) {
        bail!(
            "--features names {}, and a P4 bridge is built with its own; leave --features out \
             for its release image",
            chip.name()
        );
    }
    Ok(())
}

/// Wait for the link's `Ready`, setting `garbled` when a frame arrives that does not decode.
async fn wait_for_ready(link: &mut LinkHandle, garbled: &mut bool) -> Result<BridgeInfo, String> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason }) => return Err(reason),
            Some(LinkEvent::Garbled(_)) => *garbled = true,
            Some(LinkEvent::Message(_)) => {}
            None => return Err("the link closed".to_owned()),
        }
    }
}

/// Why no `Ready` arrived from `board` on `path`, and what to do. Frames that did not decode
/// are a `Ready` from firmware that predates link updates; silence is a held port or a bridge
/// still starting.
fn no_ready_text(board: &str, path: &str, why: &str, garbled: bool) -> String {
    if garbled {
        format!(
            "{board} on {path} answered in a form this build cannot read ({why}): its firmware \
             predates link updates. Flash it once by hand, per {FIRST_FLASH}"
        )
    } else {
        format!(
            "{board} on {path} did not announce itself within {} s ({why}). Another program may \
             hold the port, such as a running `wartui run`, or the bridge is still starting; \
             try again",
            READY_WAIT.as_secs()
        )
    }
}

/// Where the update stands when a `Ready` arrives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Sending the image.
    Sending,
    /// `Install` sent; `dropped` once the link has dropped since.
    Installing { dropped: bool },
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
        Phase::Installing { dropped: false } => OnReady::Ignore,
        Phase::Installing { dropped: true } => OnReady::Back,
    }
}

/// One image on its way to the P4.
struct Update<'a> {
    sender: Sender<'a>,
    start: Instant,
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
        Self { sender: Sender::new(image, sha256, digest), start: Instant::now(), before }
    }

    /// Why the update stopped, and what fixes it.
    fn failed(&self, failure: Failure) -> String {
        failure_text(failure, self.before.is_some())
    }

    /// Send the image until the P4 is told to install.
    async fn send(&mut self, link: &mut LinkHandle, progress: &mut ProgressLine) -> Result<()> {
        let mut tick = tokio::time::interval(TICK);
        loop {
            self.pump(link)?;
            match self.sender.stage() {
                Stage::Sending => progress.show(self.sender.percent()),
                Stage::Installed => return Ok(()),
                Stage::Failed(failure) => bail!("{}", self.failed(failure)),
            }
            tokio::select! {
                _ = tick.tick() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(message)) => self.take(message, progress),
                    Some(LinkEvent::Disconnected { reason }) => bail!(
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

    /// Wait out the reboot: `Install` again until the link drops, then the new life's
    /// `Ready`. A `Ready` before the drop is the C6 restarting, and is passed over.
    async fn come_back(
        &mut self,
        link: &mut LinkHandle,
        progress: &mut ProgressLine,
    ) -> Result<BridgeInfo> {
        let wait = async {
            let mut tick = tokio::time::interval(TICK);
            let mut dropped = false;
            loop {
                if !dropped {
                    self.pump(link)?;
                }
                if let Stage::Failed(failure) = self.sender.stage() {
                    bail!("{}", self.failed(failure));
                }
                tokio::select! {
                    _ = tick.tick(), if !dropped => {}
                    event = link.recv() => match event {
                        Some(LinkEvent::Connected(info)) => {
                            let phase = Phase::Installing { dropped };
                            if on_ready(phase, self.before, info.image) == OnReady::Back {
                                return Ok(info);
                            }
                        }
                        Some(LinkEvent::Message(message)) => self.take(message, progress),
                        Some(LinkEvent::Disconnected { .. }) => dropped = true,
                        Some(LinkEvent::Garbled(_)) => {}
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

    /// Queue every op the sender has for now.
    fn pump(&mut self, link: &LinkHandle) -> Result<()> {
        let now = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        let mut buf = [0u8; OP_MAX];
        while let Some(len) = self.sender.next(now, &mut buf) {
            let op = UpdateOp::from_slice(&buf[..len]).expect("an op fits OP_MAX");
            link.send_urgent(HostToBridge::Update(op)).context("queueing an update op")?;
        }
        Ok(())
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

/// Print what the C6's update says, until its new life or [`QUIET`] without a line about it.
async fn follow_radio(link: &mut LinkHandle, progress: &mut ProgressLine) {
    let deadline = tokio::time::Instant::now() + FOLLOW;
    let mut quiet_until = tokio::time::Instant::now() + QUIET;
    loop {
        let until = quiet_until.min(deadline);
        let Ok(event) = tokio::time::timeout_at(until, link.recv()).await else {
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
    /// The host's `Identify` confirmed it.
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
    loop {
        let Ok(event) = tokio::time::timeout_at(deadline, link.recv()).await else {
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

/// The line for a new image's confirm: `Ok` once confirmed, `Err` with what to do otherwise.
fn confirm_text(confirm: &Confirm) -> Result<&'static str, String> {
    const ROLLS_BACK: &str = "It rolls back to its old image unless a host confirms this one: \
                              run flash-bridge again, or any wartui command that opens the bridge";
    match confirm {
        Confirm::Confirmed => Ok("installed and confirmed"),
        Confirm::Failed(why) => Err(format!("installed the new image, but {why}. {ROLLS_BACK}")),
        Confirm::Unseen => Err(format!(
            "installed the new image, but no confirm was seen within {} s. {ROLLS_BACK}",
            CONFIRM_WAIT.as_secs()
        )),
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

/// Why the update stopped, and what fixes it.
fn failure_text(failure: Failure, image_known: bool) -> String {
    match failure {
        Failure::Silent if !image_known => format!(
            "the bridge did not answer the update: its firmware predates link updates. Flash it \
             once by hand, per {FIRST_FLASH}"
        ),
        Failure::Silent => "the bridge stopped answering the update".to_owned(),
        Failure::Refused(OtaState::FlashError) => format!(
            "the bridge refused the update: it has no second app slot or trial sector, or is \
             still on trial. A board on espflash's default partition table needs one flash by \
             hand with the bridge's own, per {FIRST_FLASH}"
        ),
        Failure::Refused(OtaState::HashMismatch) => {
            "the bridge received the image damaged; run flash-bridge again".to_owned()
        }
        Failure::Refused(OtaState::TooLarge) => {
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
            BridgeToHost::Log { level, message } => self.say(&format!("{level:?}  {message}")),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use wartui_proto::link::{LogLevel, LogStr};

    use super::super::testing::P4_APP_HEADER;
    use super::*;

    /// A P4 bridge app image behind the header espflash wrote for one, `len` bytes long.
    fn p4_app(project: &str, len: usize) -> Vec<u8> {
        app::fixture_with(P4_APP_HEADER, project, "0.1.0", len)
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
        let image = app::fixture(wartui_proto::air::Board::Esp32c6, "0.1.0", 0x1000);
        let error = check_app(&image).unwrap_err().to_string();
        assert!(error.contains("built for esp32c6"), "{error}");
    }

    #[test]
    fn flash_p4_matches_slot_size_when_reading_partition_table() {
        let dir = std::path::Path::new(P4_BRIDGE_APP.dir);
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
    fn flash_p4_ignores_ready_when_link_has_not_dropped_since_install() {
        let phase = Phase::Installing { dropped: false };
        assert_eq!(on_ready(phase, Some(OLD), Some(OLD)), OnReady::Ignore);
        assert_eq!(on_ready(phase, Some(OLD), Some(NEW)), OnReady::Ignore);
    }

    #[test]
    fn flash_p4_takes_ready_as_new_life_when_link_dropped_after_install() {
        let phase = Phase::Installing { dropped: true };
        assert_eq!(on_ready(phase, Some(OLD), Some(NEW)), OnReady::Back);
        assert_eq!(on_ready(phase, Some(OLD), Some(OLD)), OnReady::Back);
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

    #[test]
    fn flash_p4_refuses_features_when_they_name_a_single_chip_bridge() {
        for chip in ["esp32c5", "esp32c6"] {
            let error = refuse_chip_features(&["gps", chip]).unwrap_err().to_string();
            assert!(error.contains(chip), "{error}");
        }
        refuse_chip_features(&[]).unwrap();
        refuse_chip_features(&["gps"]).unwrap();
    }

    #[test]
    fn flash_p4_names_wired_flash_when_bridge_predates_link_updates() {
        let text = failure_text(Failure::Silent, false);
        assert!(text.contains("predates") && text.contains(FIRST_FLASH), "{text}");
        assert!(!failure_text(Failure::Silent, true).contains("predates"));
    }

    #[test]
    fn flash_p4_names_wired_flash_when_bridge_has_no_update_slot() {
        let text = failure_text(Failure::Refused(OtaState::FlashError), true);
        assert!(text.contains("partition table") && text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_says_run_again_when_switch_to_image_fails() {
        // A refusal after `Install`: the image arrived whole, so the table is not at fault.
        let text = failure_text(Failure::SwitchFailed(OtaState::FlashError), true);
        assert!(text.contains("switching") && text.contains("run flash-bridge again"), "{text}");
        assert!(!text.contains("partition table") && !text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_names_wired_flash_when_ready_arrived_garbled() {
        let text = no_ready_text("71:F4", "/dev/ttyACM0", "no answer in 10 s", true);
        assert!(text.contains("predates") && text.contains(FIRST_FLASH), "{text}");
    }

    #[test]
    fn flash_p4_names_held_port_when_nothing_arrived() {
        let text = no_ready_text("71:F4", "/dev/ttyACM0", "no answer in 10 s", false);
        assert!(text.contains("wartui run") && text.contains("still starting"), "{text}");
        assert!(!text.contains("predates") && !text.contains(FIRST_FLASH), "{text}");
    }

    fn log(text: &str) -> BridgeToHost {
        BridgeToHost::Log { level: LogLevel::Info, message: LogStr::try_from(text).unwrap() }
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
