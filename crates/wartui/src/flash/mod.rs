//! Flashing firmware through `espflash`: what `flash-fleet` and `flash-bridge` share, and where
//! `ota` finds the node's app-only images ([`app`]).
//!
//! **A node's identity is cross-checked, never assumed.** Its USB serial number is its MAC
//! (`ports.rs`), and `espflash board-info` reads the MAC out of the chip's eFuses. The two agreeing
//! is what says the port espflash opened is the board selection judged, rather than a device node
//! that re-enumeration handed to another board in between. A board whose chip or address does not
//! match is left alone: what cannot be identified is never flashed.
//!
//! **The image is checked before any board is touched.** `write-bin` writes whatever bytes it is
//! handed, so the chip ID in the bootloader's image header is compared with the chip being
//! flashed, and the image is fetched or built before the first reset. A merged image holds flash
//! from address 0, so the bootloader sits where the chip boots from: 0x0 on a C6, 0x2000 on a C5
//! or a P4, with erased flash (`0xFF`) ahead of it. An app-only image starts with its own header.
//! A node's and a P4 bridge's images are made with their `partitions.csv`.
//!
//! **A release pins its images by tag.** The host, the fleet and the bridge are flashed from one
//! tree, and the crate version does not move before 1.0, so the version cannot name a tree and the
//! tag can. A build carrying `WARTUI_RELEASE_TAG` fetches that release's image into the state
//! directory (`wartui_app::update::release`). A build without the tag is a checkout, and builds
//! the firmware beside it.
//!
//! **The remembered bridge is read strictly.** `flash-bridge` decides which board gets firmware,
//! and over which link, from `[bridge]`, so a `wartui.toml` that does not load stops it
//! ([`remembered_bridge`]), where `status` and the like read it as no bridge remembered.
//! `flash-fleet` never reads the file: a P4 bridge's CH343 is told apart by its USB IDs.
//!
//! **espflash flashes every node, and resets each one on purpose.** The one port opened here is a
//! node's after `flash-fleet` flashes it, read to see it boot ([`boot`]), and it is opened without
//! touching DTR or RTS (`ports.rs`).

pub mod app;
mod boot;
pub mod bridge;
pub mod fleet;
mod p4;
mod setup;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use wartui_app::update::image::{Chip, ImageError, find_header};
use wartui_app::update::release::{self, ReleaseError, Releases};
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::spec::BridgeSpec;
use wartui_proto::mac::{self, Mac};

/// The release this binary was built for, when it was built for one.
const RELEASE_TAG: Option<&str> = option_env!("WARTUI_RELEASE_TAG");

/// The `[bridge]` table saved in `wartui.toml`, from `explicit` (`--config`) or the default file.
/// A missing default file is none remembered; a file that does not load is an error naming it.
fn remembered_bridge(explicit: Option<&Path>) -> Result<crate::config::Bridge> {
    Ok(crate::config::load(explicit)?.bridge)
}

/// One firmware this checkout builds and a release publishes.
#[derive(Debug)]
struct Firmware {
    /// The firmware in the checkout this binary was built from.
    dir: &'static str,
    /// What `cargo build --release` leaves in the firmware directory.
    elf: &'static str,
    /// What the firmware runs on, for messages.
    what: &'static str,
    /// The chip it is built for, and the only one its images are flashed to.
    chip: Chip,
    /// The partition table its images are made with, relative to `dir`.
    partition_table: &'static str,
    /// The flash size its images name, as `--flash-size` takes it, or `None` for espflash's
    /// default of 4 MB, which `save-image` refuses a table larger than.
    flash_size: Option<&'static str>,
    /// Whether its images are merged (bootloader, partition table and app, from address 0)
    /// rather than the app alone, as one app slot holds it.
    merged: bool,
    /// The release asset of its image. Must match `release.yml`.
    asset: &'static str,
}

/// The node firmware's merged images, which `flash-fleet` writes.
const NODE: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/node"),
    elf: "target/riscv32imac-unknown-none-elf/release/wartui-node-fw",
    what: "node",
    chip: Chip::Esp32c5,
    partition_table: "partitions.csv",
    flash_size: None,
    merged: true,
    asset: "wartui-node-fw-esp32c5.bin",
};

/// The node firmware's app-only images, which an over-the-air update carries.
const NODE_APP: Firmware = Firmware { merged: false, asset: release::NODE_APP_ASSET, ..NODE };

