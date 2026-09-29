//! `wartui flash-bridge` — bridge firmware onto exactly one board, the bridge.
//!
//! **The board is chosen before anything is reset, and only when the choice is certain.** A
//! bridge image written over a node takes that node out of the fleet, and nothing on the bus tells
//! the two apart but an address. So the target is the board `--bridge` names, which must be
//! attached; else the remembered bridge, which must be attached; else, with none remembered, the
//! only Espressif board there is. A remembered bridge that is not attached is a refusal even when
//! one board is: that board is known not to be the bridge, and flashing it would also overwrite
//! the memory `flash-fleet` spares the real bridge by. Several boards and none known is a refusal
//! that lists them, never a guess. A board with no address is refused too, since the identity
//! cross-check (`super`) needs one; with a single target, a failed cross-check is an error rather
//! than a skip.
//!
//! **A flashed board is remembered as the bridge.** It now runs bridge firmware, so `flash-fleet`
//! spares it and `run` opens it first, without either being told.
//!
//! There is no `--skip`, `--jobs` or `--no-bridge`: there is one target. There is no
//! `--no-default-features` either, because the bridge firmware has no default features.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{BRIDGE_PID, PortCandidate};
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{BridgeSpec, discover_ports};
use wartui_proto::link::Mac;

use super::{
    BRIDGE, RELEASE_TAG, Skip, Source, canonical, check_image, chip_of, espflash, espflash_present,
    features, flash_args, judge_probe, matches, name, obtain, probe_args, read, source,
};
use crate::{mac, spec};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The bridge firmware's features, comma-separated, as `cargo build` takes them. Exactly one
    /// of `esp32c5` or `esp32c6`, which the board must be.
    #[arg(long, value_name = "LIST", required = true)]
    pub features: String,

    /// Which board to flash, by path or by address. It must be attached. Without it, the bridge
    /// `run` last found, which must be attached; else, with none remembered, the only Espressif
    /// board attached.
    #[arg(long, value_name = "PATH|MAC")]
    pub bridge: Option<String>,

    /// Flash this merged image, which holds flash from address 0, rather than building or
    /// downloading one.
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

pub fn run(args: Args) -> Result<()> {
    let features = features(&args.features);
    let chip = chip_of(&BRIDGE, &features)?;
    espflash_present()?;
    let memory = BridgeMemory::discover();
    let named = args.bridge.as_deref().map(|given| (given, canonical(spec(given))));
    let found = discover_ports().context("listing serial ports")?;
    let board =
        target(&found, named.as_ref().map(|(given, spec)| (*given, spec)), memory.recall())?;
    let expected = board.mac().expect("target refuses a board without an address");

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
            check_image(&read(path)?, chip)?;
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
    memory.remember(expected);
    println!("{:<6} flashed", name(&board));
    Ok(())
}

/// The one board to flash. Reads nothing and opens nothing.
///
/// `named` is `--bridge` as typed and as matched; `remembered` is the address `run` last found a
/// bridge at.
fn target(
    candidates: &[PortCandidate],
    named: Option<(&str, &BridgeSpec)>,
    remembered: Option<Mac>,
) -> Result<PortCandidate> {
    let chosen = match named {
        Some((given, spec)) => {
            candidates.iter().find(|candidate| matches(spec, candidate)).ok_or_else(|| {
                anyhow!(
                    "--bridge {given} matches no attached board. `wartui ports` lists each board \
                     with its address; name the bridge by its address"
                )
            })?
        }
        None => match (remembered, candidates) {
            (Some(wanted), _) => {
                candidates.iter().find(|board| board.mac() == Some(wanted)).ok_or_else(|| {
                    anyhow!(
                        "the remembered bridge {} is not attached. Name the board to flash with \
                         --bridge (`wartui ports` lists each board's address)",
                        mac(&wanted)
                    )
                })?
            }
            (None, [only]) => only,
            (None, []) => bail!("no Espressif board is attached"),
            (None, several) => bail!(
                "{} boards are attached and none is known to be the bridge: {}. Name it with \
                     --bridge (`wartui ports` lists each board's address)",
                several.len(),
                several
                    .iter()
                    .map(|board| board.mac().map_or_else(|| board.path.clone(), |a| mac(&a)))
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        },
    };
    if chosen.pid != Some(BRIDGE_PID) {
        bail!("{} is {}", chosen.path, Skip::NotSerialJtag(chosen.pid));
    }
    if chosen.mac().is_none() {
        bail!("{} {}, so espflash's reading of it cannot be checked", chosen.path, Skip::NoAddress);
    }
    Ok(chosen.clone())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wartui_bridge::ports::{ESPRESSIF_VID, candidate};

    use super::super::testing::{BRIDGE_MAC, NODE_MAC, board};
    use super::super::{normalise, release_asset};
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
        assert!(error.contains(&mac(&NODE_MAC)), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_several_boards_attached_and_none_known() {
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains(&mac(&BRIDGE_MAC)) && error.contains(&mac(&NODE_MAC)), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_no_board_is_attached() {
        let error = target(&[], None, None).unwrap_err().to_string();
        assert!(error.contains("no Espressif board is attached"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_board_when_it_reports_no_address() {
        let found = [board("/dev/ttyACM0", None)];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains("reports no address"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_board_when_product_is_not_serial_jtag() {
        let found = [candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(0x0002), Some("x"))];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains("not native USB Serial/JTAG"), "{error}");
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
        assert_eq!(found, Source::Build(normalise(Path::new(BRIDGE.dir))));
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
