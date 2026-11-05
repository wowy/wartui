//! `wartui flash-bridge` — bridge firmware onto exactly one board, the P4 bridge.
//!
//! **The board is chosen before anything is sent, and only when the choice is certain.** The
//! choice is made among the attached P4 bridges: a node beside one blocks nothing, and no node can
//! be chosen. So the target is the P4 bridge `--bridge` names, which must be attached; else the
//! bridge remembered in `wartui.toml`, which must be attached; else, with none remembered, the only
//! P4 bridge there is. A remembered bridge that is not attached is a refusal even when one P4
//! bridge is: that board is known not to be the bridge. A remembered board that is attached and is
//! not a P4 bridge is refused too, which is why the listing holds every fleet board: the refusal
//! has to say which of the two it is. Several and none known is a refusal that lists them, never a
//! guess. A board with no address is refused too, since the update checks the address it answers
//! with.
//!
//! **No P4 bridge attached points at the first flash.** A blank P4 shows up only as its CH343, which
//! nothing here may write to, so that flash is by hand ([`p4::FIRST_FLASH`]).
//!
//! **A board found here is remembered once flashed.** `flash-bridge` saves its address to
//! `wartui.toml` as `[bridge] mac`, the way `run` does once it connects, so `run` and the next
//! `flash-bridge` open that board first. Nothing is saved with remembering off, or for a board
//! named with `--bridge`: one flashed by name is often some other device, and must not disturb
//! which bridge the main fleet's `run` uses. A save that fails leaves the flash a success, and says
//! so.
//!
//! **A P4 bridge is updated over its own link** (`super::p4`): its Type-C reaches a CH343 rather
//! than the chip's USB, and the cable the host already holds is the one that carries the update.
//!
//! There is no `--skip` or `--jobs`: there is one target. There is no `--no-default-features`
//! either: a release image is built with the default features.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{PortCandidate, could_be_a_bridge};
use wartui_bridge::serial::{BridgeSpec, discover_ports};
use wartui_proto::mac::{self, Mac};

use super::{Skip, canonical, features, matches, name, p4};
use crate::{config, spec};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The P4 bridge firmware's features, comma-separated, as `cargo build` takes them, for a
    /// build from a firmware directory. A release publishes the default features only.
    #[arg(long, value_name = "LIST")]
    pub features: Option<String>,

    /// Which P4 bridge to flash, by path, by address or by the end of it (`71:F4`), which must
    /// match one attached P4 bridge. It is not remembered. Without it, the bridge remembered in
    /// `wartui.toml`, which must be attached; else, with none remembered, the only P4 bridge
    /// attached. A board found without it is remembered as the bridge once flashed.
    #[arg(long, value_name = "PATH|MAC")]
    pub bridge: Option<String>,

    /// Read and save the remembered bridge in this file instead of the default `wartui.toml`. A
    /// file that does not load refuses the run; see `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,

    /// Send this app-only image rather than building or downloading one.
    #[arg(long, value_name = "BIN", conflicts_with = "firmware_dir")]
    pub image: Option<PathBuf>,

    /// Build the image from the P4 bridge firmware in this directory.
    #[arg(long, value_name = "DIR")]
    pub firmware_dir: Option<PathBuf>,

    /// Say where the image would come from and which board would be updated, then stop. Builds,
    /// downloads and sends nothing.
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
    p4::run(&args, &features, &board).await?;
    if !args.dry_run {
        remember(&args, named.is_some(), &board, expected);
    }
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
            "{:<6} not remembered as the bridge, so `run` and the next flash-bridge will not \
             open it first: {why:#}",
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

