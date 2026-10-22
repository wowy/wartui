//! `wartui flash-bridge` — bridge firmware onto exactly one board, the bridge.
//!
//! **The board is chosen before anything is reset, and only when the choice is certain.** A
//! bridge image written over a node takes that node out of the fleet, and nothing on the bus tells
//! the two apart but an address. So the target is the board `--bridge` names, which must be
//! attached; else the bridge remembered in `wartui.toml`, which must be attached; else, with none
//! remembered, the only Espressif board there is. A remembered bridge that is not attached is a
//! refusal even when one board is: that board is known not to be the bridge. Several boards and
//! none known is a refusal
//! that lists them, never a guess. A board with no address is refused too, since the identity
//! cross-check (`super`) needs one; with a single target, a failed cross-check is an error rather
//! than a skip.
//!
//! **A board found here is remembered once flashed.** `flash-bridge` saves its address to
//! `wartui.toml` as `[bridge] mac`, the way `run` does once it connects, so a `flash-fleet` right
//! after spares it. Otherwise the old address is still remembered and not attached, which
//! `flash-fleet` reads as an unplugged bridge, and the new one would get node firmware. Nothing
//! is saved with remembering off, or for a board named with `--bridge`: one flashed by name is
//! often some other device, and must not disturb which bridge the main fleet's `run` and
//! `flash-fleet` use. A save that fails leaves the flash a success, and says so.
//!
//! **A P4 bridge is updated over its own link** (`super::p4`), not flashed with espflash: its
//! Type-C reaches a CH343 rather than the chip's USB, and the cable the host already holds is
//! the one that carries the update. The board is chosen by the same rules.
//!
//! There is no `--skip`, `--jobs` or `--no-bridge`: there is one target. There is no
//! `--no-default-features` either: the single-chip bridge firmware has no default features, and
//! a P4 release image is built with its own.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{BRIDGE_PID, PortCandidate, is_p4_bridge};
use wartui_bridge::serial::{BridgeSpec, discover_ports};
use wartui_proto::mac::{self, Mac};

use super::{
    BRIDGE, RELEASE_TAG, Skip, Source, canonical, check_image, chip_of, espflash, espflash_present,
    features, flash_args, judge_probe, matches, name, obtain, p4, probe_args, read, source,
};
use crate::{config, spec};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The bridge firmware's features, comma-separated, as `cargo build` takes them. For a
    /// single-chip bridge, exactly one of `esp32c5` or `esp32c6`, which the board must be. For a
    /// P4 bridge, none for its release image, or its own features for a build.
    #[arg(long, value_name = "LIST")]
    pub features: Option<String>,

    /// Which board to flash, by path, by address or by the end of it (`00:08`), which must match
    /// one board. It must be attached, and is not remembered. Without it, the bridge remembered
    /// in `wartui.toml`, which must be attached; else, with none remembered, the only Espressif
    /// board attached. A board found without it is remembered as the bridge once flashed.
    #[arg(long, value_name = "PATH|MAC")]
    pub bridge: Option<String>,

    /// Read and save the remembered bridge in this file instead of the default `wartui.toml`. A
    /// file that does not load refuses the run; see `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Flash this image rather than building or downloading one: a merged image, which holds
    /// flash from address 0, for a single-chip bridge, or an app-only image for a P4 bridge.
    #[arg(long, value_name = "BIN", conflicts_with = "firmware_dir")]
    pub image: Option<PathBuf>,

    /// Build the image from the bridge firmware in this directory.
    #[arg(long, value_name = "DIR")]
    pub firmware_dir: Option<PathBuf>,

    /// Say where the image would come from and which board would be flashed, then stop. Builds,
    /// downloads and flashes nothing.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let features = features(args.features.as_deref().unwrap_or_default());
    let named = args.bridge.as_deref().map(|given| (given, canonical(spec(given))));
    let found = discover_ports().context("listing serial ports")?;
    let board = target(
        &found,
        named.as_ref().map(|(given, spec)| (*given, spec)),
        super::remembered_bridge(args.config.as_deref())?,
    )?;
    let expected = board.mac().expect("target refuses a board without an address");
    if is_p4_bridge(&board) {
        p4::run(&args, &features, &board).await?;
        if !args.dry_run {
            remember(&args, named.is_some(), &board, expected);
        }
        return Ok(());
    }
    let chip = chip_of(&BRIDGE, &features)?;
    espflash_present()?;

    let source = source(
        &BRIDGE,
        args.image.as_deref(),
        args.firmware_dir.as_deref(),
        &features,
        false,
        RELEASE_TAG,
    )?;
    println!("image  {}", source.describe(args.dry_run));
    let image = if args.dry_run {
        // A file named on the command line costs nothing to read, and a wrong one is worth
        // hearing about before the real run.
        if let Source::File(path) = &source {
            check_image(&read(path)?, chip, BRIDGE.merged)?;
        }
        None
    } else {
        Some(obtain(&BRIDGE, &source, &features, false, chip)?)
    };

    if let Err(why) = judge_probe(expected, chip, &espflash(&probe_args(&board.path))) {
        bail!("{} on {} is not flashed: {why}", name(&board), board.path);
    }
    let Some(image) = image else {
        println!("{:<6} would flash", name(&board));
        return Ok(());
    };
    let ran = espflash(&flash_args(&board.path, chip, &image));
    if !ran.ok {
        println!("{:<6} failed: {}", name(&board), ran.last_line());
        for line in ran.stdout.lines().chain(ran.stderr.lines()) {
            println!("         {line}");
        }
        bail!("the bridge failed to flash");
    }
    println!("{:<6} flashed", name(&board));
    remember(&args, named.is_some(), &board, expected);
    Ok(())
}

