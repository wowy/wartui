//! `wartui flash-bridge` on a P4 bridge: its app image, sent over the USB link it bridges on.
//!
//! **The cable the host already holds carries the update.** The P4's Type-C reaches a CH343
//! that needs BOOT held on one board, so only the first flash uses it
//! (`firmware/p4-bridge/README.md` § "First flash"). After that the P4 takes its app image as
//! `HostToBridge::Update` ops: the `spi::update` layout it sends its own C6, driven by the same
//! `Sender`, on a [`TICK`] and on each status.
//!
//! **`Ready.image` names the outcome.** The P4 installs, reboots into the new image on trial,
//! and the transport reconnects; its `Identify` is the first host frame, which confirms the
//! image. A `Ready` carrying the new digest is installed. One carrying the old is a rollback: the
//! P4's trial watchdog resets a hung image, and the third reset returns it to the old one, which
//! is why the wait is [`COME_BACK`].
//!
//! **Then the C6.** A new P4 image carries its C6's image, which the P4 sends the C6 at boot.
//! Its progress arrives as error lines, printed until the C6's new life, or until [`QUIET`]
//! passes without one.
//!
//! **A P4 that cannot take an update names the fix.** One whose firmware predates link updates
//! announces a `Ready` this build cannot decode, and one on espflash's default partition table
//! refuses the `Begin`. Both need one last wired flash.

use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{BridgeSpec, SerialTransport};
use wartui_bridge::{BridgeInfo, LinkEvent, LinkHandle};
use wartui_proto::air::OtaState;
use wartui_proto::link::{BridgeToHost, Chip as LinkChip, HostToBridge, UpdateOp};
use wartui_proto::ota::image::{DIGEST_LEN, appended_digest};
use wartui_proto::spi::update::{Failure, OP_MAX, Op, Sender, Stage};

use super::bridge::Args;
use super::{
    Chip, P4_BRIDGE_APP, RELEASE_TAG, Source, check_image, espflash_present, name, obtain, read,
    source,
};

/// How often the sender is asked for its next op, besides on each status.
const TICK: Duration = Duration::from_millis(50);

/// How long to wait for the P4's `Ready`. It enumerates once its C6 answers, or after 2 s
/// without, and the transport waits 6 s for a lone board.
const READY_WAIT: Duration = Duration::from_secs(10);

/// How long to wait for the P4 to come back after the install: three trial boots that the
/// 30 s watchdog ends, with room for each boot.
const COME_BACK: Duration = Duration::from_secs(120);

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
            check_image(&read(path)?, Chip::Esp32p4, false)?;
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
    let sha256: [u8; DIGEST_LEN] = Sha256::digest(&image).into();
    let Some(digest) = appended_digest(|at, buf| read_at(&image, at, buf)) else {
        bail!("{} carries no appended digest, which names the image to the P4", path.display());
    };

    let expected = board.mac().context("a P4 bridge reports its address")?;
    let mut link = SerialTransport::with_spec(BridgeSpec::Mac(expected))
        .remember_in(BridgeMemory::none())
        .start()
        .context("opening the link")?;
    let info = match tokio::time::timeout(READY_WAIT, wait_for_ready(&mut link)).await {
        Ok(info) => info,
        Err(_) => Err(anyhow::anyhow!("no answer in {} s", READY_WAIT.as_secs())),
    }
    .map_err(|why| {
        anyhow::anyhow!(
            "{} on {} did not announce itself: {why}. A P4 whose firmware predates link \
             updates announces nothing this build reads; flash it once by hand, per {FIRST_FLASH}",
            name(board),
            board.path
        )
    })?;
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
    let mut update = Update::new(&image, sha256, &digest, info.image.is_some());
    update.send(&mut link, &mut progress).await?;
    progress.say("installing; waiting for the bridge to come back");
    let back = update.come_back(&mut link, &mut progress).await?;
    match judge(&digest, info.image, back.image) {
        Outcome::Installed => progress.say("installed and confirmed"),
        Outcome::RolledBack => bail!("{} did not boot the new image; rolled back", name(board)),
        Outcome::Other => bail!("{} came back running another image", name(board)),
    }
    follow_radio(&mut link, &mut progress).await;
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

/// Fill `buf` from `image` at `at`, or return `false` past its end.
fn read_at(image: &[u8], at: u32, buf: &mut [u8]) -> bool {
    let start = at as usize;
    match image.get(start..start + buf.len()) {
        Some(bytes) => {
            buf.copy_from_slice(bytes);
            true
        }
        None => false,
    }
}

async fn wait_for_ready(link: &mut LinkHandle) -> Result<BridgeInfo> {
    loop {
        match link.recv().await {
            Some(LinkEvent::Connected(info)) => return Ok(info),
            Some(LinkEvent::Disconnected { reason }) => bail!("{reason}"),
            Some(_) => {}
            None => bail!("the link closed"),
        }
    }
}

/// One image on its way to the P4.
struct Update<'a> {
    sender: Sender<'a>,
    start: Instant,
    /// The P4's `Ready` named its image, so its firmware takes updates.
    image_known: bool,
}

impl<'a> Update<'a> {
    fn new(
        image: &'a [u8],
        sha256: [u8; DIGEST_LEN],
        digest: &[u8; DIGEST_LEN],
        image_known: bool,
    ) -> Self {
        Self { sender: Sender::new(image, sha256, digest), start: Instant::now(), image_known }
    }

    /// Send the image until the P4 is told to install.
    async fn send(&mut self, link: &mut LinkHandle, progress: &mut ProgressLine) -> Result<()> {
        let mut tick = tokio::time::interval(TICK);
        loop {
            self.pump(link)?;
            match self.sender.stage() {
                Stage::Sending => progress.show(self.sender.percent()),
                Stage::Installed => return Ok(()),
                Stage::Failed(failure) => bail!("{}", failure_text(failure, self.image_known)),
            }
            tokio::select! {
                _ = tick.tick() => {}
                event = link.recv() => match event {
                    Some(LinkEvent::Message(message)) => self.take(message, progress),
                    Some(LinkEvent::Disconnected { reason }) => bail!(
                        "the link dropped before the update finished ({reason}); the bridge \
                         keeps its image, and flash-bridge can be run again"
                    ),
                    Some(LinkEvent::Connected(_)) => bail!(
                        "the bridge restarted before the update finished; run flash-bridge again"
                    ),
                    Some(LinkEvent::Garbled(_)) => {}
                    None => bail!("the link closed"),
                },
            }
        }
    }

    /// Wait out the reboot: `Install` again until the link drops, then the new life's
    /// `Ready`.
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
                    bail!("{}", failure_text(failure, self.image_known));
                }
                tokio::select! {
                    _ = tick.tick(), if !dropped => {}
                    event = link.recv() => match event {
                        Some(LinkEvent::Connected(info)) => return Ok(info),
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
    matches!(message, BridgeToHost::Error { message } if message.starts_with("updating the radio"))
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
    use super::*;

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
}
