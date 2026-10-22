//! Flashing firmware through `espflash`: what `flash-fleet` and `flash-bridge` share, and where
//! `ota` finds the node's app-only images ([`app`]).
//!
//! **A board's identity is cross-checked, never assumed.** Its USB serial number is its MAC
//! (`ports.rs`), and `espflash board-info` reads the MAC out of the chip's eFuses. The two agreeing
//! is what says the port espflash opened is the board selection judged, rather than a device node
//! that re-enumeration handed to another board in between. A board whose chip or address does not
//! match is left alone: what cannot be identified is never flashed.
//!
//! **The image is checked before any board is touched.** `write-bin` writes whatever bytes it is
//! handed, so the chip ID in the bootloader's image header is compared with the chip the features
//! name, and the image is fetched or built before the first reset. A merged image holds flash from
//! address 0, so the bootloader sits where the chip boots from: 0x0 on a C6, 0x2000 on a C5 or a
//! P4, with erased flash (`0xFF`) ahead of it. An app-only image starts with its own header. A
//! node's and a P4 bridge's images are made with their `partitions.csv`, and a single-chip
//! bridge's with espflash's default table.
//!
//! **A release pins its images by tag.** The host, the fleet and the bridge are flashed from one
//! tree, and the crate version does not move before 1.0, so the version cannot name a tree and the
//! tag can. A build carrying `WARTUI_RELEASE_TAG` fetches that release's image, checks it against
//! the same release's `SHA256SUMS`, and keeps both under the state directory, so a laptop with no
//! signal can still reflash. One `SHA256SUMS` covers every image of a release, so both firmwares
//! share one cache directory per tag. A build without the tag is a checkout, and builds the
//! firmware beside it.
//!
//! **The remembered bridge is read strictly.** Both commands decide which board gets firmware
//! from `[bridge] mac`, so a `wartui.toml` that does not load stops them ([`remembered_bridge`]),
//! where `status` and the like read it as no bridge remembered.
//!
//! **espflash flashes every board, and resets each one on purpose.** The one port opened here is a
//! node's after `flash-fleet` flashes it, read to see it boot ([`boot`]), and it is opened without
//! touching DTR or RTS (`ports.rs`).

pub mod app;
mod boot;
pub mod bridge;
pub mod fleet;
mod p4;

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use wartui_bridge::ports::PortCandidate;
use wartui_bridge::serial::BridgeSpec;
use wartui_proto::mac::{self, Mac};

/// The release this binary was built for, when it was built for one.
const RELEASE_TAG: Option<&str> = option_env!("WARTUI_RELEASE_TAG");

/// Where a release's assets are published.
const RELEASES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/download");

/// The release asset naming every other asset's digest.
const SUMS: &str = "SHA256SUMS";

/// An ESP image header's first byte.
const IMAGE_MAGIC: u8 = 0xE9;

/// Where `esp_image_header_t` keeps the chip ID: after the 8-byte common header and the four
/// SPI pin and drive bytes of the extended one, as a little-endian `u16`.
const CHIP_ID_AT: usize = 12;

/// The whole header, common and extended, which a file must hold to be read at all.
const HEADER_LEN: usize = 24;

/// The bridge saved in `wartui.toml`, from `explicit` (`--config`) or the default file. A
/// missing default file is none remembered; a file that does not load is an error naming it.
fn remembered_bridge(explicit: Option<&Path>) -> Result<Option<Mac>> {
    Ok(crate::config::load(explicit)?.bridge.mac)
}

/// One firmware this checkout builds and a release publishes.
#[derive(Debug)]
struct Firmware {
    /// The firmware in the checkout this binary was built from.
    dir: &'static str,
    /// What `cargo build --release` leaves in the firmware directory.
    elf: &'static str,
    /// The feature sets a release publishes an image for, and the asset each is published as.
    /// Each is built with the firmware's default features. Must match `release.yml`.
    variants: &'static [(&'static [&'static str], &'static str)],
    /// What the firmware runs on, for messages.
    what: &'static str,
    /// The partition table its images are made with, relative to `dir`, or `None` for
    /// espflash's default.
    partition_table: Option<&'static str>,
    /// Whether its images are merged (bootloader, partition table and app, from address 0)
    /// rather than the app alone, as one app slot holds it.
    merged: bool,
}

