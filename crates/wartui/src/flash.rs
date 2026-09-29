//! `wartui flash-fleet` — one node image onto every attached board of one chip.
//!
//! **The bridge is spared before anything else happens, or nothing happens.** It is a C5 or C6
//! with the same vendor and product ID as every node, so nothing on the bus tells it apart but its
//! address, and a node image written over it takes the fleet's radio away until somebody reflashes
//! it by hand. The run therefore needs that address — `--bridge`, else the one `run` remembered —
//! and refuses without one unless `--no-bridge` says no bridge is attached. That flag only lifts
//! the refusal: a remembered bridge is spared all the same, since sparing it costs nothing and a
//! board known to be the bridge is never worth the risk. The chip cannot stand in for the address:
//! the bridge is often the same part as the nodes.
//!
//! **Every bridge known is spared, and a named one must be attached.** A `--bridge` value and the
//! remembered address are both spared, not one in place of the other. A path that matches nothing
//! attached — a typo, a device node that moved on a replug, a macOS `/dev/tty.*` where only the
//! `/dev/cu.*` is listed — would otherwise spare nothing, and the real bridge would pass its probe
//! and be flashed. So a `--bridge` value that matches no attached board refuses the run. A
//! remembered address that matches nothing is not an error: the bridge may simply be unplugged.
//!
//! **A board's identity is cross-checked, never assumed.** Its USB serial number is its MAC
//! (`ports.rs`), and `espflash board-info` reads the MAC out of the chip's eFuses. The two agreeing
//! is what says the port espflash opened is the board selection judged, rather than a device node
//! that re-enumeration handed to another board in between. A board whose chip or address does not
//! match is skipped, neither flashed nor failed: what cannot be identified is left alone.
//!
//! **The image is checked before any board is touched.** `write-bin` writes whatever bytes it is
//! handed, so the chip ID in the bootloader's image header is compared with the chip the features
//! name, and the image is fetched or built before the first reset. A merged image holds flash from
//! address 0, so the bootloader sits where the chip boots from: 0x0 on a C6, 0x2000 on a C5, with
//! erased flash (`0xFF`) ahead of it.
//!
//! **A release pins its node image by tag.** The host and the fleet are flashed from one tree, and
//! the crate version does not move before 1.0, so the version cannot name a tree and the tag can.
//! A build carrying `WARTUI_RELEASE_TAG` fetches that release's image, checks it against the same
//! release's `SHA256SUMS`, and keeps both under the state directory, so a laptop with no signal can
//! still reflash. A build without the tag is a checkout, and builds the firmware beside it.
//!
//! Nothing here opens a serial port. Every board is reached through `espflash`, which resets each
//! one on purpose, so the DTR/RTS rule in `ports.rs` is not in play.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};
use clap::Args as ClapArgs;
use sha2::{Digest, Sha256};
use wartui_bridge::ports::{BRIDGE_PID, PortCandidate, parse_mac};
use wartui_bridge::remember::{BridgeMemory, state_dir};
use wartui_bridge::serial::{BridgeSpec, discover_ports};
use wartui_proto::link::Mac;

use crate::{mac, spec};

/// The release this binary was built for, when it was built for one.
const RELEASE_TAG: Option<&str> = option_env!("WARTUI_RELEASE_TAG");

/// Where a release's assets are published.
const RELEASES: &str = concat!(env!("CARGO_PKG_REPOSITORY"), "/releases/download");

/// The node firmware in the checkout this binary was built from.
const FIRMWARE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../firmware/node");

/// What `cargo build --release` leaves in the firmware directory.
const FIRMWARE_ELF: &str = "target/riscv32imac-unknown-none-elf/release/wartui-node-fw";

/// The feature sets a release publishes an image for, and the asset each is published as.
/// Every one is built with the firmware's default features.
const VARIANTS: &[(&[&str], &str)] = &[
    (&["esp32c5"], "wartui-node-fw-esp32c5.bin"),
    (&["esp32c6"], "wartui-node-fw-esp32c6.bin"),
    (&["esp32c6", "xiao-external-antenna"], "wartui-node-fw-esp32c6-xiao.bin"),
];

/// The release asset naming every other asset's digest.
const SUMS: &str = "SHA256SUMS";

/// An ESP image header's first byte.
const IMAGE_MAGIC: u8 = 0xE9;

/// Where `esp_image_header_t` keeps the chip ID: after the 8-byte common header and the four
/// SPI pin and drive bytes of the extended one, as a little-endian `u16`.
const CHIP_ID_AT: usize = 12;

