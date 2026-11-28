//! `wartui flash-bridge` on a P4 bridge: its app image, sent over the link it bridges on.
//!
//! **The update is `wartui_app::update::bridge`'s**, and so is its reasoning: what the link
//! carries, what a `Ready` means while sending and after the install, how the outcome is judged
//! over USB, and how it is read across the reconnects over Bluetooth. This command adds which
//! bridge and which link, where the image comes from, the dry run, and every line the operator
//! reads.
//!
//! **The link is opened and its `Ready` waited for here**, and over USB the bridge that answers
//! must be the one the probe chose. The port is opened by path: the probe that chose it has
//! already asked, and the reboot leaves the CH343 where it is. A ctrl-c while the link opens ends
//! the command. One while the image is sent is the update's stop: it ends the bridge's update
//! session, and the bridge keeps its image. From the install on the update runs to its outcome,
//! since only the host's frames confirm the new image, and a second ctrl-c exits without it.
//! `main` closes the link. The download or build before it blocks the task, so it runs with no
//! ctrl-c handler, and a ctrl-c there ends the process.
//!
//! A failed update over Bluetooth is recovered over USB, by running `flash-bridge` there.
//!
//! **A P4 that cannot take an update names the fix.** One running firmware from another tree
//! announces a `Ready` this build cannot decode, and one on espflash's default partition table
//! refuses the `Begin`. Both need one last wired flash. A P4 that sends nothing at all is more
//! likely held by another program, or still starting, so that advice is kept for a `Ready` that
//! arrived garbled.

use std::io::{IsTerminal, Write};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use wartui_app::update::bridge::{self, Failed, Line, Progress, Updated};
use wartui_app::update::image::{BridgeImage, Chip};
use wartui_app::update::release;
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::SerialTransport;
use wartui_bridge::spec::BridgeSpec;
use wartui_bridge::{BridgeInfo, LinkHandle};
use wartui_proto::bus::UpdateState;
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::update::Failure;

use super::bridge::Args;
use super::{Firmware, RELEASE_TAG, Source, app, espflash_present, obtain, read, source};
use crate::config::Link;

/// The P4 bridge's app-only image, which `flash-bridge` sends it over its link. Its merged image
/// is for the first flash by hand (`firmware/p4-bridge/README.md`), so nothing here fetches it.
const P4_BRIDGE_APP: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/p4-bridge"),
    elf: "target/riscv32imafc-unknown-none-elf/release/wartui-p4-bridge-fw",
    what: "P4 bridge",
    chip: Chip::Esp32p4,
    partition_table: "partitions.csv",
    // The table ends at 0xFF1000, past espflash's default 4 MB. The board's flash is 32 MB, so a
    // 16 MB header is safe on it.
    flash_size: Some("16mb"),
    merged: false,
    asset: release::BRIDGE_APP_ASSET,
};

/// The longest to wait for the P4's `Ready`. It announces once its C6 answers, or after 2 s
/// without. The transport's first failure ends the wait sooner: a held port at once, and a silent
/// lone board after the transport's 6 s.
const READY_WAIT: Duration = Duration::from_secs(10);

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
            // Every check the update makes, so a file the update would refuse is refused here.
            app::checked(path, BridgeImage::parse(read(path)?))?;
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
    let image = app::checked(&path, BridgeImage::parse(read(&path)?))?;
    let (link, over, info, label) = crate::interruptible(open(target, label)).await?;
    update_over_link(link, over, &info, &label, &image).await
}

/// Open the link to the bridge `target` names and wait for its `Ready`: the link, which it is,
/// the bridge's announcement, and the label the lines name it by.
async fn open(target: Target, label: String) -> Result<(LinkHandle, Link, BridgeInfo, String)> {
    Ok(match target {
        Target::Usb { board, expected } => {
            let (link, info) = open_usb(&label, &board.path).await?;
            ensure!(info.mac == expected, "{label} answered with another address");
            (link, Link::Usb, info, label)
        }
        Target::Bluetooth { bridge, memory } => {
            let (link, info) = open_bluetooth(&label, &bridge, memory).await?;
            // The transport checked the `Ready` against the name; its address names the lines.
            let label = mac::short(&info.mac).to_string();
            (link, Link::Bluetooth, info, label)
        }
    })
}