const NODE: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/node"),
    elf: "target/riscv32imac-unknown-none-elf/release/wartui-node-fw",
    variants: &[
        (&["esp32c5"], "wartui-node-fw-esp32c5.bin"),
        (&["esp32c6"], "wartui-node-fw-esp32c6.bin"),
        (&["esp32c6", "xiao-external-antenna"], "wartui-node-fw-esp32c6-xiao.bin"),
    ],
    what: "node",
    partition_table: Some("partitions.csv"),
    merged: true,
};

/// The node firmware's app-only images, which an over-the-air update carries.
const NODE_APP: Firmware = Firmware {
    variants: &[
        (&["esp32c5"], "wartui-node-app-esp32c5.bin"),
        (&["esp32c6"], "wartui-node-app-esp32c6.bin"),
        (&["esp32c6", "xiao-external-antenna"], "wartui-node-app-esp32c6-xiao.bin"),
    ],
    merged: false,
    ..NODE
};

/// The P4 bridge's merged image, for its first flash over the CH343
/// (`firmware/p4-bridge/README.md`). `flash-bridge` sends it [`P4_BRIDGE_APP`] over its link.
const P4_BRIDGE: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/p4-bridge"),
    elf: "target/riscv32imafc-unknown-none-elf/release/wartui-p4-bridge-fw",
    variants: &[(&[], "wartui-p4-bridge-fw-esp32p4.bin")],
    what: "P4 bridge",
    partition_table: Some("partitions.csv"),
    merged: true,
};

/// The P4 bridge's app-only image, which `flash-bridge` sends it over USB.
const P4_BRIDGE_APP: Firmware =
    Firmware { variants: &[(&[], "wartui-p4-bridge-app-esp32p4.bin")], merged: false, ..P4_BRIDGE };

const BRIDGE: Firmware = Firmware {
    dir: concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/bridge"),
    elf: "target/riscv32imac-unknown-none-elf/release/wartui-bridge-fw",
    variants: &[
        (&["esp32c5"], "wartui-bridge-fw-esp32c5.bin"),
        (&["esp32c5", "t-dongle-c5"], "wartui-bridge-fw-esp32c5-t-dongle.bin"),
        (&["esp32c5", "t-dongle-c5-gps"], "wartui-bridge-fw-esp32c5-t-dongle-gps.bin"),
        (
            &["esp32c5", "t-dongle-c5", "t-dongle-c5-gps"],
            "wartui-bridge-fw-esp32c5-t-dongle-panel-gps.bin",
        ),
        (&["esp32c6"], "wartui-bridge-fw-esp32c6.bin"),
        (&["esp32c6", "xiao-external-antenna"], "wartui-bridge-fw-esp32c6-xiao.bin"),
    ],
    what: "bridge",
    partition_table: None,
    merged: true,
};

fn mac_arg(text: &str) -> Result<Mac, String> {
    mac::parse(text).ok_or_else(|| format!("'{text}' is not an address like 10:BD:A3:EC:44:C0"))
}

/// The chips the firmwares are built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chip {
    Esp32c5,
    Esp32c6,
    Esp32p4,
}

impl Chip {
    /// The chips `--features` names: a node's, or a single-chip bridge's.
    const FEATURED: [Self; 2] = [Self::Esp32c5, Self::Esp32c6];

    /// Every chip an image header here may name.
    const ALL: [Self; 3] = [Self::Esp32c5, Self::Esp32c6, Self::Esp32p4];

    /// Spelled as the firmware's feature, espflash's `--chip` and `board-info`'s `Chip type:`
    /// all spell it.
    const fn name(self) -> &'static str {
        match self {
            Self::Esp32c5 => "esp32c5",
            Self::Esp32c6 => "esp32c6",
            Self::Esp32p4 => "esp32p4",
        }
    }

    /// IDF's `esp_chip_id_t`, as an image header carries it.
    const fn image_id(self) -> u16 {
        match self {
            Self::Esp32c5 => 23,
            Self::Esp32c6 => 13,
            Self::Esp32p4 => 18,
        }
    }

    /// Where the bootloader sits in flash, and so in a merged image.
    const fn boot_offset(self) -> usize {
        match self {
            Self::Esp32c5 | Self::Esp32p4 => 0x2000,
            Self::Esp32c6 => 0x0,
        }
    }
}

/// `--features` as a list, split the way cargo splits it.
fn features(list: &str) -> Vec<&str> {
    list.split([',', ' ']).map(str::trim).filter(|f| !f.is_empty()).collect()
}