/// The whole header, common and extended, which a file must hold to be read at all.
const HEADER_LEN: usize = 24;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The node firmware's features, comma-separated, as `cargo build` takes them. Exactly one
    /// of `esp32c5` or `esp32c6`, and only boards of that chip are flashed.
    #[arg(long, value_name = "LIST", required = true)]
    pub features: String,

    /// Build without the firmware's default features.
    #[arg(long)]
    pub no_default_features: bool,

    /// Which board is the bridge, by path or by address, so that it is never flashed. It must be
    /// attached. The one `run` last found is spared as well, and is the only one by default.
    #[arg(long, value_name = "PATH|MAC", conflicts_with = "no_bridge")]
    pub bridge: Option<String>,

    /// No bridge is attached. Without a bridge's address the run is refused, because any board
    /// could be it; this lifts that refusal. A remembered bridge is still spared.
    #[arg(long)]
    pub no_bridge: bool,

    /// Leave this board alone as well. Repeatable.
    #[arg(long = "skip", value_name = "MAC", value_parser = mac_arg)]
    pub skip: Vec<Mac>,

    /// Flash this merged image, which holds flash from address 0, rather than building or
    /// downloading one.
    #[arg(long, value_name = "BIN", conflicts_with = "firmware_dir")]
    pub image: Option<PathBuf>,

    /// Build the image from the node firmware in this directory.
    #[arg(long, value_name = "DIR")]
    pub firmware_dir: Option<PathBuf>,

    /// How many boards to probe or flash at once.
    #[arg(long, value_name = "N", default_value_t = 4,
          value_parser = clap::value_parser!(u16).range(1..))]
    pub jobs: u16,

    /// Say where the image would come from and which boards would be flashed, then stop. Builds,
    /// downloads and flashes nothing.
    #[arg(long)]
    pub dry_run: bool,
}

fn mac_arg(text: &str) -> Result<Mac, String> {
    parse_mac(text).ok_or_else(|| format!("'{text}' is not an address like 10:BD:A3:EC:44:C0"))
}

pub fn run(args: Args) -> Result<()> {
    let features = features(&args.features);
    let chip = chip_of(&features)?;
    espflash_present()?;
    let mut bridge =
        bridge_to_spare(args.bridge.as_deref(), BridgeMemory::discover().recall(), args.no_bridge)?;
    bridge.named = bridge.named.map(|(given, spec)| (given, canonical(spec)));
    let source = source(
        args.image.as_deref(),
        args.firmware_dir.as_deref(),
        &features,
        args.no_default_features,
        RELEASE_TAG,
    )?;
    println!("image  {}", source.describe(args.dry_run));

    let image = if args.dry_run {
        // A file named on the command line costs nothing to read, and a wrong one is worth
        // hearing about before the real run.
        if let Source::File(path) = &source {
            check_image(&read(path)?, chip)?;
        }
        None
    } else {
        Some(obtain(&source, &features, args.no_default_features, chip)?)
    };

    let found = discover_ports().context("listing serial ports")?;
    named_bridge_attached(&found, &bridge)?;
    let mut probe = Vec::new();
    let mut report = Vec::new();
    for verdict in select(&found, &bridge, &args.skip) {
        match verdict {
            Verdict::Probe(candidate) => probe.push(candidate),
            Verdict::Skip(candidate, why) => report.push((candidate, Outcome::Skipped(why))),
        }
    }
    let jobs = usize::from(args.jobs);
    let probed = in_parallel(&probe, jobs, |candidate| {
        let expected = candidate.mac().expect("select probes only boards with an address");
        judge_probe(expected, chip, &espflash(&probe_args(&candidate.path)))
    });
    let mut verified = Vec::new();
    for (candidate, judged) in probe.into_iter().zip(probed) {
        match judged {
            Ok(()) => verified.push(candidate),
            Err(why) => report.push((candidate, Outcome::Skipped(why))),
        }
    }

    let failed = match &image {
        None => {
            report.extend(verified.into_iter().map(|c| (c, Outcome::WouldFlash)));
            0
        }
        Some(image) => {
            let flashed = in_parallel(&verified, jobs, |candidate| {
                espflash(&flash_args(&candidate.path, chip, image))
            });
            let mut failed = 0;
            for (candidate, ran) in verified.into_iter().zip(flashed) {
                if ran.ok {
                    report.push((candidate, Outcome::Flashed));
                } else {
                    failed += 1;
                    report.push((candidate, Outcome::Failed(ran)));
                }
            }
            failed
        }
    };

    if report.is_empty() {
        println!("No Espressif board is attached.");
    }
    report.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    for (candidate, outcome) in &report {
        println!("{:<6} {outcome}", name(candidate));
        if let Outcome::Failed(ran) = outcome {
            for line in ran.stdout.lines().chain(ran.stderr.lines()) {
                println!("         {line}");
            }
        }
    }
    ensure!(failed == 0, "{failed} board(s) failed to flash");
    Ok(())
}

/// The chips a node is built for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chip {
    Esp32c5,
    Esp32c6,
}

impl Chip {
    const ALL: [Self; 2] = [Self::Esp32c5, Self::Esp32c6];