/// Remember the flashed board as the bridge, unless `--bridge` named it, saying so when that
/// fails.
fn remember(args: &Args, named: bool, board: &PortCandidate, mac: Mac) {
    if named {
        return;
    }
    let saved = config::path(args.config.as_deref())
        .ok_or_else(|| anyhow!("there is no default location for it; use --config"))
        .and_then(|path| remember_flashed(&path, mac));
    if let Err(why) = saved {
        println!(
            "{:<6} not remembered as the bridge, so flash-fleet will not spare it: {why:#}",
            name(board)
        );
    }
}

/// Save `mac` as the bridge in the `wartui.toml` at `path`, keeping every other key. A missing
/// file is created. Writes nothing when the file says `remember = false`.
fn remember_flashed(path: &Path, mac: Mac) -> Result<()> {
    let mut config = config::load_from(None, || Some(path.to_path_buf()))?;
    if config.bridge.remember == Some(false) {
        return Ok(());
    }
    config.bridge.mac = Some(mac);
    config::save(path, &config)
}

/// The one board to flash. Reads nothing and opens nothing.
///
/// `named` is `--bridge` as typed and as matched; `remembered` is the bridge in `wartui.toml`.
fn target(
    candidates: &[PortCandidate],
    named: Option<(&str, &BridgeSpec)>,
    remembered: Option<Mac>,
) -> Result<PortCandidate> {
    let chosen = match named {
        Some((given, spec)) => {
            match candidates.iter().filter(|candidate| matches(spec, candidate)).collect::<Vec<_>>()
            {
                named if named.len() > 1 => bail!(
                    "--bridge {given} matches {} attached boards: {}. Name the bridge by more of \
                     its address",
                    named.len(),
                    named.iter().map(|board| board.label()).collect::<Vec<_>>().join(", ")
                ),
                named => named.first().copied().ok_or_else(|| {
                    anyhow!(
                        "--bridge {given} matches no attached board. `wartui ports` lists each \
                         board with its address; name the bridge by its address"
                    )
                })?,
            }
        }
        None => match (remembered, candidates) {
            (Some(wanted), _) => {
                candidates.iter().find(|board| board.mac() == Some(wanted)).ok_or_else(|| {
                    anyhow!(
                        "the remembered bridge {} is not attached. Name the board to flash with \
                         --bridge (`wartui ports` lists each board's address)",
                        mac::full(&wanted)
                    )
                })?
            }
            (None, [only]) => only,
            (None, []) => bail!("no Espressif board or P4 bridge is attached"),
            (None, several) => bail!(
                "{} boards are attached and none is known to be the bridge: {}. Name it with \
                     --bridge (`wartui ports` lists each board's address)",
                several.len(),
                several.iter().map(|board| board.label()).collect::<Vec<_>>().join(", "),
            ),
        },
    };
    if !is_p4_bridge(chosen) && chosen.pid != Some(BRIDGE_PID) {
        bail!("{} is {}", chosen.path, Skip::NotSerialJtag(chosen.pid));
    }
    if chosen.mac().is_none() {
        bail!(
            "{} {}, so what the board reports cannot be checked against it",
            chosen.path,
            Skip::NoAddress
        );
    }
    Ok(chosen.clone())
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ports::{ESPRESSIF_VID, TEST_PID, TEST_VID, candidate};

    use super::super::testing::{BRIDGE_MAC, NODE_MAC, board};
    use super::super::{normalize, release_asset};
    use super::*;

    fn named(given: &str) -> (&str, BridgeSpec) {
        (given, spec(given))
    }

    #[test]
    fn flash_bridge_targets_named_board_when_bridge_is_attached() {
        let mut bridge = board("/dev/ttyACM0", Some(&BRIDGE_MAC));
        bridge.path = "/dev/serial/by-id/usb-Espressif_10:BD:A3:EC:44:C0-if00".to_owned();
        let found = [bridge.clone(), board("/dev/ttyACM1", Some(&NODE_MAC))];
        for given in ["10:bd:a3:ec:44:c0", bridge.path.as_str(), "/dev/ttyACM0"] {
            let (given, spec) = named(given);
            assert_eq!(target(&found, Some((given, &spec)), Some(NODE_MAC)).unwrap(), bridge);
        }
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_bridge_matches_no_board() {
        let found = [board("/dev/ttyACM1", Some(&NODE_MAC))];
        let (given, spec) = named("10:BD:A3:EC:44:C0");
        let error = target(&found, Some((given, &spec)), None).unwrap_err().to_string();
        assert!(error.contains("--bridge 10:BD:A3:EC:44:C0"), "{error}");
        assert!(error.contains("wartui ports"), "{error}");
    }

    #[test]
    fn flash_bridge_targets_named_board_when_its_last_octets_match_one_board() {
        let found =
            [board("/dev/ttyACM0", Some(&NODE_MAC)), board("/dev/ttyACM1", Some(&BRIDGE_MAC))];
        let (given, spec) = named("44:c0");
        assert_eq!(target(&found, Some((given, &spec)), None).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_tail_matches_several_boards() {
        // Bridge firmware over a node takes it out of the fleet, so a guess between
        // two boards ending the same way is never made.
        let twin = [0x3C, 0xDC, 0x75, 0x84, 0x44, 0xC0];
        let found = [board("/dev/ttyACM0", Some(&twin)), board("/dev/ttyACM1", Some(&BRIDGE_MAC))];
        let (given, spec) = named("44:C0");
        let error = target(&found, Some((given, &spec)), Some(BRIDGE_MAC)).unwrap_err().to_string();
        assert!(error.contains("--bridge 44:C0 matches 2 attached boards"), "{error}");
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&twin).to_string()),
            "{error}"
        );
    }

    #[test]
    fn flash_bridge_targets_remembered_board_when_several_are_attached() {
        let found =
            [board("/dev/ttyACM0", Some(&NODE_MAC)), board("/dev/ttyACM1", Some(&BRIDGE_MAC))];
        assert_eq!(target(&found, None, Some(BRIDGE_MAC)).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_targets_only_board_when_nothing_is_known() {
        let found = [board("/dev/ttyACM0", Some(&BRIDGE_MAC))];
        assert_eq!(target(&found, None, None).unwrap(), found[0]);
    }

    #[test]
    fn flash_bridge_refuses_run_when_remembered_bridge_is_not_attached() {
        // The one board present is known not to be the bridge.
        let found = [board("/dev/ttyACM0", Some(&BRIDGE_MAC))];
        let error = target(&found, None, Some(NODE_MAC)).unwrap_err().to_string();
        assert!(error.contains(&mac::full(&NODE_MAC).to_string()), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_several_boards_attached_and_none_known() {
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&NODE_MAC).to_string()),
            "{error}"
        );
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_no_board_is_attached() {
        let error = target(&[], None, None).unwrap_err().to_string();
        assert!(error.contains("no Espressif board or P4 bridge is attached"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_board_when_it_reports_no_address() {
        let found = [board("/dev/ttyACM0", None)];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains("reports no address"), "{error}");
    }

    /// A P4 bridge as the OS lists it, its serial number the bridge's address.
    fn p4(device: &str, address: &Mac) -> PortCandidate {
        let serial = mac::full(address).to_string();
        let mut p4 = candidate(device, Some(TEST_VID), Some(TEST_PID), Some(&serial));
        p4.product = Some("wartui p4 bridge".to_owned());
        p4
    }

    #[test]
    fn flash_bridge_targets_p4_bridge_when_named() {
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), p4("/dev/ttyACM1", &BRIDGE_MAC)];
        let (given, spec) = named("44:c0");
        let chosen = target(&found, Some((given, &spec)), Some(NODE_MAC)).unwrap();
        assert_eq!(chosen, found[1]);
        assert!(is_p4_bridge(&chosen));
    }

    #[test]
    fn flash_bridge_targets_p4_bridge_when_remembered() {
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), p4("/dev/ttyACM1", &BRIDGE_MAC)];
        assert_eq!(target(&found, None, Some(BRIDGE_MAC)).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_targets_p4_bridge_when_it_is_the_only_board() {
        let found = [p4("/dev/ttyACM0", &BRIDGE_MAC)];
        assert_eq!(target(&found, None, None).unwrap(), found[0]);
    }

    #[test]
    fn flash_bridge_refuses_p4_bridge_when_it_reports_no_address() {
        let mut bare = p4("/dev/ttyACM0", &BRIDGE_MAC);
        bare.serial = None;
        let error = target(&[bare], None, None).unwrap_err().to_string();
        assert!(error.contains("reports no address"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_board_when_product_is_not_serial_jtag() {
        let found = [candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(0x0002), Some("x"))];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains("not native USB Serial/JTAG"), "{error}");
    }

    #[test]
    fn flash_bridge_remembers_flashed_board_when_file_holds_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        std::fs::write(&path, "pool = \"eu\"\n[bridge]\nmac = \"3C:DC:75:84:A1:B0\"\n").unwrap();

        remember_flashed(&path, BRIDGE_MAC).unwrap();

        let saved = config::load(Some(&path)).unwrap();
        assert_eq!(saved.bridge.mac, Some(BRIDGE_MAC), "the new bridge replaces the old");
        assert_eq!(saved.pool, Some(crate::run::PoolArg::Eu), "the rest is kept");
    }

    #[test]
    fn flash_bridge_writes_nothing_when_remember_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let original = "[bridge]\nremember = false\n";
        std::fs::write(&path, original).unwrap();

        remember_flashed(&path, BRIDGE_MAC).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn flash_bridge_creates_file_when_remembering_and_none_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/wartui.toml");

        remember_flashed(&path, BRIDGE_MAC).unwrap();

        assert_eq!(config::load(Some(&path)).unwrap().bridge.mac, Some(BRIDGE_MAC));
    }

    #[test]
    fn flash_bridge_finds_release_asset_when_features_name_a_published_variant() {
        let asset = |features: &[&str]| release_asset(&BRIDGE, features);
        assert_eq!(asset(&["esp32c5"]), Some("wartui-bridge-fw-esp32c5.bin"));
        let t_dongle = asset(&["t-dongle-c5", "esp32c5"]);
        assert_eq!(t_dongle, Some("wartui-bridge-fw-esp32c5-t-dongle.bin"));
        assert_eq!(asset(&["esp32c6"]), Some("wartui-bridge-fw-esp32c6.bin"));
        let xiao = asset(&["esp32c6", "xiao-external-antenna"]);
        assert_eq!(xiao, Some("wartui-bridge-fw-esp32c6-xiao.bin"));
        assert_eq!(asset(&["esp32c5", "xiao-external-antenna"]), None);
    }

    #[test]
    fn flash_bridge_chooses_source_when_given_each_combination() {
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        let c6 = ["esp32c6"];
        let found = source(&BRIDGE, Some(image), None, &c6, false, Some("v1")).unwrap();
        assert_eq!(found, Source::File(image.to_owned()));
        let found = source(&BRIDGE, None, Some(dir), &c6, false, Some("v1")).unwrap();
        assert_eq!(found, Source::Build(dir.to_owned()));
        let found = source(&BRIDGE, None, None, &c6, false, None).unwrap();
        assert_eq!(found, Source::Build(normalize(Path::new(BRIDGE.dir))));
        let Source::Build(default) = found else { panic!("not a build") };
        assert!(default.ends_with("firmware/bridge"), "{}", default.display());
        let found = source(&BRIDGE, None, None, &c6, false, Some("v1")).unwrap();
        let asset = "wartui-bridge-fw-esp32c6.bin";
        assert_eq!(found, Source::Release { tag: "v1".to_owned(), asset });
        let error =
            source(&BRIDGE, None, None, &["esp32c6", "log"], false, Some("v1")).unwrap_err();
        assert!(error.to_string().contains("no bridge image"), "{error}");
    }
}
