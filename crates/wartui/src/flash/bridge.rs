//! `wartui flash-bridge` — bridge firmware onto exactly one board, the P4 bridge.
//!
//! **The board is chosen before any image is sent, and only when the choice is certain.** Each
//! attached CH343 is asked which bridge it is (`serial::probe`), and the choice is made among
//! those that answered: a node beside one blocks nothing, and no node can be chosen. So the target
//! is the P4 bridge `--bridge` names, which must answer; else the bridge remembered in
//! `wartui.toml`, which must answer; else, with none remembered, the only P4 bridge that answered.
//! A remembered bridge that does not answer is a refusal even when another P4 bridge does: that
//! board is known not to be the bridge. A remembered address that a node carries is refused too,
//! which is why the listing holds every fleet board: the refusal has to say which of the two it
//! is. Several and none known is a refusal that lists them, never a guess.
//!
//! **No P4 bridge answering points at the first flash.** A blank P4 is a CH343 that does not
//! answer, so that flash is by hand ([`p4::FIRST_FLASH`]).
//!
//! **A board found here is remembered once flashed.** `flash-bridge` saves its address and its
//! CH343's serial number to `wartui.toml` as `[bridge] mac` and `serial`, the way `run` does once
//! it connects, so `run` and the next `flash-bridge` open that board first. Nothing is saved with remembering off, or for a board
//! named with `--bridge`: one flashed by name is often some other device, and must not disturb
//! which bridge the main fleet's `run` uses. A save that fails leaves the flash a success, and says
//! so.
//!
//! **A P4 bridge is updated over its own link** (`super::p4`): the cable the host already holds is
//! the one that carries the update.
//!
//! There is no `--skip` or `--jobs`: there is one target. There is no `--features` either: the P4
//! bridge firmware has none, so a build and a release image are the same.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{PortCandidate, could_be_a_bridge};
use wartui_bridge::serial::{BridgeSpec, Probed, discover_ports, probe_all};
use wartui_proto::mac::{self, Mac};

use super::{canonical, matches, p4};
use crate::{config, spec};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Which P4 bridge to flash, by path, by address or by the end of it (`71:F4`), which must
    /// match one P4 bridge that answers. It is not remembered. Without it, the bridge remembered
    /// in `wartui.toml`, which must answer; else, with none remembered, the only P4 bridge that
    /// answers. A board found without it is remembered as the bridge once flashed.
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
    /// downloads and sends no image; each CH343 is still asked which bridge it is.
    #[arg(long)]
    pub dry_run: bool,
}

pub async fn run(args: Args) -> Result<()> {
    let named = args.bridge.as_deref().map(|given| (given, canonical(spec(given))));
    let found = discover_ports().context("listing serial ports")?;
    let ch343s: Vec<PortCandidate> =
        found.iter().filter(|c| could_be_a_bridge(c)).cloned().collect();
    let probed = probe_all(&ch343s).await;
    let (board, expected) = target(
        &found,
        &probed,
        named.as_ref().map(|(given, spec)| (*given, spec)),
        super::remembered_bridge(args.config.as_deref())?,
    )?;
    p4::run(&args, &board, expected).await?;
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
        .and_then(|path| remember_flashed(&path, mac, board.serial.clone()));
    if let Err(why) = saved {
        println!(
            "{:<6} not remembered as the bridge, so `run` and the next flash-bridge will not \
             open it first: {why:#}",
            mac::short(&mac)
        );
    }
}

/// Save `mac`, on the CH343 with `serial`, as the bridge in the `wartui.toml` at `path`, keeping
/// every other key. A missing file is created. Writes nothing when the file says
/// `remember = false`.
fn remember_flashed(path: &Path, mac: Mac, serial: Option<String>) -> Result<()> {
    let mut config = config::load_from(None, || Some(path.to_path_buf()))?;
    if config.bridge.remember == Some(false) {
        return Ok(());
    }
    config.bridge.mac = Some(mac);
    config.bridge.serial = serial;
    config::save(path, &config)
}