    /// Spelled as the firmware's feature, espflash's `--chip` and `board-info`'s `Chip type:`
    /// all spell it.
    const fn name(self) -> &'static str {
        match self {
            Self::Esp32c5 => "esp32c5",
            Self::Esp32c6 => "esp32c6",
        }
    }

    /// IDF's `esp_chip_id_t`, as an image header carries it.
    const fn image_id(self) -> u16 {
        match self {
            Self::Esp32c5 => 23,
            Self::Esp32c6 => 13,
        }
    }

    /// Where the bootloader sits in flash, and so in a merged image.
    const fn boot_offset(self) -> usize {
        match self {
            Self::Esp32c5 => 0x2000,
            Self::Esp32c6 => 0x0,
        }
    }
}

/// `--features` as a list, split the way cargo splits it.
fn features(list: &str) -> Vec<&str> {
    list.split([',', ' ']).map(str::trim).filter(|f| !f.is_empty()).collect()
}

/// The one chip the features name, as `firmware/node/src/main.rs` demands.
fn chip_of(features: &[&str]) -> Result<Chip> {
    let named: Vec<Chip> =
        Chip::ALL.into_iter().filter(|chip| features.contains(&chip.name())).collect();
    match named.as_slice() {
        [chip] => Ok(*chip),
        [] => bail!("--features names no chip; add esp32c5 or esp32c6"),
        _ => bail!("--features names both esp32c5 and esp32c6; a node is built for one"),
    }
}

/// Every board known to be the bridge.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Spare {
    /// `--bridge`, as typed and as matched.
    named: Option<(String, BridgeSpec)>,
    /// The address `run` last found a bridge at.
    remembered: Option<Mac>,
}

impl Spare {
    fn specs(&self) -> impl Iterator<Item = BridgeSpec> + '_ {
        let named = self.named.iter().map(|(_, spec)| spec.clone());
        named.chain(self.remembered.map(BridgeSpec::Mac))
    }
}

/// Which boards the bridge could be, refusing when none is known and `--no-bridge` is absent.
fn bridge_to_spare(named: Option<&str>, remembered: Option<Mac>, no_bridge: bool) -> Result<Spare> {
    let spare = Spare { named: named.map(|text| (text.to_owned(), spec(text))), remembered };
    if spare.named.is_some() || spare.remembered.is_some() || no_bridge {
        return Ok(spare);
    }
    bail!(
        "no bridge is known, so any board attached could be it. Name it with --bridge \
         (`wartui ports` lists each board's address), or pass --no-bridge if none is attached"
    )
}

fn matches(spec: &BridgeSpec, candidate: &PortCandidate) -> bool {
    match spec {
        BridgeSpec::Mac(address) => candidate.mac() == Some(*address),
        BridgeSpec::Path(path) => *path == candidate.path || *path == candidate.device,
    }
}

/// Refuse a `--bridge` that names no attached board: sparing it would spare nothing.
fn named_bridge_attached(candidates: &[PortCandidate], spare: &Spare) -> Result<()> {
    let Some((given, spec)) = &spare.named else { return Ok(()) };
    ensure!(
        candidates.iter().any(|candidate| matches(spec, candidate)),
        "--bridge {given} matches no attached board, so the bridge could not be spared. \
         `wartui ports` lists each board with its address; name the bridge by its address"
    );
    Ok(())
}

/// A path named as the bridge, as the device node it resolves to, so a `by-id` link and the
/// node it points at are recognised as one board.
fn canonical(spec: BridgeSpec) -> BridgeSpec {
    match spec {
        BridgeSpec::Path(path) => match std::fs::canonicalize(&path) {
            Ok(device) => BridgeSpec::Path(device.to_string_lossy().into_owned()),
            Err(_) => BridgeSpec::Path(path),
        },
        other => other,
    }
}

/// What to do with one attached board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Probe(PortCandidate),
    Skip(PortCandidate, Skip),
}

/// Why a board was left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Skip {
    Bridge,
    NotSerialJtag(Option<u16>),
    NoAddress,
    Operator,
    ProbeFailed(String),
    Unreadable,
    OtherChip(String),
    OtherAddress(Mac),
}

impl fmt::Display for Skip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bridge => f.write_str("the bridge"),
            Self::NotSerialJtag(Some(pid)) => {
                write!(f, "not native USB Serial/JTAG (product {pid:04x})")
            }
            Self::NotSerialJtag(None) => f.write_str("reports no product ID"),
            Self::NoAddress => f.write_str("reports no address"),
            Self::Operator => f.write_str("named with --skip"),
            Self::ProbeFailed(line) => write!(f, "espflash board-info failed: {line}"),
            Self::Unreadable => f.write_str("espflash board-info said nothing readable"),
            Self::OtherChip(chip) => write!(f, "an {chip}"),
            Self::OtherAddress(read) => write!(f, "espflash read {} from it", mac(read)),
        }
    }
}