/// Send `image` to the bridge `info` announced on `link`, over `over`, to the outcome. The first
/// ctrl-c is the update's stop, and a second exits.
async fn update_over_link(
    mut link: LinkHandle,
    over: Link,
    info: &BridgeInfo,
    label: &str,
    image: &BridgeImage,
) -> Result<()> {
    let progress = Mutex::new(ProgressLine::new(label.to_owned()));
    let line = || progress.lock().unwrap_or_else(PoisonError::into_inner);
    let (stopping, stopped) = tokio::sync::oneshot::channel::<()>();
    let stop = async {
        let _ = stopped.await;
    };
    let updated =
        bridge::update(&mut link, over, info.image, image, stop, |said| line().take(said));
    tokio::pin!(updated);
    let mut stopping = Some(stopping);
    let updated = loop {
        tokio::select! {
            updated = &mut updated => break updated,
            // A handler that would not install is no ctrl-c: the arm is disabled, not fired.
            Ok(()) = tokio::signal::ctrl_c() => {
                let Some(stopping) = stopping.take() else {
                    return Err(crate::Interrupted.into());
                };
                let _ = stopping.send(());
                line().stopping();
            }
        }
    };
    match updated {
        Ok(Updated::AlreadyCurrent) => {
            println!("{label:<6} already runs this image");
            Ok(())
        }
        Ok(Updated::Installed) => Ok(()),
        Err(failed) => Err(failed_error(label, failed)),
    }
}