/// The one chip the features name, as each firmware's `src/main.rs` demands.
fn chip_of(firmware: &Firmware, features: &[&str]) -> Result<Chip> {
    let named: Vec<Chip> =
        Chip::FEATURED.into_iter().filter(|chip| features.contains(&chip.name())).collect();
    match named.as_slice() {
        [chip] => Ok(*chip),
        [] => bail!("--features names no chip; add esp32c5 or esp32c6"),
        _ => {
            bail!("--features names both esp32c5 and esp32c6; a {} is built for one", firmware.what)
        }
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
    Bridge,
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
            Self::Bridge => f.write_str("the bridge"),
            Self::P4Bridge => f.write_str("a P4 bridge, which `wartui flash-bridge` updates"),
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
/// is a checkout's, else this binary's own release.
fn source(
    firmware: &Firmware,
    image: Option<&Path>,
    firmware_dir: Option<&Path>,
    features: &[&str],
    no_default_features: bool,
    tag: Option<&str>,
) -> Result<Source> {
    if let Some(image) = image {
        return Ok(Source::File(image.to_owned()));
    }
    let tag = tag.filter(|tag| !tag.is_empty());
    let Some(tag) = tag.filter(|_| firmware_dir.is_none()) else {
        return Ok(Source::Build(normalize(firmware_dir.unwrap_or(Path::new(firmware.dir)))));
    };
    let asset = release_asset(firmware, features).filter(|_| !no_default_features);
    let Some(asset) = asset else {
        bail!(
            "release {tag} publishes no {} image for --features {}{}; build one with \
             --firmware-dir, or name one with --image",
            firmware.what,
            features.join(","),
            if no_default_features { " --no-default-features" } else { "" },
        );
    };
    Ok(Source::Release { tag: tag.to_owned(), asset })
}

/// The release asset built with exactly these features, in any order.
fn release_asset(firmware: &Firmware, features: &[&str]) -> Option<&'static str> {
    let mut wanted = features.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    firmware.variants.iter().find_map(|(set, asset)| {
        let mut set = set.to_vec();
        set.sort_unstable();
        (set == wanted).then_some(*asset)
    })
}

/// Get the image onto disk and check it is for `chip`, merged or app-only as `firmware` makes
/// it.
fn obtain(
    firmware: &Firmware,
    source: &Source,
    features: &[&str],
    no_default_features: bool,
    chip: Chip,
) -> Result<PathBuf> {
    let path = fetch(firmware, source, features, no_default_features, chip)?;
    check_image(&read(&path)?, chip, firmware.merged)
        .with_context(|| format!("checking {}", path.display()))?;
    Ok(path)
}

/// Get the image onto disk: named, built or downloaded.
fn fetch(
    firmware: &Firmware,
    source: &Source,
    features: &[&str],
    no_default_features: bool,
    chip: Chip,
) -> Result<PathBuf> {
    match source {
        Source::File(path) => Ok(path.clone()),
        Source::Build(dir) => build_image(firmware, dir, features, no_default_features, chip),
        Source::Release { tag, asset } => {
            let cache =
                state_dir().unwrap_or_else(|| std::env::temp_dir().join("wartui")).join("images");
            release_image(RELEASES, tag, asset, &cache, &http_get)
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
    // Erased flash ahead of the bootloader, when the chip boots from further in than 0x0.
    let at = image.iter().take_while(|byte| **byte == 0xFF).count();
    ensure!(
        image.get(at) == Some(&IMAGE_MAGIC),
        "not an ESP image: no header where a bootloader should start"
    );
    ensure!(image.len() >= at + HEADER_LEN, "too short to hold an image header");
    let id = u16::from_le_bytes([image[at + CHIP_ID_AT], image[at + CHIP_ID_AT + 1]]);
    if id != chip.image_id() {
        let built_for = Chip::ALL
            .into_iter()
            .find(|other| other.image_id() == id)
            .map_or_else(|| format!("chip ID {id}"), |other| other.name().to_owned());
        bail!("the image is built for {built_for}, and --features names {}", chip.name());
    }
    if merged {
        ensure!(
            at == chip.boot_offset(),
            "the image's bootloader is at {at:#x}, and an {} boots from {:#x}",
            chip.name(),
            chip.boot_offset()
        );
    } else {
        ensure!(at == 0, "the image's header is at {at:#x}, and an app-only image starts with it");
    }
    Ok(())
}

/// Build the firmware in `dir` and save its image beside the ELF: merged, or the app alone
/// as `<elf>.app.bin`. Each build of other features writes over the last, so a caller reads
/// the image before the next.
fn build_image(
    firmware: &Firmware,
    dir: &Path,
    features: &[&str],
    no_default_features: bool,
    chip: Chip,
) -> Result<PathBuf> {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(dir).args(["build", "--release"]);
    if !features.is_empty() {
        cargo.args(["--features", &features.join(",")]);
    }
    if no_default_features {
        cargo.arg("--no-default-features");
    }
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
    let merge: &[&str] = if firmware.merged { &["--merge", "--skip-padding"] } else { &[] };
    let mut args: Vec<String> = ["save-image", "--skip-update-check", "--chip", chip.name()]
        .into_iter()
        .chain(merge.iter().copied())
        .map(str::to_owned)
        .collect();
    if let Some(table) = firmware.partition_table {
        args.push("--partition-table".to_owned());
        args.push(dir.join(table).to_string_lossy().into_owned());
    }
    args.push(elf.to_string_lossy().into_owned());
    args.push(bin.to_string_lossy().into_owned());
    let ran = espflash(&args);
    if !ran.ok {
        eprint!("{}{}", ran.stdout, ran.stderr);
        bail!("espflash could not make an image of {}: {}", elf.display(), ran.last_line());
    }
    Ok(bin)
}

/// Where a release keeps one of its assets.
fn asset_url(base: &str, tag: &str, asset: &str) -> String {
    format!("{base}/{tag}/{asset}")
}

/// The digest `SHA256SUMS` gives for `asset`, in the `sha256sum` format: a hex digest, a space,
/// then a space or a `*`, then the name.
fn listed_digest<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let (digest, rest) = line.trim().split_once(' ')?;
        let name = rest.trim_start_matches([' ', '*']);
        (name == asset).then_some(digest)
    })
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Refuse `bytes` unless `SHA256SUMS` lists exactly their digest for `asset`.
fn verify(bytes: &[u8], sums: &str, asset: &str) -> Result<()> {
    let Some(listed) = listed_digest(sums, asset) else {
        bail!("{SUMS} does not list {asset}");
    };
    let actual = sha256_hex(bytes);
    ensure!(listed.eq_ignore_ascii_case(&actual), "{asset} is {actual}, and {SUMS} says {listed}");
    Ok(())
}

/// A release's image, from the cache when it still matches the cached digest, else fetched from
/// `base` and cached. `fetch` is the network, and is a parameter so a test can be without one.
fn release_image(
    base: &str,
    tag: &str,
    asset: &str,
    cache: &Path,
    fetch: &dyn Fn(&str) -> Result<Vec<u8>>,
) -> Result<PathBuf> {
    let dir = cache.join(tag);
    let (sums_path, image_path) = (dir.join(SUMS), dir.join(asset));
    if let (Ok(sums), Ok(image)) = (std::fs::read_to_string(&sums_path), std::fs::read(&image_path))
        && verify(&image, &sums, asset).is_ok()
    {
        return Ok(image_path);
    }
    let sums = fetch(&asset_url(base, tag, SUMS))?;
    let sums = String::from_utf8(sums).with_context(|| format!("{SUMS} is not text"))?;
    let image = fetch(&asset_url(base, tag, asset))?;
    verify(&image, &sums, asset)?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    std::fs::write(&sums_path, &sums)
        .with_context(|| format!("writing {}", sums_path.display()))?;
    std::fs::write(&image_path, &image)
        .with_context(|| format!("writing {}", image_path.display()))?;
    Ok(image_path)
}

fn http_get(url: &str) -> Result<Vec<u8>> {
    let mut response = ureq::get(url).call().with_context(|| format!("fetching {url}"))?;
    response
        .body_mut()
        .with_config()
        .limit(16 * 1024 * 1024)
        .read_to_vec()
        .with_context(|| format!("reading {url}"))
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
    use wartui_bridge::ports::{BRIDGE_PID, ESPRESSIF_VID, PortCandidate, candidate};
    use wartui_proto::mac::{self, Mac};

    use super::Ran;

    pub(super) const BRIDGE_MAC: Mac = [0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0];
    pub(super) const NODE_MAC: Mac = [0x3C, 0xDC, 0x75, 0x84, 0xA1, 0xB0];

    /// The first 24 bytes `espflash save-image --merge` wrote for each chip, from one ELF.
    pub(super) const C6_HEADER: [u8; 24] = [
        0xe9, 0x03, 0x02, 0x20, 0x1a, 0xb9, 0x86, 0x40, 0xee, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];
    pub(super) const C5_HEADER: [u8; 24] = [
        0xe9, 0x03, 0x02, 0x20, 0xaa, 0xbb, 0x84, 0x40, 0xee, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00,
        0x64, 0x00, 0xc7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];
    /// The first 24 bytes `espflash save-image` 4.6 wrote for the P4 bridge's app-only image.
    pub(super) const P4_APP_HEADER: [u8; 24] = [
        0xe9, 0x05, 0x02, 0x20, 0x76, 0x8a, 0x08, 0x40, 0xee, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00,
        0x2c, 0x01, 0x8f, 0x01, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];

    /// `espflash board-info` 4.6 against a C5, laid out as `print_board_info` writes it.
    pub(super) const BOARD_INFO: &str = "\
[2026-09-29T16:13:37Z INFO ] Serial port: '/dev/ttyACM1'
[2026-09-29T16:13:37Z INFO ] Connecting...
[2026-09-29T16:13:38Z INFO ] Using flash stub
Chip type:         esp32c5 (revision v0.1)
Crystal frequency: 48 MHz
Flash size:        8MB
Features:          WiFi 6, BT 5, IEEE802.15.4
MAC address:       3c:dc:75:84:a1:b0

Security Information:
=====================
Flags: 0x00000000 (0)
Key Purposes: [0, 0, 0, 0, 0, 0, 12]
Chip ID: 23
";

    pub(super) fn board(device: &str, address: Option<&Mac>) -> PortCandidate {
        let serial = address.map(|a| mac::full(a).to_string());
        candidate(device, Some(ESPRESSIF_VID), Some(BRIDGE_PID), serial.as_deref())
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
    use std::cell::RefCell;

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
        assert_eq!(remembered_bridge(Some(&path)).unwrap(), Some(BRIDGE_MAC));
    }

    #[test]
    fn flash_refuses_features_when_no_chip_or_both_chips_are_named() {
        assert!(chip_of(&NODE, &features("xiao-external-antenna")).is_err());
        assert!(chip_of(&NODE, &features("")).is_err());
        let error = chip_of(&BRIDGE, &features("esp32c5,esp32c6")).unwrap_err().to_string();
        assert!(error.contains("a bridge is built for one"), "{error}");
    }

    #[test]
    fn flash_resolves_chip_when_features_carry_extras() {
        let chip = chip_of(&NODE, &features("esp32c5,xiao-external-antenna")).unwrap();
        assert_eq!(chip, Chip::Esp32c5);
        assert_eq!(chip_of(&NODE, &features(" esp32c6 , log")).unwrap(), Chip::Esp32c6);
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
        assert_eq!(parse_board_info("MAC address:       3c:dc:75:84:a1:b0"), None);
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
        let table = std::fs::read_to_string(Path::new(NODE.dir).join("partitions.csv")).unwrap();
        let row = table.lines().find(|line| line.starts_with("otadata")).unwrap();
        let cells: Vec<&str> = row.split(',').map(str::trim).collect();
        assert_eq!((cells[3], cells[4]), OTADATA);
        let args = erase_otadata_args("/dev/ttyACM1", Chip::Esp32c6);
        assert_eq!(&args[..5], ["erase-region", "--port", "/dev/ttyACM1", "--chip", "esp32c6"]);
        assert_eq!(&args[args.len() - 2..], ["0xd000", "0x2000"]);
    }

    #[test]
    fn flash_resets_board_when_reboot_is_not_seen() {
        let args = reset_args("/dev/ttyACM1", Chip::Esp32c6);
        assert_eq!(&args[..5], ["reset", "--port", "/dev/ttyACM1", "--chip", "esp32c6"]);
        assert!(args.contains(&"--non-interactive"));
    }

    #[test]
    fn flash_pins_chip_ids_when_reading_image_headers() {
        // IDF's `esp_chip_id_t`, which espflash's `Chip::id` also carries.
        assert_eq!(Chip::Esp32c6.image_id(), 13);
        assert_eq!(Chip::Esp32c5.image_id(), 23);
        assert_eq!(Chip::Esp32p4.image_id(), 18);
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
    fn flash_finds_p4_release_assets_when_features_are_default() {
        assert_eq!(release_asset(&P4_BRIDGE, &[]), Some("wartui-p4-bridge-fw-esp32p4.bin"));
        assert_eq!(release_asset(&P4_BRIDGE_APP, &[]), Some("wartui-p4-bridge-app-esp32p4.bin"));
        assert_eq!(release_asset(&P4_BRIDGE_APP, &["gps"]), None);
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
        let dir =
            source(&NODE, None, Some(Path::new("/x/y/../fw")), &["esp32c5"], false, None).unwrap();
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
    fn flash_finds_gps_bridge_assets_when_features_are_in_any_order() {
        assert_eq!(
            release_asset(&BRIDGE, &["t-dongle-c5-gps", "esp32c5"]),
            Some("wartui-bridge-fw-esp32c5-t-dongle-gps.bin")
        );
        assert_eq!(
            release_asset(&BRIDGE, &["t-dongle-c5-gps", "esp32c5", "t-dongle-c5"]),
            Some("wartui-bridge-fw-esp32c5-t-dongle-panel-gps.bin")
        );
        assert_eq!(release_asset(&BRIDGE, &["esp32c6", "t-dongle-c5-gps"]), None);
    }

    #[test]
    fn flash_builds_download_url_when_given_tag_and_asset() {
        let url = asset_url(RELEASES, "v0.2.0", "wartui-node-fw-esp32c5.bin");
        assert_eq!(
            url,
            "https://github.com/wowy/wartui/releases/download/v0.2.0/wartui-node-fw-esp32c5.bin"
        );
    }

    #[test]
    fn flash_verifies_digest_when_sums_list_the_asset() {
        let sums = format!("{}  a.bin\n{} *b.bin\n", sha256_hex(b"a"), sha256_hex(b"b"));
        verify(b"a", &sums, "a.bin").unwrap();
        verify(b"b", &sums, "b.bin").unwrap();
    }

    #[test]
    fn flash_rejects_download_when_digest_mismatches() {
        let sums = format!("{}  a.bin\n", sha256_hex(b"a"));
        assert!(verify(b"tampered", &sums, "a.bin").is_err());
        assert!(verify(b"a", &sums, "missing.bin").is_err());
    }

    #[test]
    fn flash_fetches_and_caches_image_when_cache_is_empty() {
        let cache = tempfile::tempdir().unwrap();
        let image = c5_image();
        let sums = format!("{}  x.bin\n", sha256_hex(&image));
        let asked = RefCell::new(Vec::new());
        let fetch = |url: &str| -> Result<Vec<u8>> {
            asked.borrow_mut().push(url.to_owned());
            Ok(if url.ends_with(SUMS) { sums.clone().into_bytes() } else { image.clone() })
        };
        let path = release_image("http://test", "v1", "x.bin", cache.path(), &fetch).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), image);
        assert_eq!(*asked.borrow(), ["http://test/v1/SHA256SUMS", "http://test/v1/x.bin"]);
    }

    #[test]
    fn flash_uses_cached_image_when_offline() {
        let cache = tempfile::tempdir().unwrap();
        let dir = cache.path().join("v1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("x.bin"), c5_image()).unwrap();
        std::fs::write(dir.join(SUMS), format!("{}  x.bin\n", sha256_hex(&c5_image()))).unwrap();
        let offline = |_: &str| -> Result<Vec<u8>> { bail!("offline") };
        let path = release_image("http://test", "v1", "x.bin", cache.path(), &offline).unwrap();
        assert_eq!(path, dir.join("x.bin"));
    }

    #[test]
    fn flash_rejects_download_when_fetched_image_mismatches_sums() {
        let cache = tempfile::tempdir().unwrap();
        let sums = format!("{}  x.bin\n", sha256_hex(b"the real one"));
        let fetch = |url: &str| -> Result<Vec<u8>> {
            Ok(if url.ends_with(SUMS) { sums.clone().into_bytes() } else { b"another".to_vec() })
        };
        assert!(release_image("http://test", "v1", "x.bin", cache.path(), &fetch).is_err());
        assert!(!cache.path().join("v1/x.bin").exists(), "a rejected image is not cached");
    }
}