/// Which attached boards are worth probing. Reads nothing and opens nothing.
fn select(candidates: &[PortCandidate], bridge: &Spare, skips: &[Mac]) -> Vec<Verdict> {
    candidates
        .iter()
        .map(|candidate| {
            let is_bridge = bridge.specs().any(|spec| matches(&spec, candidate));
            let skip = if is_bridge {
                Some(Skip::Bridge)
            } else if candidate.pid != Some(BRIDGE_PID) {
                Some(Skip::NotSerialJtag(candidate.pid))
            } else if let Some(address) = candidate.mac() {
                skips.contains(&address).then_some(Skip::Operator)
            } else {
                Some(Skip::NoAddress)
            };
            match skip {
                Some(why) => Verdict::Skip(candidate.clone(), why),
                None => Verdict::Probe(candidate.clone()),
            }
        })
        .collect()
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
}

/// Read `board-info`'s `Chip type:` and `MAC address:` lines. The chip may carry a revision
/// after it, `esp32c5 (revision v0.1)`, which is not the chip's name.
fn parse_board_info(text: &str) -> Option<BoardInfo> {
    let field =
        |label: &str| text.lines().find_map(|line| line.trim().strip_prefix(label).map(str::trim));
    let chip = field("Chip type:")?.split_whitespace().next()?.to_owned();
    let mac = parse_mac(field("MAC address:")?)?;
    Some(BoardInfo { chip, mac })
}

/// Whether a probed board is the one selection judged, and the chip being flashed.
fn judge_probe(expected: Mac, chip: Chip, probe: &Ran) -> Result<(), Skip> {
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
    Ok(())
}

/// What became of one board.
enum Outcome {
    Skipped(Skip),
    WouldFlash,
    Flashed,
    Failed(Ran),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(why) => write!(f, "skipped: {why}"),
            Self::WouldFlash => f.write_str("would flash"),
            Self::Flashed => f.write_str("flashed"),
            Self::Failed(ran) => write!(f, "failed: {}", ran.last_line()),
        }
    }
}

/// A board by its last two octets, as the fleet table names it, or by path without an address.
fn name(candidate: &PortCandidate) -> String {
    match candidate.mac() {
        Some(address) => mac(&address)[12..].to_owned(),
        None => candidate.path.clone(),
    }
}

/// Where the image comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    File(PathBuf),
    Build(PathBuf),
    Release { tag: String, asset: &'static str },
}

impl Source {
    /// Where the image comes from, worded for whether this run will act on it.
    fn describe(&self, dry_run: bool) -> String {
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
fn normalise(path: &Path) -> PathBuf {
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
        return Ok(Source::Build(normalise(firmware_dir.unwrap_or(Path::new(FIRMWARE_DIR)))));
    };
    let asset = release_asset(features).filter(|_| !no_default_features);
    let Some(asset) = asset else {
        bail!(
            "release {tag} publishes no image for --features {}{}; build one with \
             --firmware-dir, or name one with --image",
            features.join(","),
            if no_default_features { " --no-default-features" } else { "" },
        );
    };
    Ok(Source::Release { tag: tag.to_owned(), asset })
}

/// The release asset built with exactly these features, in any order.
fn release_asset(features: &[&str]) -> Option<&'static str> {
    let mut wanted = features.to_vec();
    wanted.sort_unstable();
    wanted.dedup();
    VARIANTS.iter().find_map(|(set, asset)| {
        let mut set = set.to_vec();
        set.sort_unstable();
        (set == wanted).then_some(*asset)
    })
}

/// Get the image onto disk and check it is for `chip`.
fn obtain(
    source: &Source,
    features: &[&str],
    no_default_features: bool,
    chip: Chip,
) -> Result<PathBuf> {
    let path = match source {
        Source::File(path) => path.clone(),
        Source::Build(dir) => build_image(dir, features, no_default_features, chip)?,
        Source::Release { tag, asset } => {
            let cache = state_dir()
                .unwrap_or_else(|| std::env::temp_dir().join("wartui"))
                .join("node-images");
            release_image(RELEASES, tag, asset, &cache, &http_get)?
        }
    };
    check_image(&read(&path)?, chip).with_context(|| format!("checking {}", path.display()))?;
    Ok(path)
}

fn read(path: &Path) -> Result<Vec<u8>> {
    std::fs::read(path).with_context(|| format!("reading {}", path.display()))
}

/// Refuse an image whose bootloader is not built for `chip`.
fn check_image(image: &[u8], chip: Chip) -> Result<()> {
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
    ensure!(
        at == chip.boot_offset(),
        "the image's bootloader is at {at:#x}, and an {} boots from {:#x}",
        chip.name(),
        chip.boot_offset()
    );
    Ok(())
}