fn mac_arg(text: &str) -> Result<Mac, String> {
    mac::parse(text).ok_or_else(|| format!("'{text}' is not an address like 02:00:5E:10:44:C0"))
}

/// Where `chip`'s bootloader sits in flash, and so in a merged image.
const fn boot_offset(chip: Chip) -> usize {
    match chip {
        Chip::Esp32c5 | Chip::Esp32p4 => 0x2000,
        Chip::Esp32c6 => 0x0,
    }
}

/// Whether `spec` could name this attached board. A tail can match several, which each caller
/// refuses before acting on any of them.
fn matches(spec: &BridgeSpec, candidate: &PortCandidate) -> bool {
    match spec {
        BridgeSpec::Path(path) => *path == candidate.path || *path == candidate.device,
        BridgeSpec::Mac(_) | BridgeSpec::Tail(_) => {
            candidate.mac().is_some_and(|address| spec.matches_address(&address))
        }
    }
}

/// A path named as the bridge, as the device node it resolves to, so a `by-id` link and the
/// node it points at are recognized as one board.
fn canonical(spec: BridgeSpec) -> BridgeSpec {
    match spec {
        BridgeSpec::Path(path) => match std::fs::canonicalize(&path) {
            Ok(device) => BridgeSpec::Path(device.to_string_lossy().into_owned()),
            Err(_) => BridgeSpec::Path(path),
        },
        other => other,
    }
}

/// Why a board was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Skip {
    P4Bridge,
    NotSerialJtag(Option<u16>),
    NoAddress,
    Operator,
    ProbeFailed(String),
    Unreadable,
    OtherChip(String),
    OtherAddress(Mac),
    SmallFlash(Option<u64>),
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::P4Bridge => {
                f.write_str("a CH343, the P4 bridge's port, which `wartui flash-bridge` updates")
            }
            Self::NotSerialJtag(Some(pid)) => {
                write!(f, "not native USB Serial/JTAG (product {pid:04x})")
            }
            Self::NotSerialJtag(None) => f.write_str("reports no product ID"),
            Self::NoAddress => f.write_str("reports no address"),
            Self::Operator => f.write_str("named with --skip"),
            Self::ProbeFailed(line) => write!(f, "espflash board-info failed: {line}"),
            Self::Unreadable => f.write_str("espflash board-info said nothing readable"),
            Self::OtherChip(chip) => write!(f, "an {chip}"),
            Self::OtherAddress(read) => write!(f, "espflash read {} from it", mac::full(read)),
            Self::SmallFlash(Some(bytes)) => write!(
                f,
                "has {} KiB of flash, and a node needs {} KiB for two app slots",
                bytes >> 10,
                NODE_MIN_FLASH >> 10
            ),
            Self::SmallFlash(None) => write!(
                f,
                "espflash read no flash size, and a node needs {} KiB for two app slots",
                NODE_MIN_FLASH >> 10
            ),
        }
    }
}

/// What a child process said, and whether it succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Ran {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl Ran {
    /// The line most likely to say what went wrong.
    fn last_line(&self) -> &str {
        last_line(&self.stderr).or_else(|| last_line(&self.stdout)).unwrap_or("no output")
    }
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

/// What `espflash board-info` read off a board.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BoardInfo {
    chip: String,
    mac: Mac,
    /// The flash size in bytes, or `None` when `board-info` gave none it could read.
    flash_bytes: Option<u64>,
}

/// Read `board-info`'s `Chip type:`, `MAC address:` and `Flash size:` lines. The chip may carry a
/// revision after it, `esp32c5 (revision v0.1)`, which is not the chip's name.
fn parse_board_info(text: &str) -> Option<BoardInfo> {
    let field =
        |label: &str| text.lines().find_map(|line| line.trim().strip_prefix(label).map(str::trim));
    let chip = field("Chip type:")?.split_whitespace().next()?.to_owned();
    let mac = mac::parse(field("MAC address:")?)?;
    let flash_bytes = field("Flash size:").and_then(parse_flash_size);
    Some(BoardInfo { chip, mac, flash_bytes })
}

/// A flash size as espflash prints one, `4MB` or `512KB`, in bytes.
fn parse_flash_size(text: &str) -> Option<u64> {
    let text = text.trim().trim_start_matches('_').to_ascii_uppercase();
    let (digits, unit) = text.split_at(text.find(|c: char| !c.is_ascii_digit())?);
    let scale = match unit.trim() {
        "KB" => 1 << 10,
        "MB" => 1 << 20,
        _ => return None,
    };
    digits.parse::<u64>().ok()?.checked_mul(scale)
}