/// The one board to flash, out of the P4 bridges among `candidates`. Reads nothing and opens
/// nothing.
///
/// `named` is `--bridge` as typed and as matched; `remembered` is the bridge in `wartui.toml`.
fn target(
    candidates: &[PortCandidate],
    named: Option<(&str, &BridgeSpec)>,
    remembered: Option<Mac>,
) -> Result<PortCandidate> {
    // The listing holds the nodes too, so a remembered board can be told from an absent one.
    // Everything chosen below is chosen from this: bridge firmware never goes anywhere else.
    let bridges: Vec<&PortCandidate> =
        candidates.iter().filter(|candidate| could_be_a_bridge(candidate)).collect();
    let chosen = match named {
        Some((given, spec)) => {
            let named: Vec<&PortCandidate> =
                bridges.iter().copied().filter(|candidate| matches(spec, candidate)).collect();
            match named.as_slice() {
                [only] => *only,
                [] => bail!(
                    "--bridge {given} matches no attached P4 bridge. `wartui ports` lists each \
                     board with its address; name the bridge by its address"
                ),
                several => bail!(
                    "--bridge {given} matches {} attached P4 bridges: {}. Name the bridge by \
                     more of its address",
                    several.len(),
                    several.iter().map(|board| board.label()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
        None => match (remembered, bridges.as_slice()) {
            (Some(wanted), _) => {
                bridges.iter().copied().find(|board| board.mac() == Some(wanted)).ok_or_else(
                    || {
                        let state = if candidates.iter().any(|board| board.mac() == Some(wanted)) {
                            "is attached but is not a P4 bridge"
                        } else {
                            "is not attached"
                        };
                        anyhow!(
                            "the remembered bridge {} {state}. Name the P4 bridge to flash with \
                             --bridge (`wartui ports` lists each board's address)",
                            mac::full(&wanted)
                        )
                    },
                )?
            }
            (None, [only]) => *only,
            (None, []) => bail!(
                "no P4 bridge is attached. A blank P4 shows up only as its CH343, and takes its \
                 first flash by hand, per {}",
                p4::FIRST_FLASH
            ),
            (None, several) => bail!(
                "{} P4 bridges are attached and none is known to be the bridge: {}. Name it with \
                 --bridge (`wartui ports` lists each board's address)",
                several.len(),
                several.iter().map(|board| board.label()).collect::<Vec<_>>().join(", "),
            ),
        },
    };
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
    use wartui_bridge::ports::{TEST_PID, TEST_VID, candidate, is_p4_bridge};

    use super::super::testing::{BRIDGE_MAC, NODE_MAC, board};
    use super::super::{P4_BRIDGE_APP, Source, normalize, release_asset, source};
    use super::*;

    fn named(given: &str) -> (&str, BridgeSpec) {
        (given, spec(given))
    }

    /// A P4 bridge as the OS lists it, its serial number the bridge's address.
    fn bridge(device: &str, address: &Mac) -> PortCandidate {
        let serial = mac::full(address).to_string();
        let mut p4 = candidate(device, Some(TEST_VID), Some(TEST_PID), Some(&serial));
        p4.product = Some("wartui p4 bridge".to_owned());
        p4
    }

    #[test]
    fn flash_bridge_targets_named_board_when_bridge_is_attached() {
        let mut p4 = bridge("/dev/ttyACM0", &BRIDGE_MAC);
        p4.path = "/dev/serial/by-id/usb-wartui_10:BD:A3:EC:44:C0-if00".to_owned();
        let found = [p4.clone(), board("/dev/ttyACM1", Some(&NODE_MAC))];
        for given in ["10:bd:a3:ec:44:c0", p4.path.as_str(), "/dev/ttyACM0"] {
            let (given, spec) = named(given);
            assert_eq!(target(&found, Some((given, &spec)), Some(NODE_MAC)).unwrap(), p4);
        }
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_bridge_matches_no_board() {
        let found = [bridge("/dev/ttyACM1", &NODE_MAC)];
        let (given, spec) = named("10:BD:A3:EC:44:C0");
        let error = target(&found, Some((given, &spec)), None).unwrap_err().to_string();
        assert!(error.contains("--bridge 10:BD:A3:EC:44:C0"), "{error}");
        assert!(error.contains("wartui ports"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_board_is_not_a_p4_bridge() {
        // Attached, and the operator's own choice, but bridge firmware over a node takes it out
        // of the fleet.
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        for given in ["3C:DC:75:84:A1:B0", "A1:B0", "/dev/ttyACM0"] {
            let (given, spec) = named(given);
            let error = target(&found, Some((given, &spec)), None).unwrap_err().to_string();
            assert!(error.contains("matches no attached P4 bridge"), "{error}");
        }
    }

    #[test]
    fn flash_bridge_targets_named_board_when_its_last_octets_match_one_board() {
        let found = [bridge("/dev/ttyACM0", &NODE_MAC), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let (given, spec) = named("44:c0");
        assert_eq!(target(&found, Some((given, &spec)), None).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_tail_matches_several_boards() {
        // A guess between two bridges ending the same way is never made.
        let twin = [0x3C, 0xDC, 0x75, 0x84, 0x44, 0xC0];
        let found = [bridge("/dev/ttyACM0", &twin), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let (given, spec) = named("44:C0");
        let error = target(&found, Some((given, &spec)), Some(BRIDGE_MAC)).unwrap_err().to_string();
        assert!(error.contains("--bridge 44:C0 matches 2 attached P4 bridges"), "{error}");
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&twin).to_string()),
            "{error}"
        );
    }

    #[test]
    fn flash_bridge_targets_remembered_board_when_several_are_attached() {
        let found = [bridge("/dev/ttyACM0", &NODE_MAC), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        assert_eq!(target(&found, None, Some(BRIDGE_MAC)).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_targets_only_board_when_nothing_is_known() {
        let found = [bridge("/dev/ttyACM0", &BRIDGE_MAC)];
        let chosen = target(&found, None, None).unwrap();
        assert_eq!(chosen, found[0]);
        assert!(is_p4_bridge(&chosen));
    }

    #[test]
    fn flash_bridge_targets_p4_bridge_when_a_node_is_also_attached() {
        // The node sorts first and is an ordinary sight on the desk. It blocks nothing.
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        assert_eq!(target(&found, None, None).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_refuses_run_when_remembered_bridge_is_not_attached() {
        // The one board present is known not to be the bridge.
        let found = [bridge("/dev/ttyACM0", &BRIDGE_MAC)];
        let error = target(&found, None, Some(NODE_MAC)).unwrap_err().to_string();
        assert!(error.contains(&mac::full(&NODE_MAC).to_string()), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_remembered_board_is_a_node() {
        // Attached, and remembered, and still not a P4 bridge. The P4 bridge beside it is not
        // taken in its place.
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let error = target(&found, None, Some(NODE_MAC)).unwrap_err().to_string();
        assert!(error.contains("is attached but is not a P4 bridge"), "{error}");
        assert!(error.contains(&mac::full(&NODE_MAC).to_string()), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_says_not_attached_when_remembered_board_is_absent() {
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let absent = [0x3C, 0xDC, 0x75, 0x84, 0x00, 0x08];
        let error = target(&found, None, Some(absent)).unwrap_err().to_string();
        assert!(error.contains("is not attached"), "{error}");
        assert!(error.contains(&mac::full(&absent).to_string()), "{error}");
    }

    #[test]
    fn flash_bridge_targets_remembered_board_when_nodes_are_beside_it() {
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC)), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        assert_eq!(target(&found, None, Some(BRIDGE_MAC)).unwrap(), found[1]);
    }

    #[test]
    fn flash_bridge_refuses_run_when_several_boards_attached_and_none_known() {
        let found = [bridge("/dev/ttyACM0", &BRIDGE_MAC), bridge("/dev/ttyACM1", &NODE_MAC)];
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
        assert!(error.contains("no P4 bridge is attached"), "{error}");
        assert!(error.contains(p4::FIRST_FLASH), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_only_nodes_are_attached() {
        let found = [board("/dev/ttyACM0", Some(&NODE_MAC))];
        let error = target(&found, None, None).unwrap_err().to_string();
        assert!(error.contains("no P4 bridge is attached"), "{error}");
        assert!(error.contains(p4::FIRST_FLASH), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_board_when_it_reports_no_address() {
        let mut bare = bridge("/dev/ttyACM0", &BRIDGE_MAC);
        bare.serial = None;
        let error = target(&[bare], None, None).unwrap_err().to_string();
        assert!(error.contains("reports no address"), "{error}");
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
    fn flash_bridge_finds_release_asset_when_features_are_default() {
        let asset = |features: &[&str]| release_asset(&P4_BRIDGE_APP, features);
        assert_eq!(asset(&[]), Some("wartui-p4-bridge-app-esp32p4.bin"));
        assert_eq!(asset(&["esp32c6"]), None);
        assert_eq!(asset(&["gps"]), None);
    }

    #[test]
    fn flash_bridge_chooses_source_when_given_each_combination() {
        let firmware = &P4_BRIDGE_APP;
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        let found = source(firmware, Some(image), None, &[], false, Some("v1")).unwrap();
        assert_eq!(found, Source::File(image.to_owned()));
        let found = source(firmware, None, Some(dir), &[], false, Some("v1")).unwrap();
        assert_eq!(found, Source::Build(dir.to_owned()));
        let found = source(firmware, None, None, &[], false, None).unwrap();
        assert_eq!(found, Source::Build(normalize(Path::new(firmware.dir))));
        let Source::Build(default) = found else { panic!("not a build") };
        assert!(default.ends_with("firmware/p4-bridge"), "{}", default.display());
        let found = source(firmware, None, None, &[], false, Some("v1")).unwrap();
        let asset = "wartui-p4-bridge-app-esp32p4.bin";
        assert_eq!(found, Source::Release { tag: "v1".to_owned(), asset });
        let error = source(firmware, None, None, &["log"], false, Some("v1")).unwrap_err();
        assert!(error.to_string().contains("no P4 bridge image"), "{error}");
    }
}