/// Build the node firmware in `dir` and merge it into one image beside the ELF.
fn build_image(
    dir: &Path,
    features: &[&str],
    no_default_features: bool,
    chip: Chip,
) -> Result<PathBuf> {
    let mut cargo = Command::new("cargo");
    cargo.current_dir(dir).args(["build", "--release", "--features", &features.join(",")]);
    if no_default_features {
        cargo.arg("--no-default-features");
    }
    // The firmware directory's `rust-toolchain.toml` and `.cargo/config.toml` decide the
    // toolchain and the target, and these would override them — the first is set by rustup for
    // anything run under `cargo run`, and the second moves the ELF from where it is looked for.
    cargo.env_remove("RUSTUP_TOOLCHAIN").env_remove("CARGO_TARGET_DIR");
    let status = cargo.status().context("running cargo")?;
    ensure!(status.success(), "building the node firmware in {} failed", dir.display());

    let elf = dir.join(FIRMWARE_ELF);
    let bin = elf.with_extension("bin");
    let (elf_text, bin_text) = (elf.to_string_lossy(), bin.to_string_lossy());
    let ran = espflash(&[
        "save-image",
        "--skip-update-check",
        "--chip",
        chip.name(),
        "--merge",
        "--skip-padding",
        &elf_text,
        &bin_text,
    ]);
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

/// `work` over every item, at most `jobs` at once, with the results in the items' order.
fn in_parallel<T: Sync, R: Send>(
    items: &[T],
    jobs: usize,
    work: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..jobs.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(index) else { break };
                    let result = work(item);
                    results.lock().expect("no worker panics holding it")[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("no worker panics holding it")
        .into_iter()
        .map(|result| result.expect("every item ran"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::AtomicUsize;

    use wartui_bridge::ports::{ESPRESSIF_VID, candidate};

    use super::*;

    const BRIDGE: Mac = [0x10, 0xBD, 0xA3, 0xEC, 0x44, 0xC0];
    const NODE: Mac = [0x3C, 0xDC, 0x75, 0x84, 0xA1, 0xB0];

    /// The first 24 bytes `espflash save-image --merge` wrote for each chip, from one ELF.
    const C6_HEADER: [u8; 24] = [
        0xe9, 0x03, 0x02, 0x20, 0x1a, 0xb9, 0x86, 0x40, 0xee, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00,
        0x00, 0x00, 0x63, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];
    const C5_HEADER: [u8; 24] = [
        0xe9, 0x03, 0x02, 0x20, 0xaa, 0xbb, 0x84, 0x40, 0xee, 0x00, 0x00, 0x00, 0x17, 0x00, 0x00,
        0x64, 0x00, 0xc7, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01,
    ];

    /// `espflash board-info` 4.6 against a C5, laid out as `print_board_info` writes it.
    const BOARD_INFO: &str = "\
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

    fn board(device: &str, address: Option<&Mac>) -> PortCandidate {
        let serial = address.map(mac);
        candidate(device, Some(ESPRESSIF_VID), Some(BRIDGE_PID), serial.as_deref())
    }

    fn c5_image() -> Vec<u8> {
        let mut image = vec![0xFF; 0x2000];
        image.extend_from_slice(&C5_HEADER);
        image.extend_from_slice(&[0x13; 64]);
        image
    }

    fn c6_image() -> Vec<u8> {
        let mut image = C6_HEADER.to_vec();
        image.extend_from_slice(&[0x13; 64]);
        image
    }

    fn remembered(address: Mac) -> Spare {
        Spare { named: None, remembered: Some(address) }
    }

    fn ran(ok: bool, stdout: &str) -> Ran {
        Ran { ok, stdout: stdout.to_owned(), stderr: String::new() }
    }

    #[test]
    fn flash_fleet_skips_bridge_when_remembered_mac_matches() {
        let found = [board("/dev/ttyACM0", Some(&BRIDGE)), board("/dev/ttyACM1", Some(&NODE))];
        let verdicts = select(&found, &remembered(BRIDGE), &[]);
        assert_eq!(verdicts[0], Verdict::Skip(found[0].clone(), Skip::Bridge));
        assert_eq!(verdicts[1], Verdict::Probe(found[1].clone()));
    }

    #[test]
    fn flash_fleet_skips_bridge_when_named_by_path_or_device() {
        let mut bridge = board("/dev/ttyACM0", Some(&BRIDGE));
        bridge.path = "/dev/serial/by-id/usb-Espressif_10:BD:A3:EC:44:C0-if00".to_owned();
        let found = [bridge.clone()];
        for named in [bridge.path.as_str(), "/dev/ttyACM0"] {
            let verdicts = select(&found, &bridge_to_spare(Some(named), None, false).unwrap(), &[]);
            assert_eq!(verdicts, [Verdict::Skip(bridge.clone(), Skip::Bridge)], "{named}");
        }
    }

    #[test]
    fn flash_fleet_skips_board_when_no_mac_is_reported() {
        let found = [board("/dev/ttyACM0", None)];
        let verdicts = select(&found, &Spare::default(), &[]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::NoAddress)]);
    }

    #[test]
    fn flash_fleet_skips_board_when_named_with_skip() {
        let found = [board("/dev/ttyACM1", Some(&NODE))];
        let verdicts = select(&found, &Spare::default(), &[NODE]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::Operator)]);
    }

    #[test]
    fn flash_fleet_skips_board_when_product_is_not_serial_jtag() {
        let found = [candidate("/dev/ttyACM1", Some(ESPRESSIF_VID), Some(0x0002), Some("x"))];
        let verdicts = select(&found, &Spare::default(), &[]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::NotSerialJtag(Some(2)))]);
    }

    #[test]
    fn flash_fleet_refuses_run_when_no_bridge_is_known() {
        let error = bridge_to_spare(None, None, false).unwrap_err().to_string();
        assert!(error.contains("--no-bridge"), "{error}");
    }

    #[test]
    fn flash_fleet_spares_remembered_bridge_when_bridge_names_a_path() {
        // A path that matches nothing must not leave the remembered bridge exposed.
        let spare = bridge_to_spare(Some("/dev/ttyACM3"), Some(BRIDGE), false).unwrap();
        let found = [board("/dev/ttyACM0", Some(&BRIDGE)), board("/dev/ttyACM1", Some(&NODE))];
        let verdicts = select(&found, &spare, &[]);
        assert_eq!(verdicts[0], Verdict::Skip(found[0].clone(), Skip::Bridge));
        assert_eq!(verdicts[1], Verdict::Probe(found[1].clone()));
    }

    #[test]
    fn flash_fleet_spares_both_when_named_and_remembered_bridges_differ() {
        let spare = bridge_to_spare(Some("/dev/ttyACM1"), Some(BRIDGE), false).unwrap();
        let found = [board("/dev/ttyACM0", Some(&BRIDGE)), board("/dev/ttyACM1", Some(&NODE))];
        let verdicts = select(&found, &spare, &[]);
        assert!(verdicts.iter().all(|v| matches!(v, Verdict::Skip(_, Skip::Bridge))));
    }

    #[test]
    fn flash_fleet_refuses_run_when_bridge_path_matches_no_candidate() {
        let spare = bridge_to_spare(Some("/dev/tty.usbmodem2101"), Some(BRIDGE), false).unwrap();
        let found = [board("/dev/cu.usbmodem2101", Some(&BRIDGE))];
        let error = named_bridge_attached(&found, &spare).unwrap_err().to_string();
        assert!(error.contains("--bridge /dev/tty.usbmodem2101"), "{error}");
        assert!(error.contains("wartui ports"), "{error}");
    }

    #[test]
    fn flash_fleet_refuses_run_when_bridge_mac_matches_no_candidate() {
        let spare = bridge_to_spare(Some("10:BD:A3:EC:44:C0"), None, false).unwrap();
        let found = [board("/dev/ttyACM1", Some(&NODE))];
        let error = named_bridge_attached(&found, &spare).unwrap_err().to_string();
        assert!(error.contains("--bridge 10:BD:A3:EC:44:C0"), "{error}");
    }

    #[test]
    fn flash_fleet_accepts_run_when_named_bridge_is_attached() {
        let found = [board("/dev/ttyACM0", Some(&BRIDGE))];
        for named in ["/dev/ttyACM0", "10:bd:a3:ec:44:c0"] {
            let spare = bridge_to_spare(Some(named), None, false).unwrap();
            named_bridge_attached(&found, &spare).unwrap();
        }
    }

    #[test]
    fn flash_fleet_accepts_run_when_remembered_bridge_is_not_attached() {
        // An unplugged bridge is not a mistake; only a named one that is absent is.
        let found = [board("/dev/ttyACM1", Some(&NODE))];
        named_bridge_attached(&found, &remembered(BRIDGE)).unwrap();
        let verdicts = select(&found, &remembered(BRIDGE), &[]);
        assert_eq!(verdicts, [Verdict::Probe(found[0].clone())]);
    }

    #[test]
    fn flash_fleet_spares_nothing_when_no_bridge_is_passed_and_none_is_remembered() {
        assert_eq!(bridge_to_spare(None, None, true).unwrap(), Spare::default());
    }

    #[test]
    fn flash_fleet_spares_remembered_bridge_when_no_bridge_is_given() {
        // `--no-bridge` lifts the refusal; it never unprotects a board known to be the bridge.
        assert_eq!(bridge_to_spare(None, Some(BRIDGE), true).unwrap(), remembered(BRIDGE));
    }

    #[test]
    fn flash_fleet_refuses_features_when_no_chip_or_both_chips_are_named() {
        assert!(chip_of(&features("xiao-external-antenna")).is_err());
        assert!(chip_of(&features("")).is_err());
        assert!(chip_of(&features("esp32c5,esp32c6")).is_err());
    }

    #[test]
    fn flash_fleet_resolves_chip_when_features_carry_extras() {
        assert_eq!(chip_of(&features("esp32c5,xiao-external-antenna")).unwrap(), Chip::Esp32c5);
        assert_eq!(chip_of(&features(" esp32c6 , log")).unwrap(), Chip::Esp32c6);
    }

    #[test]
    fn flash_fleet_parses_board_info_when_given_espflash_transcript() {
        let info = parse_board_info(BOARD_INFO).unwrap();
        assert_eq!(info, BoardInfo { chip: "esp32c5".to_owned(), mac: NODE });
    }

    #[test]
    fn flash_fleet_returns_none_when_board_info_is_garbage() {
        assert_eq!(parse_board_info("Error: espflash::connection_failed\n  x timed out"), None);
        assert_eq!(parse_board_info("Chip type:         esp32c5 (revision v0.1)"), None);
        assert_eq!(parse_board_info("MAC address:       3c:dc:75:84:a1:b0"), None);
    }

    #[test]
    fn flash_fleet_accepts_probe_when_chip_and_mac_match() {
        assert_eq!(judge_probe(NODE, Chip::Esp32c5, &ran(true, BOARD_INFO)), Ok(()));
    }

    #[test]
    fn flash_fleet_skips_probed_board_when_chip_differs() {
        let judged = judge_probe(NODE, Chip::Esp32c6, &ran(true, BOARD_INFO));
        assert_eq!(judged, Err(Skip::OtherChip("esp32c5".to_owned())));
    }

    #[test]
    fn flash_fleet_skips_probed_board_when_mac_differs() {
        let judged = judge_probe(BRIDGE, Chip::Esp32c5, &ran(true, BOARD_INFO));
        assert_eq!(judged, Err(Skip::OtherAddress(NODE)));
    }

    #[test]
    fn flash_fleet_skips_probed_board_when_espflash_fails() {
        let failed = Ran { ok: false, stdout: String::new(), stderr: "\n  x timed out\n".into() };
        let judged = judge_probe(NODE, Chip::Esp32c5, &failed);
        assert_eq!(judged, Err(Skip::ProbeFailed("x timed out".to_owned())));
        assert_eq!(judge_probe(NODE, Chip::Esp32c5, &ran(true, "")), Err(Skip::Unreadable));
    }

    #[test]
    fn flash_fleet_pins_chip_ids_when_reading_image_headers() {
        // IDF's `esp_chip_id_t`, which espflash's `Chip::id` also carries.
        assert_eq!(Chip::Esp32c6.image_id(), 13);
        assert_eq!(Chip::Esp32c5.image_id(), 23);
    }

    #[test]
    fn flash_fleet_accepts_image_when_header_names_the_chip() {
        check_image(&c5_image(), Chip::Esp32c5).unwrap();
        check_image(&c6_image(), Chip::Esp32c6).unwrap();
    }

    #[test]
    fn flash_fleet_refuses_image_when_header_names_another_chip() {
        let error = check_image(&c6_image(), Chip::Esp32c5).unwrap_err().to_string();
        assert!(error.contains("built for esp32c6"), "{error}");
        let error = check_image(&c5_image(), Chip::Esp32c6).unwrap_err().to_string();
        assert!(error.contains("built for esp32c5"), "{error}");
    }

    #[test]
    fn flash_fleet_refuses_image_when_bootloader_is_at_the_wrong_offset() {
        // A C5 header at 0x0, where a C5 never boots from.
        let mut image = C5_HEADER.to_vec();
        image.extend_from_slice(&[0x13; 64]);
        assert!(check_image(&image, Chip::Esp32c5).is_err());
    }

    #[test]
    fn flash_fleet_refuses_image_when_file_is_short() {
        assert!(check_image(&C5_HEADER[..16], Chip::Esp32c5).is_err());
        assert!(check_image(&[], Chip::Esp32c5).is_err());
        assert!(check_image(&c5_image()[..0x2000 + 8], Chip::Esp32c5).is_err());
    }

    #[test]
    fn flash_fleet_refuses_image_when_magic_is_missing() {
        let mut image = c6_image();
        image[0] = 0x7F;
        assert!(check_image(&image, Chip::Esp32c6).is_err());
    }

    #[test]
    fn flash_fleet_finds_release_asset_when_features_are_in_any_order() {
        let asset = release_asset(&["xiao-external-antenna", "esp32c6"]);
        assert_eq!(asset, Some("wartui-node-fw-esp32c6-xiao.bin"));
        assert_eq!(release_asset(&["esp32c5"]), Some("wartui-node-fw-esp32c5.bin"));
        assert_eq!(release_asset(&["esp32c6"]), Some("wartui-node-fw-esp32c6.bin"));
    }

    #[test]
    fn flash_fleet_refuses_release_asset_when_features_are_outside_table() {
        assert_eq!(release_asset(&["esp32c5", "xiao-external-antenna"]), None);
        assert_eq!(release_asset(&["esp32c6", "log"]), None);
    }

    #[test]
    fn flash_fleet_chooses_source_when_given_each_combination() {
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        let c5 = ["esp32c5"];
        let found = source(Some(image), None, &c5, false, Some("v1")).unwrap();
        assert_eq!(found, Source::File(image.to_owned()));
        let found = source(None, Some(dir), &c5, false, Some("v1")).unwrap();
        assert_eq!(found, Source::Build(dir.to_owned()));
        let found = source(None, None, &c5, false, None).unwrap();
        assert_eq!(found, Source::Build(normalise(Path::new(FIRMWARE_DIR))));
        let found = source(None, None, &c5, false, Some("")).unwrap();
        assert_eq!(found, Source::Build(normalise(Path::new(FIRMWARE_DIR))));
        let found = source(None, None, &c5, false, Some("v1")).unwrap();
        let asset = "wartui-node-fw-esp32c5.bin";
        assert_eq!(found, Source::Release { tag: "v1".to_owned(), asset });
    }

    #[test]
    fn flash_fleet_normalises_firmware_dir_when_path_has_parent_segments() {
        assert_eq!(
            normalise(Path::new("/a/b/crates/wartui/../../firmware/node")),
            Path::new("/a/b/firmware/node")
        );
        assert_eq!(normalise(Path::new("./fw/./node/")), Path::new("fw/node"));
        assert_eq!(normalise(Path::new("../fw/x/..")), Path::new("../fw"));
        assert_eq!(normalise(Path::new("/../fw")), Path::new("/fw"));
        let dir = source(None, Some(Path::new("/x/y/../fw")), &["esp32c5"], false, None).unwrap();
        assert_eq!(dir, Source::Build("/x/fw".into()));
    }

    #[test]
    fn flash_fleet_shows_default_firmware_dir_without_parent_segments_when_building() {
        let Source::Build(dir) = source(None, None, &["esp32c5"], false, None).unwrap() else {
            panic!("not a build")
        };
        assert!(dir.ends_with("firmware/node"), "{}", dir.display());
        assert!(
            !dir.components().any(|c| c == std::path::Component::ParentDir),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn flash_fleet_words_source_as_future_when_dry_run() {
        let build = Source::Build("/fw".into());
        assert_eq!(build.describe(true), "would build from /fw");
        let release = Source::Release { tag: "v1".to_owned(), asset: "x.bin" };
        assert_eq!(release.describe(true), "would fetch x.bin from release v1");
        assert_eq!(Source::File("fw.bin".into()).describe(true), "fw.bin");
    }

    #[test]
    fn flash_fleet_words_source_as_present_when_real_run() {
        assert_eq!(Source::Build("/fw".into()).describe(false), "building from /fw");
        let release = Source::Release { tag: "v1".to_owned(), asset: "x.bin" };
        assert_eq!(release.describe(false), "x.bin from release v1");
        assert_eq!(Source::File("fw.bin".into()).describe(false), "fw.bin");
    }

    #[test]
    fn flash_fleet_refuses_release_source_when_default_features_are_off() {
        let error = source(None, None, &["esp32c5"], true, Some("v1")).unwrap_err().to_string();
        assert!(error.contains("--firmware-dir") && error.contains("--image"), "{error}");
    }

    #[test]
    fn flash_fleet_builds_download_url_when_given_tag_and_asset() {
        let url = asset_url(RELEASES, "v0.2.0", "wartui-node-fw-esp32c5.bin");
        assert_eq!(
            url,
            "https://github.com/wowy/wartui/releases/download/v0.2.0/wartui-node-fw-esp32c5.bin"
        );
    }

    #[test]
    fn flash_fleet_verifies_digest_when_sums_list_the_asset() {
        let sums = format!("{}  a.bin\n{} *b.bin\n", sha256_hex(b"a"), sha256_hex(b"b"));
        verify(b"a", &sums, "a.bin").unwrap();
        verify(b"b", &sums, "b.bin").unwrap();
    }

    #[test]
    fn flash_fleet_rejects_download_when_digest_mismatches() {
        let sums = format!("{}  a.bin\n", sha256_hex(b"a"));
        assert!(verify(b"tampered", &sums, "a.bin").is_err());
        assert!(verify(b"a", &sums, "missing.bin").is_err());
    }

    #[test]
    fn flash_fleet_fetches_and_caches_image_when_cache_is_empty() {
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
    fn flash_fleet_uses_cached_image_when_offline() {
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
    fn flash_fleet_rejects_download_when_fetched_image_mismatches_sums() {
        let cache = tempfile::tempdir().unwrap();
        let sums = format!("{}  x.bin\n", sha256_hex(b"the real one"));
        let fetch = |url: &str| -> Result<Vec<u8>> {
            Ok(if url.ends_with(SUMS) { sums.clone().into_bytes() } else { b"another".to_vec() })
        };
        assert!(release_image("http://test", "v1", "x.bin", cache.path(), &fetch).is_err());
        assert!(!cache.path().join("v1/x.bin").exists(), "a rejected image is not cached");
    }

    #[test]
    fn flash_fleet_keeps_order_and_bounds_workers_when_running_in_parallel() {
        let running = AtomicUsize::new(0);
        let most = AtomicUsize::new(0);
        let items: Vec<usize> = (0..12).collect();
        let doubled = in_parallel(&items, 3, |item| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            most.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(5));
            running.fetch_sub(1, Ordering::SeqCst);
            item * 2
        });
        assert_eq!(doubled, items.iter().map(|i| i * 2).collect::<Vec<_>>());
        assert!(most.load(Ordering::SeqCst) <= 3);
    }
}