/// Whether a probed board is the one selection judged, and the chip being flashed. Returns what
/// the probe read, for the checks only one firmware makes.
fn judge_probe(expected: Mac, chip: Chip, probe: &Ran) -> Result<BoardInfo, Skip> {
    if !probe.ok {
        return Err(Skip::ProbeFailed(probe.last_line().to_owned()));
    }
    let info = parse_board_info(&probe.stdout).ok_or(Skip::Unreadable)?;
    if info.chip != chip.name() {
        return Err(Skip::OtherChip(info.chip));
    }
    if info.mac != expected {
        return Err(Skip::OtherAddress(info.mac));
    }
    Ok(info)
}

/// The least flash a node's partition table fits: two 1920 KiB app slots and the data
/// partitions, ending at 0x3D1000 (`firmware/node/partitions.csv`).
const NODE_MIN_FLASH: u64 = 4 << 20;

/// Refuse a board whose flash cannot hold the node's partition table, or that did not say how
/// much it has.
fn judge_node_flash(info: &BoardInfo) -> Result<(), Skip> {
    match info.flash_bytes {
        Some(bytes) if bytes >= NODE_MIN_FLASH => Ok(()),
        size => Err(Skip::SmallFlash(size)),
    }
}

/// Where the node's `otadata` partition sits, and its size (`firmware/node/partitions.csv`).
const OTADATA: (&str, &str) = ("0xd000", "0x2000");

/// Erase a node's `otadata`, so the bootloader boots `ota_0`, where the image just went.
fn erase_otadata_args(port: &str, chip: Chip) -> Vec<&str> {
    vec![
        "erase-region",
        "--port",
        port,
        "--chip",
        chip.name(),
        "--non-interactive",
        "--skip-update-check",
        OTADATA.0,
        OTADATA.1,
    ]
}

/// Reset a board through espflash, for one that did not reboot after it was flashed.
fn reset_args(port: &str, chip: Chip) -> Vec<&str> {
    vec!["reset", "--port", port, "--chip", chip.name(), "--non-interactive", "--skip-update-check"]
}

/// A board by its last two octets, as the fleet table names it, or by path without an address.
fn name(candidate: &PortCandidate) -> String {
    match candidate.mac() {
        Some(address) => mac::short(&address).to_string(),
        None => candidate.path.clone(),
    }
}

/// Where the image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Source {
    File(PathBuf),
    Build(PathBuf),
    Release { tag: String, asset: &'static str },
}

impl Source {
    /// Where the image comes from, worded for whether this run will act on it.
    pub(crate) fn describe(&self, dry_run: bool) -> String {
        match (self, dry_run) {
            (Self::File(path), _) => path.display().to_string(),
            (Self::Build(dir), true) => format!("would build from {}", dir.display()),
            (Self::Build(dir), false) => format!("building from {}", dir.display()),
            (Self::Release { tag, asset }, true) => {
                format!("would fetch {asset} from release {tag}")
            }
            (Self::Release { tag, asset }, false) => format!("{asset} from release {tag}"),
        }
    }
}

/// `path` with `.` dropped and each `..` taken out with the component before it, without asking
/// the filesystem, so a directory that does not exist still reads cleanly in an error. A `..` with
/// nothing before it to remove is kept on a relative path and dropped at the root.
fn normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::RootDir | Component::Prefix(_)) => {}
                _ => out.push(".."),
            },
            other => out.push(other),
        }
    }
    out
}

/// Choose the image's source: a file named, else a build when asked for one or when this binary
/// is a checkout's. Otherwise `firmware`'s asset from the release this binary was built for.
fn source(
    firmware: &Firmware,
    image: Option<&Path>,
    firmware_dir: Option<&Path>,
    tag: Option<&str>,
) -> Source {
    if let Some(image) = image {
        return Source::File(image.to_owned());
    }
    let tag = tag.filter(|tag| !tag.is_empty());
    match tag.filter(|_| firmware_dir.is_none()) {
        Some(tag) => Source::Release { tag: tag.to_owned(), asset: firmware.asset },
        None => Source::Build(normalize(firmware_dir.unwrap_or(Path::new(firmware.dir)))),
    }
}