/// Open the P4 bridge on `path` over its CH343 and wait for its `Ready`.
async fn open_usb(label: &str, path: &str) -> Result<(LinkHandle, BridgeInfo)> {
    // By path: the probe that chose it has already asked, and the reboot leaves the CH343 where
    // it is.
    let mut link = SerialTransport::with_spec(
        wartui_desktop::serial_ports(),
        BridgeSpec::Path(path.to_owned()),
    )
    .remember_in(BridgeMemory::none())
    .start()
    .context("opening the link")?;
    let waited = crate::status::wait_for_ready_or_why(&mut link, Link::Usb, READY_WAIT).await;
    match waited.ready {
        Ok(Some(info)) => Ok((link, info)),
        Err(why) => bail!("{}", no_ready_text(label, path, &why, waited.garbled)),
        Ok(None) => {
            let why = format!("no answer in {} s", READY_WAIT.as_secs());
            bail!("{}", no_ready_text(label, path, &why, waited.garbled))
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

/// What follows a confirm that was not seen.
const ROLLS_BACK: &str = "It rolls back to its old image unless a host confirms this one: run \
                          flash-bridge again, or any wartui command that opens the bridge";

/// How the update ended when it failed, as the operator reads it, the bridge named `label`.
fn failed_error(label: &str, failed: Failed) -> anyhow::Error {
    match failed {
        Failed::Dropped { reason } => anyhow!(
            "the link dropped before the update finished ({reason}); the bridge keeps its image, \
             and flash-bridge can be run again"
        ),
        Failed::Closed => anyhow!("the link closed"),
        Failed::RestartedIntoOther => anyhow!(
            "the bridge restarted into another image before the update finished; run \
             flash-bridge again"
        ),
        Failed::PortStalled { percent } => anyhow!(port_stall_text(percent)),
        Failed::Sender { failure, image_known } => anyhow!(failure_text(failure, image_known)),
        Failed::Queue(error) => anyhow::Error::new(error).context("queueing an update op"),
        Failed::Stopped => anyhow!(
            "stopped before the install; {label} keeps its image, and flash-bridge can be run \
             again"
        ),
        Failed::NoComeBack { over: Link::Usb, secs } => anyhow!(
            "the bridge did not come back within {secs} s of the install; check its CH343 log"
        ),
        Failed::NoComeBack { over: Link::Bluetooth, secs } => anyhow!(
            "the bridge did not come back over Bluetooth within {secs} s of the install; \
             flash-bridge --link usb recovers it, and its CH343 shows its log"
        ),
        Failed::NeverTookInstall => {
            anyhow!("{label} never took the install, and keeps its image; run flash-bridge again")
        }
        Failed::RolledBack => anyhow!("{label} did not boot the new image; rolled back"),
        Failed::OtherImage => anyhow!("{label} came back running another image"),
        Failed::NotConfirmed => {
            anyhow!("{label} installed the new image, but could not confirm it. {ROLLS_BACK}")
        }
        Failed::NoConfirm { secs: Some(secs) } => anyhow!(
            "{label} installed the new image, but no confirm was seen within {secs} s. \
             {ROLLS_BACK}"
        ),
        Failed::NoConfirm { secs: None } => anyhow!(
            "{label} installed the new image, but no confirm was seen over Bluetooth. {ROLLS_BACK}"
        ),
        Failed::NotBack => anyhow!("{label} did not come back"),
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
    /// `Install` is out, so a stop is not taken.
    installing: bool,
}

impl ProgressLine {
    fn new(board: String) -> Self {
        Self { board, tty: std::io::stdout().is_terminal(), drawn: None, installing: false }
    }

    /// The operator pressed ctrl-c.
    fn stopping(&mut self) {
        if self.installing {
            self.say("installing, so the update runs to its outcome; ctrl-c again exits now");
        } else {
            self.say("stopping; ending the bridge's update session");
        }
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

    /// What the update said, as its line.
    fn take(&mut self, said: Progress) {
        match said {
            Progress::ScanNotStopped => {
                self.say("did not report its scan stopped; updating anyway")
            }
            Progress::Sending { percent } => self.show(percent),
            Progress::Installing => {
                self.installing = true;
                self.say("installing; waiting for the bridge to come back");
            }
            Progress::LinkDropped => {
                self.say("the link dropped as the bridge restarts; reconnecting")
            }
            Progress::Confirmed => self.say("installed and confirmed"),
            Progress::RadioRestarted => self.say("the radio restarted on its bundled image"),
            Progress::RadioStillUpdating => {
                self.say("the radio's update is still running; `wartui status` shows its end");
            }
            Progress::Line(Line::Fault(text)) => self.say(&format!("fault  {text}")),
            Progress::Line(Line::Log(text)) => self.say(&format!("Info  {text}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wartui_app::update::image::BRIDGE_PROJECT;
    use wartui_app::update::testing::{P4_APP_HEADER, fixture_with};

    use super::super::{normalize, save_image_args};
    use super::*;

    #[test]
    fn flash_p4_saves_image_for_sixteen_megabytes_when_building() {
        // The P4's table ends at 0xFF1000, which `save-image` refuses under its 4 MB default.
        let args = save_image_args(&P4_BRIDGE_APP, Path::new("fw"), Path::new("e"), Path::new("b"));
        let at = args.iter().position(|arg| arg == "--flash-size").expect("no --flash-size");
        assert_eq!(args[at + 1], "16mb");
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

    #[tokio::test]
    async fn flash_p4_refuses_image_in_a_dry_run_when_it_carries_no_appended_digest() {
        // As the real run refuses it: the digest is what names the image to the P4.
        let mut image = fixture_with(P4_APP_HEADER, BRIDGE_PROJECT, "0.1.0", 0x1000);
        // The header's `hash_appended` flag, cleared, and the SHA-256 it promised taken off.
        image[23] = 0;
        image.truncate(image.len() - 32);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p4.bin");
        std::fs::write(&path, image).unwrap();
        let args = Args {
            bridge: None,
            link: Some(Link::Bluetooth),
            config: None,
            image: Some(path.clone()),
            firmware_dir: None,
            dry_run: true,
        };
        let bridge = BridgeSpec::Mac([0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0]);
        let target = Target::Bluetooth { bridge, memory: BridgeMemory::none() };
        let refused = run(&args, target).await.unwrap_err();
        let expected = format!(
            "{} carries no appended digest, which names the image to the P4",
            path.display()
        );
        assert_eq!(format!("{refused:?}"), expected);
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
        let text = failed_error("71:F4", Failed::PortStalled { percent: 42 }).to_string();
        assert!(text.contains("stopped taking the image at 42%"), "{text}");
        assert!(text.contains("replug the bridge's USB data cable"), "{text}");
        assert!(text.contains("`flash-bridge --link bluetooth`"), "{text}");
    }

    #[test]
    fn flash_p4_names_wired_flash_when_bridge_has_no_update_slot() {
        let failure = Failure::Refused(UpdateState::FlashError);
        let text = failed_error("71:F4", Failed::Sender { failure, image_known: true });
        let text = text.to_string();
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

    #[test]
    fn flash_p4_words_failed_confirm_when_ready_says_confirm_failed() {
        let text = failed_error("71:F4", Failed::NotConfirmed).to_string();
        assert!(text.contains("could not confirm it") && text.contains("rolls back"), "{text}");
        assert!(text.starts_with("71:F4 installed the new image, but "), "{text}");
    }

    #[test]
    fn flash_p4_says_image_kept_when_stopped_while_sending() {
        let text = failed_error("71:F4", Failed::Stopped).to_string();
        assert!(text.starts_with("stopped before the install; 71:F4 keeps its image"), "{text}");
    }

    #[test]
    fn flash_p4_says_rolls_back_when_confirm_is_unseen() {
        let text = failed_error("71:F4", Failed::NoConfirm { secs: Some(3) }).to_string();
        assert!(text.contains("no confirm was seen within 3 s") && text.contains("rolls back"));
        let text = failed_error("71:F4", Failed::NoConfirm { secs: None }).to_string();
        assert!(text.contains("no confirm was seen over Bluetooth") && text.contains("rolls back"));
    }

    #[test]
    fn flash_p4_names_bridge_when_it_comes_back_on_another_image() {
        let text = failed_error("71:F4", Failed::RolledBack).to_string();
        assert_eq!(text, "71:F4 did not boot the new image; rolled back");
        let text = failed_error("71:F4", Failed::OtherImage).to_string();
        assert_eq!(text, "71:F4 came back running another image");
        let text = failed_error("71:F4", Failed::NeverTookInstall).to_string();
        assert_eq!(
            text,
            "71:F4 never took the install, and keeps its image; run flash-bridge again"
        );
        assert_eq!(failed_error("71:F4", Failed::NotBack).to_string(), "71:F4 did not come back");
    }

    #[test]
    fn flash_p4_names_the_link_when_bridge_does_not_come_back() {
        let usb = failed_error("71:F4", Failed::NoComeBack { over: Link::Usb, secs: 120 });
        let usb = usb.to_string();
        assert!(usb.starts_with("the bridge did not come back within 120 s"), "{usb}");
        let bt = failed_error("71:F4", Failed::NoComeBack { over: Link::Bluetooth, secs: 180 });
        let bt = bt.to_string();
        assert!(bt.contains("over Bluetooth within 180 s") && bt.contains("--link usb"), "{bt}");
    }

    #[test]
    fn flash_p4_keeps_queue_error_as_cause_when_an_op_cannot_be_queued() {
        let error = failed_error("71:F4", Failed::Queue(wartui_bridge::SendError::Full));
        assert_eq!(
            format!("{error:?}"),
            "queueing an update op\n\nCaused by:\n    command queue full"
        );
    }
}