/// Whether `spec` names `board`: its path, or the address it answered with.
fn names(spec: &BridgeSpec, board: &Probed) -> bool {
    match spec {
        BridgeSpec::Path(_) => matches(spec, &board.candidate),
        BridgeSpec::Mac(_) | BridgeSpec::Tail(_) => {
            board.mac.is_some_and(|address| spec.matches_address(&address))
        }
    }
}

/// The one board to flash, and the address it answered with, out of the CH343s `probed`.
/// `candidates` is every fleet board attached. Reads nothing and opens nothing.
///
/// `named` is `--bridge` as typed and as matched; `remembered` is the bridge in `wartui.toml`.
fn target(
    candidates: &[PortCandidate],
    probed: &[Probed],
    named: Option<(&str, &BridgeSpec)>,
    remembered: Option<Mac>,
) -> Result<(PortCandidate, Mac)> {
    // Everything chosen below is chosen from the CH343s that answered: bridge firmware never
    // goes anywhere else.
    let bridges: Vec<&Probed> = probed.iter().filter(|board| board.mac.is_some()).collect();
    let chosen = match named {
        Some((given, spec)) => {
            let named: Vec<&Probed> =
                bridges.iter().copied().filter(|board| names(spec, board)).collect();
            match named.as_slice() {
                [only] => *only,
                [] => bail!(
                    "--bridge {given} matches no attached P4 bridge. `wartui ports` asks each \
                     CH343 which bridge it is; name the bridge by its address"
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
                bridges.iter().copied().find(|board| board.mac == Some(wanted)).ok_or_else(
                    || {
                        let state = if candidates.iter().any(|board| board.mac() == Some(wanted)) {
                            "is attached but is not a P4 bridge"
                        } else {
                            "did not answer on any CH343"
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
                "no P4 bridge answered. A blank P4 is a CH343 that does not answer, and takes its \
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
    let mac = chosen.mac.expect("only boards that answered are chosen");
    Ok((chosen.candidate.clone(), mac))
}

#[cfg(test)]
mod tests {
    use wartui_bridge::ports::{CH343_PID, CH343_VID, candidate, is_ch343};

    use super::super::testing::{BRIDGE_MAC, NODE_MAC, board};
    use super::*;

    fn named(given: &str) -> (&str, BridgeSpec) {
        (given, spec(given))
    }

    /// A CH343 as the OS lists it, with a serial number of its own.
    fn ch343(device: &str) -> PortCandidate {
        candidate(device, Some(CH343_VID), Some(CH343_PID), Some(device))
    }

    /// A CH343 whose probe answered as the bridge `address`.
    fn bridge(device: &str, address: &Mac) -> Probed {
        Probed { candidate: ch343(device), opened: true, mac: Some(*address) }
    }

    /// A CH343 whose probe heard nothing: a blank P4, or another device.
    fn silent(device: &str) -> Probed {
        Probed { candidate: ch343(device), opened: true, mac: None }
    }

    /// Every fleet board as `discover_ports` lists it: the nodes, then each probed CH343.
    fn listed(nodes: &[PortCandidate], probed: &[Probed]) -> Vec<PortCandidate> {
        nodes.iter().cloned().chain(probed.iter().map(|board| board.candidate.clone())).collect()
    }

    #[test]
    fn flash_bridge_targets_named_board_when_bridge_answers() {
        let mut p4 = bridge("/dev/ttyACM0", &BRIDGE_MAC);
        p4.candidate.path =
            "/dev/serial/by-id/usb-1a86_USB_Single_Serial_5CF7153520-if00".to_owned();
        let probed = [p4.clone()];
        let found = listed(&[board("/dev/ttyACM1", Some(&NODE_MAC))], &probed);
        for given in ["10:bd:a3:ec:44:c0", p4.candidate.path.as_str(), "/dev/ttyACM0"] {
            let (given, spec) = named(given);
            let chosen = target(&found, &probed, Some((given, &spec)), Some(NODE_MAC)).unwrap();
            assert_eq!(chosen, (p4.candidate.clone(), BRIDGE_MAC));
        }
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_bridge_matches_no_board() {
        let probed = [bridge("/dev/ttyACM1", &NODE_MAC)];
        let found = listed(&[], &probed);
        let (given, spec) = named("10:BD:A3:EC:44:C0");
        let error = target(&found, &probed, Some((given, &spec)), None).unwrap_err().to_string();
        assert!(error.contains("--bridge 10:BD:A3:EC:44:C0"), "{error}");
        assert!(error.contains("wartui ports"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_board_is_not_a_p4_bridge() {
        // Attached, and the operator's own choice, but bridge firmware over a node takes it out
        // of the fleet.
        let probed = [bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let found = listed(&[board("/dev/ttyACM0", Some(&NODE_MAC))], &probed);
        for given in ["3C:DC:75:84:A1:B0", "A1:B0", "/dev/ttyACM0"] {
            let (given, spec) = named(given);
            let error =
                target(&found, &probed, Some((given, &spec)), None).unwrap_err().to_string();
            assert!(error.contains("matches no attached P4 bridge"), "{error}");
        }
    }

    #[test]
    fn flash_bridge_refuses_named_path_when_its_ch343_does_not_answer() {
        let probed = [silent("/dev/ttyACM0")];
        let (given, spec) = named("/dev/ttyACM0");
        let error = target(&listed(&[], &probed), &probed, Some((given, &spec)), None);
        assert!(error.unwrap_err().to_string().contains("matches no attached P4 bridge"));
    }

    #[test]
    fn flash_bridge_targets_named_board_when_its_last_octets_match_one_board() {
        let probed = [bridge("/dev/ttyACM0", &NODE_MAC), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let (given, spec) = named("44:c0");
        let chosen = target(&listed(&[], &probed), &probed, Some((given, &spec)), None).unwrap();
        assert_eq!(chosen.0, probed[1].candidate);
    }

    #[test]
    fn flash_bridge_refuses_run_when_named_tail_matches_several_boards() {
        // A guess between two bridges ending the same way is never made.
        let twin = [0x3C, 0xDC, 0x75, 0x84, 0x44, 0xC0];
        let probed = [bridge("/dev/ttyACM0", &twin), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let (given, spec) = named("44:C0");
        let found = listed(&[], &probed);
        let error = target(&found, &probed, Some((given, &spec)), Some(BRIDGE_MAC))
            .unwrap_err()
            .to_string();
        assert!(error.contains("--bridge 44:C0 matches 2 attached P4 bridges"), "{error}");
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&twin).to_string()),
            "{error}"
        );
    }

    #[test]
    fn flash_bridge_targets_remembered_board_when_several_answer() {
        let probed = [bridge("/dev/ttyACM0", &NODE_MAC), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let chosen = target(&listed(&[], &probed), &probed, None, Some(BRIDGE_MAC)).unwrap();
        assert_eq!(chosen, (probed[1].candidate.clone(), BRIDGE_MAC));
    }

    #[test]
    fn flash_bridge_targets_only_board_when_nothing_is_known() {
        let probed = [bridge("/dev/ttyACM0", &BRIDGE_MAC)];
        let (chosen, mac) = target(&listed(&[], &probed), &probed, None, None).unwrap();
        assert_eq!((&chosen, mac), (&probed[0].candidate, BRIDGE_MAC));
        assert!(is_ch343(&chosen));
    }

    #[test]
    fn flash_bridge_passes_over_ch343_when_it_does_not_answer() {
        // A blank P4, or something else behind a CH343, beside the bridge.
        let probed = [silent("/dev/ttyACM0"), bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let chosen = target(&listed(&[], &probed), &probed, None, None).unwrap();
        assert_eq!(chosen.0, probed[1].candidate);
    }

    #[test]
    fn flash_bridge_targets_p4_bridge_when_a_node_is_also_attached() {
        // The node sorts first and is an ordinary sight on the desk. It blocks nothing.
        let probed = [bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let found = listed(&[board("/dev/ttyACM0", Some(&NODE_MAC))], &probed);
        assert_eq!(target(&found, &probed, None, None).unwrap().0, probed[0].candidate);
    }

    #[test]
    fn flash_bridge_refuses_run_when_remembered_bridge_does_not_answer() {
        // The one bridge present is known not to be the bridge.
        let probed = [bridge("/dev/ttyACM0", &BRIDGE_MAC)];
        let absent = [0x3C, 0xDC, 0x75, 0x84, 0x00, 0x08];
        let error = target(&listed(&[], &probed), &probed, None, Some(absent)).unwrap_err();
        let error = error.to_string();
        assert!(error.contains("did not answer on any CH343"), "{error}");
        assert!(error.contains(&mac::full(&absent).to_string()), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_remembered_board_is_a_node() {
        // Attached, and remembered, and still not a P4 bridge. The P4 bridge beside it is not
        // taken in its place.
        let probed = [bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let found = listed(&[board("/dev/ttyACM0", Some(&NODE_MAC))], &probed);
        let error = target(&found, &probed, None, Some(NODE_MAC)).unwrap_err().to_string();
        assert!(error.contains("is attached but is not a P4 bridge"), "{error}");
        assert!(error.contains(&mac::full(&NODE_MAC).to_string()), "{error}");
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_targets_remembered_board_when_nodes_are_beside_it() {
        let probed = [bridge("/dev/ttyACM1", &BRIDGE_MAC)];
        let found = listed(&[board("/dev/ttyACM0", Some(&NODE_MAC))], &probed);
        assert_eq!(target(&found, &probed, None, Some(BRIDGE_MAC)).unwrap().1, BRIDGE_MAC);
    }

    #[test]
    fn flash_bridge_refuses_run_when_several_answer_and_none_known() {
        let probed = [bridge("/dev/ttyACM0", &BRIDGE_MAC), bridge("/dev/ttyACM1", &NODE_MAC)];
        let error = target(&listed(&[], &probed), &probed, None, None).unwrap_err().to_string();
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&NODE_MAC).to_string()),
            "{error}"
        );
        assert!(error.contains("--bridge"), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_no_board_is_attached() {
        let error = target(&[], &[], None, None).unwrap_err().to_string();
        assert!(error.contains("no P4 bridge answered"), "{error}");
        assert!(error.contains(p4::FIRST_FLASH), "{error}");
    }

    #[test]
    fn flash_bridge_refuses_run_when_only_nodes_and_silent_ch343s_are_attached() {
        let probed = [silent("/dev/ttyACM1")];
        let found = listed(&[board("/dev/ttyACM0", Some(&NODE_MAC))], &probed);
        let error = target(&found, &probed, None, None).unwrap_err().to_string();
        assert!(error.contains("no P4 bridge answered"), "{error}");
        assert!(error.contains("a CH343 that does not answer"), "{error}");
        assert!(error.contains(p4::FIRST_FLASH), "{error}");
    }

    #[test]
    fn flash_bridge_remembers_flashed_board_when_file_holds_other_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let text = "excluded-ssids = [\"HomeNet\"]\n[bridge]\nmac = \"3C:DC:75:84:A1:B0\"\n";
        std::fs::write(&path, text).unwrap();

        remember_flashed(&path, BRIDGE_MAC, Some("5CF7153520".to_owned())).unwrap();

        let saved = config::load(Some(&path)).unwrap();
        assert_eq!(saved.bridge.mac, Some(BRIDGE_MAC), "the new bridge replaces the old");
        assert_eq!(saved.bridge.serial.as_deref(), Some("5CF7153520"), "with its port");
        assert_eq!(saved.excluded_ssids, ["HomeNet"], "the rest is kept");
    }

    #[test]
    fn flash_bridge_writes_nothing_when_remember_is_off() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui.toml");
        let original = "[bridge]\nremember = false\n";
        std::fs::write(&path, original).unwrap();

        remember_flashed(&path, BRIDGE_MAC, Some("5CF7153520".to_owned())).unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[test]
    fn flash_bridge_creates_file_when_remembering_and_none_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/wartui.toml");

        remember_flashed(&path, BRIDGE_MAC, None).unwrap();

        assert_eq!(config::load(Some(&path)).unwrap().bridge.mac, Some(BRIDGE_MAC));
    }
}