/// Get the image onto disk and check it is for `firmware`'s chip, merged or app-only as
/// `firmware` makes it.
fn obtain(firmware: &Firmware, source: &Source) -> Result<PathBuf> {
    let path = fetch(firmware, source)?;
    check_image(&read(&path)?, firmware.chip, firmware.merged)
        .with_context(|| format!("checking {}", path.display()))?;
    Ok(path)
}

/// Get the image onto disk: named, built or downloaded.
fn fetch(firmware: &Firmware, source: &Source) -> Result<PathBuf> {
    match source {
        Source::File(path) => Ok(path.clone()),
        Source::Build(dir) => build_image(firmware, dir),
        Source::Release { tag, asset } => {
            let cache =
                state_dir().unwrap_or_else(|| std::env::temp_dir().join("wartui")).join("images");
            let cached = release::fetch(&Releases::live(), tag, asset, &cache);
            Ok(cached.map_err(release_error)?.path().to_owned())
        }
    }
}

/// This host's directory for state a program rebuilds when it has to.
///
/// `XDG_STATE_HOME` is honored only when it is absolute: the specification says a
/// relative value is invalid, and taking one literally would scatter a `wartui`
/// directory through whichever directory a flash was started from.
fn state_dir() -> Option<PathBuf> {
    if cfg!(target_os = "macos") {
        let home = std::env::var_os("HOME")?;
        return Some(Path::new(&home).join("Library/Application Support/wartui"));
    }
    if let Some(state) = std::env::var_os("XDG_STATE_HOME") {
        let state = PathBuf::from(state);
        if state.is_absolute() {
            return Some(state.join("wartui"));
        }
    }
    let home = std::env::var_os("HOME")?;
    Some(Path::new(&home).join(".local/state/wartui"))
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Refuse an image not built for `chip`: a merged one by its bootloader's header, an app-only
/// one by its own.
fn check_image(image: &[u8], chip: Chip, merged: bool) -> Result<()> {
    // Past erased flash ahead of the bootloader, when the chip boots from further in than 0x0.
    let at =
        find_header(image, chip).map_err(|error| anyhow::anyhow!(app::image_error_text(&error)))?;
    if merged {
        ensure!(
            at == boot_offset(chip),
            "the image's bootloader is at {at:#x}, and an {} boots from {:#x}",
            chip.name(),
            boot_offset(chip)
        );
    } else {
        ensure!(at == 0, "{}", app::image_error_text(&ImageError::NotAtStart { at }));
    }
    Ok(())
}

/// Build the firmware in `dir` with `cargo build --release`, and save its image beside the ELF:
/// merged, or the app alone as `<elf>.app.bin`.
fn build_image(firmware: &Firmware, dir: &Path) -> Result<PathBuf> {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(dir).args(["build", "--release"]);
    // The firmware directory's `rust-toolchain.toml` and `.cargo/config.toml` decide the
    // toolchain and the target, and these would override them — the first is set by rustup for
    // anything run under `cargo run`, and the second moves the ELF from where it is looked for.
    cargo.env_remove("RUSTUP_TOOLCHAIN").env_remove("CARGO_TARGET_DIR");
    let status = cargo.status().context("running cargo")?;
    ensure!(
        status.success(),
        "building the {} firmware in {} failed",
        firmware.what,
        dir.display()
    );

    let elf = dir.join(firmware.elf);
    let bin = elf.with_extension(if firmware.merged { "bin" } else { "app.bin" });
    let ran = espflash(&save_image_args(firmware, dir, &elf, &bin));
    if !ran.ok {
        eprint!("{}{}", ran.stdout, ran.stderr);
        bail!("espflash could not make an image of {}: {}", elf.display(), ran.last_line());
    }
    Ok(bin)
}

/// The `espflash save-image` arguments that save `elf` from the firmware in `dir` as `bin`.
fn save_image_args(firmware: &Firmware, dir: &Path, elf: &Path, bin: &Path) -> Vec<String> {
    let merge: &[&str] = if firmware.merged { &["--merge", "--skip-padding"] } else { &[] };
    let mut args: Vec<String> =
        ["save-image", "--skip-update-check", "--chip", firmware.chip.name()]
            .into_iter()
            .chain(merge.iter().copied())
            .map(str::to_owned)
            .collect();
    args.push("--partition-table".to_owned());
    args.push(dir.join(firmware.partition_table).to_string_lossy().into_owned());
    if let Some(size) = firmware.flash_size {
        args.extend(["--flash-size".to_owned(), size.to_owned()]);
    }
    args.push(elf.to_string_lossy().into_owned());
    args.push(bin.to_string_lossy().into_owned());
    args
}

/// Why a release's image could not be had, as an operator reads it.
fn release_error(error: ReleaseError) -> anyhow::Error {
    let sums = release::SUMS;
    match error {
        ReleaseError::Fetch { url, error } => {
            anyhow::Error::msg(error).context(format!("fetching {url}"))
        }
        ReleaseError::Read { url, error } => {
            anyhow::Error::msg(error).context(format!("reading {url}"))
        }
        ReleaseError::SumsNotText(error) => {
            anyhow::Error::new(error).context(format!("{sums} is not text"))
        }
        ReleaseError::Unlisted { asset } => anyhow::anyhow!("{sums} does not list {asset}"),
        ReleaseError::Mismatch { asset, actual, listed } => {
            anyhow::anyhow!("{asset} is {actual}, and {sums} says {listed}")
        }
        ReleaseError::CreateDir { dir, error } => {
            anyhow::Error::new(error).context(format!("creating {}", dir.display()))
        }
        ReleaseError::Write { path, error } => {
            anyhow::Error::new(error).context(format!("writing {}", path.display()))
        }
    }
}

/// Refuse the run up front when there is no `espflash` to run.
fn espflash_present() -> Result<()> {
    match Command::new("espflash").arg("--version").stdin(Stdio::null()).output() {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            bail!("espflash is not on PATH; install it with `cargo install espflash`")
        }
        Err(error) => Err(error).context("running espflash"),
    }
}

fn probe_args(port: &str) -> Vec<&str> {
    vec!["board-info", "--port", port, "--non-interactive", "--skip-update-check"]
}

/// Flash with `--chip` as well, so espflash itself refuses a board that is some other part.
fn flash_args<'a>(port: &'a str, chip: Chip, image: &'a Path) -> Vec<String> {
    let image = image.to_string_lossy().into_owned();
    ["write-bin", "--port", port, "--chip", chip.name()]
        .into_iter()
        .map(str::to_owned)
        .chain(["--non-interactive".into(), "--skip-update-check".into(), "0x0".into(), image])
        .collect()
}

/// Run espflash to completion, keeping what it said.
fn espflash<S: AsRef<std::ffi::OsStr>>(args: &[S]) -> Ran {
    match Command::new("espflash").args(args).stdin(Stdio::null()).output() {
        Ok(output) => Ran {
            ok: output.status.success(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        },
        Err(error) => {
            Ran { ok: false, stdout: String::new(), stderr: format!("running espflash: {error}") }
        }
    }
}

/// Fixtures the tests of every flashing command share.
#[cfg(test)]
mod testing {
    pub(super) use wartui_app::update::testing::{C5_HEADER, C6_HEADER, P4_APP_HEADER};
    use wartui_bridge::ports::{
        CH343_PID, CH343_VID, ESPRESSIF_VID, PortCandidate, SERIAL_JTAG_PID, candidate,
    };
    use wartui_bridge::serial::Probed;
    use wartui_proto::mac::{self, Mac};

    use super::Ran;

    pub(super) const BRIDGE_MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];
    pub(super) const NODE_MAC: Mac = [0x02, 0x00, 0x5E, 0x10, 0xA1, 0xB0];

    /// `espflash board-info` 4.6 against a C5, laid out as `print_board_info` writes it.
    pub(super) const BOARD_INFO: &str = "\
[2026-09-29T16:13:37Z INFO ] Serial port: '/dev/ttyACM1'
[2026-09-29T16:13:37Z INFO ] Connecting...
[2026-09-29T16:13:38Z INFO ] Using flash stub
Chip type:         esp32c5 (revision v0.1)
Crystal frequency: 48 MHz
Flash size:        8MB
Features:          WiFi 6, BT 5, IEEE802.15.4
MAC address:       02:00:5e:10:a1:b0

Security Information:
=====================
Flags: 0x00000000 (0)
Key Purposes: [0, 0, 0, 0, 0, 0, 12]
Chip ID: 23
";

    pub(super) fn board(device: &str, address: Option<&Mac>) -> PortCandidate {
        let serial = address.map(|a| mac::full(a).to_string());
        candidate(device, Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), serial.as_deref())
    }

    /// A CH343 as the OS lists it, with a serial number of its own.
    pub(super) fn ch343(device: &str) -> PortCandidate {
        candidate(device, Some(CH343_VID), Some(CH343_PID), Some(device))
    }

    /// A CH343 whose probe answered as the bridge `address`.
    pub(super) fn bridge(device: &str, address: &Mac) -> Probed {
        Probed { candidate: ch343(device), opened: true, mac: Some(*address) }
    }

    /// A CH343 whose probe heard nothing: a blank P4, or another device.
    pub(super) fn silent(device: &str) -> Probed {
        Probed { candidate: ch343(device), opened: true, mac: None }
    }

    pub(super) fn c5_image() -> Vec<u8> {
        let mut image = vec![0xFF; 0x2000];
        image.extend_from_slice(&C5_HEADER);
        image.extend_from_slice(&[0x13; 64]);
        image
    }

    pub(super) fn p4_app_image() -> Vec<u8> {
        let mut image = P4_APP_HEADER.to_vec();
        image.extend_from_slice(&[0x13; 64]);
        image
    }

    pub(super) fn c6_image() -> Vec<u8> {
        let mut image = C6_HEADER.to_vec();
        image.extend_from_slice(&[0x13; 64]);
        image
    }

    pub(super) fn ran(ok: bool, stdout: &str) -> Ran {
        Ran { ok, stdout: stdout.to_owned(), stderr: String::new() }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn flash_refuses_remembered_bridge_when_config_does_not_parse() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "this is [not toml\n").unwrap();
        let error = remembered_bridge(Some(&path)).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn flash_reads_remembered_bridge_when_config_holds_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, format!("[bridge]\nmac = \"{}\"\n", mac::full(&BRIDGE_MAC))).unwrap();
        assert_eq!(remembered_bridge(Some(&path)).unwrap().mac, Some(BRIDGE_MAC));
    }

    #[test]
    fn flash_parses_board_info_when_given_espflash_transcript() {
        let info = parse_board_info(BOARD_INFO).unwrap();
        let flash_bytes = Some(8 << 20);
        assert_eq!(info, BoardInfo { chip: "esp32c5".to_owned(), mac: NODE_MAC, flash_bytes });
    }

    #[test]
    fn flash_returns_none_when_board_info_is_garbage() {
        assert_eq!(parse_board_info("Error: espflash::connection_failed\n  x timed out"), None);
        assert_eq!(parse_board_info("Chip type:         esp32c5 (revision v0.1)"), None);
        assert_eq!(parse_board_info("MAC address:       02:00:5e:10:a1:b0"), None);
    }

    #[test]
    fn flash_accepts_probe_when_chip_and_mac_match() {
        let info = judge_probe(NODE_MAC, Chip::Esp32c5, &ran(true, BOARD_INFO)).unwrap();
        assert_eq!(info.mac, NODE_MAC);
    }

    #[test]
    fn flash_rejects_probed_board_when_chip_differs() {
        let judged = judge_probe(NODE_MAC, Chip::Esp32c6, &ran(true, BOARD_INFO));
        assert_eq!(judged, Err(Skip::OtherChip("esp32c5".to_owned())));
    }

    #[test]
    fn flash_rejects_probed_board_when_mac_differs() {
        let judged = judge_probe(BRIDGE_MAC, Chip::Esp32c5, &ran(true, BOARD_INFO));
        assert_eq!(judged, Err(Skip::OtherAddress(NODE_MAC)));
    }

    #[test]
    fn flash_rejects_probed_board_when_espflash_fails() {
        let failed = Ran { ok: false, stdout: String::new(), stderr: "\n  x timed out\n".into() };
        let judged = judge_probe(NODE_MAC, Chip::Esp32c5, &failed);
        assert_eq!(judged, Err(Skip::ProbeFailed("x timed out".to_owned())));
        assert_eq!(judge_probe(NODE_MAC, Chip::Esp32c5, &ran(true, "")), Err(Skip::Unreadable));
    }

    #[test]
    fn flash_parses_flash_size_when_given_espflash_spellings() {
        assert_eq!(parse_flash_size("4MB"), Some(4 << 20));
        assert_eq!(parse_flash_size("16MB"), Some(16 << 20));
        assert_eq!(parse_flash_size("512KB"), Some(512 << 10));
        assert_eq!(parse_flash_size("_2MB"), Some(2 << 20), "the enum's own spelling");
        assert_eq!(parse_flash_size("unknown"), None);
        assert_eq!(parse_flash_size("4"), None);
        assert_eq!(parse_flash_size("4GB"), None);
    }

    #[test]
    fn flash_reads_no_flash_size_when_board_info_omits_it() {
        let text = BOARD_INFO.replace("Flash size:        8MB\n", "");
        assert_eq!(parse_board_info(&text).unwrap().flash_bytes, None);
    }

    #[test]
    fn flash_accepts_node_board_when_flash_is_at_least_four_megabytes() {
        let info = parse_board_info(BOARD_INFO).unwrap();
        assert_eq!(judge_node_flash(&info), Ok(()));
        let four = BoardInfo { flash_bytes: Some(4 << 20), ..info };
        assert_eq!(judge_node_flash(&four), Ok(()));
    }

    #[test]
    fn flash_refuses_node_board_when_flash_is_under_four_megabytes() {
        let text = BOARD_INFO.replace("8MB", "2MB");
        let info = judge_probe(NODE_MAC, Chip::Esp32c5, &ran(true, &text)).unwrap();
        let judged = judge_node_flash(&info);
        assert_eq!(judged, Err(Skip::SmallFlash(Some(2 << 20))));
        let message = judged.unwrap_err().to_string();
        assert!(message.contains("2048 KiB") && message.contains("4096 KiB"), "{message}");
        let unknown = BoardInfo { flash_bytes: None, ..info };
        assert_eq!(judge_node_flash(&unknown), Err(Skip::SmallFlash(None)));
    }

    #[test]
    fn flash_erases_otadata_when_node_is_flashed() {
        // The partition table's `otadata` row, which these must match.
        let dir = Path::new(NODE.dir);
        let table = std::fs::read_to_string(dir.join("partitions.csv")).unwrap();
        let row = table.lines().find(|line| line.starts_with("otadata")).unwrap();
        let cells: Vec<&str> = row.split(',').map(str::trim).collect();
        assert_eq!((cells[3], cells[4]), OTADATA);
        let args = erase_otadata_args("/dev/ttyACM1", Chip::Esp32c5);
        assert_eq!(&args[..5], ["erase-region", "--port", "/dev/ttyACM1", "--chip", "esp32c5"]);
        assert_eq!(&args[args.len() - 2..], ["0xd000", "0x2000"]);
    }

    #[test]
    fn flash_leaves_flash_size_to_espflash_when_saving_node_image() {
        let (dir, elf, bin) = (Path::new("fw"), Path::new("e"), Path::new("b"));
        for firmware in [&NODE, &NODE_APP] {
            let args = save_image_args(firmware, dir, elf, bin);
            assert!(!args.iter().any(|arg| arg == "--flash-size"), "{args:?}");
        }
    }

    #[test]
    fn flash_resets_board_when_reboot_is_not_seen() {
        let args = reset_args("/dev/ttyACM1", Chip::Esp32c5);
        assert_eq!(&args[..5], ["reset", "--port", "/dev/ttyACM1", "--chip", "esp32c5"]);
        assert!(args.contains(&"--non-interactive"));
    }

    #[test]
    fn flash_accepts_app_image_when_header_names_the_p4() {
        check_image(&p4_app_image(), Chip::Esp32p4, false).unwrap();
    }

    #[test]
    fn flash_refuses_p4_app_image_when_header_names_another_chip() {
        let error = check_image(&c6_image(), Chip::Esp32p4, false).unwrap_err().to_string();
        assert!(error.contains("built for esp32c6"), "{error}");
    }

    #[test]
    fn flash_refuses_app_image_when_given_a_merged_one() {
        // A merged P4 image, its bootloader behind 0x2000 of erased flash.
        let mut merged = vec![0xFF; 0x2000];
        merged.extend_from_slice(&p4_app_image());
        let error = check_image(&merged, Chip::Esp32p4, false).unwrap_err().to_string();
        assert!(error.contains("app-only"), "{error}");
        check_image(&merged, Chip::Esp32p4, true).unwrap();
    }

    #[test]
    fn flash_accepts_image_when_header_names_the_chip() {
        check_image(&c5_image(), Chip::Esp32c5, true).unwrap();
        check_image(&c6_image(), Chip::Esp32c6, true).unwrap();
    }

    #[test]
    fn flash_refuses_image_when_header_names_another_chip() {
        let error = check_image(&c6_image(), Chip::Esp32c5, true).unwrap_err().to_string();
        assert!(error.contains("built for esp32c6"), "{error}");
        let error = check_image(&c5_image(), Chip::Esp32c6, true).unwrap_err().to_string();
        assert!(error.contains("built for esp32c5"), "{error}");
    }

    #[test]
    fn flash_refuses_image_when_bootloader_is_at_the_wrong_offset() {
        // A C5 header at 0x0, where a C5 never boots from.
        let mut image = C5_HEADER.to_vec();
        image.extend_from_slice(&[0x13; 64]);
        assert!(check_image(&image, Chip::Esp32c5, true).is_err());
    }

    #[test]
    fn flash_refuses_image_when_file_is_short() {
        assert!(check_image(&C5_HEADER[..16], Chip::Esp32c5, true).is_err());
        assert!(check_image(&[], Chip::Esp32c5, true).is_err());
        assert!(check_image(&c5_image()[..0x2000 + 8], Chip::Esp32c5, true).is_err());
    }

    #[test]
    fn flash_refuses_image_when_magic_is_missing() {
        let mut image = c6_image();
        image[0] = 0x7F;
        assert!(check_image(&image, Chip::Esp32c6, true).is_err());
    }

    #[test]
    fn flash_normalizes_firmware_dir_when_path_has_parent_segments() {
        assert_eq!(
            normalize(Path::new("/a/b/crates/wartui/../../firmware/node")),
            Path::new("/a/b/firmware/node")
        );
        assert_eq!(normalize(Path::new("./fw/./node/")), Path::new("fw/node"));
        assert_eq!(normalize(Path::new("../fw/x/..")), Path::new("../fw"));
        assert_eq!(normalize(Path::new("/../fw")), Path::new("/fw"));
        let dir = source(&NODE, None, Some(Path::new("/x/y/../fw")), None);
        assert_eq!(dir, Source::Build("/x/fw".into()));
    }

    #[test]
    fn flash_words_source_as_future_when_dry_run() {
        let build = Source::Build("/fw".into());
        assert_eq!(build.describe(true), "would build from /fw");
        let release = Source::Release { tag: "v1".to_owned(), asset: "x.bin" };
        assert_eq!(release.describe(true), "would fetch x.bin from release v1");
        assert_eq!(Source::File("fw.bin".into()).describe(true), "fw.bin");
    }

    #[test]
    fn flash_words_source_as_present_when_real_run() {
        assert_eq!(Source::Build("/fw".into()).describe(false), "building from /fw");
        let release = Source::Release { tag: "v1".to_owned(), asset: "x.bin" };
        assert_eq!(release.describe(false), "x.bin from release v1");
        assert_eq!(Source::File("fw.bin".into()).describe(false), "fw.bin");
    }

    #[test]
    fn flash_words_release_refusal_when_sums_do_not_match() {
        let error = release_error(ReleaseError::Unlisted { asset: "x.bin".to_owned() });
        assert_eq!(format!("{error:?}"), "SHA256SUMS does not list x.bin");
        let (asset, actual, listed) = ("x.bin".to_owned(), "ab".to_owned(), "cd".to_owned());
        let error = release_error(ReleaseError::Mismatch { asset, actual, listed });
        assert_eq!(format!("{error:?}"), "x.bin is ab, and SHA256SUMS says cd");
    }

    #[test]
    fn flash_words_release_refusal_when_a_fetch_fails() {
        let url = "http://test/v1/x.bin".to_owned();
        let failed = ReleaseError::Fetch { url: url.clone(), error: "io: refused".to_owned() };
        let error = format!("{:?}", release_error(failed));
        assert_eq!(error, "fetching http://test/v1/x.bin\n\nCaused by:\n    io: refused");
        let failed = ReleaseError::Read { url, error: "too big".to_owned() };
        let error = format!("{:?}", release_error(failed));
        assert_eq!(error, "reading http://test/v1/x.bin\n\nCaused by:\n    too big");
    }

    #[test]
    fn flash_words_release_refusal_when_the_cache_cannot_be_written() {
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let failed = ReleaseError::CreateDir { dir: "/c/v1".into(), error };
        assert_eq!(release_error(failed).to_string(), "creating /c/v1");
        let error = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let failed = ReleaseError::Write { path: "/c/v1/x.bin".into(), error };
        assert_eq!(release_error(failed).to_string(), "writing /c/v1/x.bin");
        let sums = String::from_utf8(vec![0xFF]).unwrap_err();
        let error = release_error(ReleaseError::SumsNotText(sums));
        assert_eq!(error.to_string(), "SHA256SUMS is not text");
        assert_eq!(error.chain().count(), 2, "the decode error follows");
    }
}
