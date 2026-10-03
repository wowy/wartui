//! `wartui flash-fleet` — one node image onto every attached board of one chip.
//!
//! **The bridge is spared before anything else happens, or nothing happens.** It is a C5 or C6
//! with the same vendor and product ID as every node, so nothing on the bus tells it apart but its
//! address, and a node image written over it takes the fleet's radio away until `flash-bridge`
//! puts it back. The run therefore needs that address — `--bridge`, else the one `run` remembered —
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
//! **A board that fails the identity cross-check (`super`) is skipped**, neither flashed nor
//! failed, and the rest of the fleet goes ahead.

use std::fmt;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result, bail, ensure};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{BRIDGE_PID, PortCandidate};
use wartui_bridge::remember::BridgeMemory;
use wartui_bridge::serial::{BridgeSpec, discover_ports};
use wartui_proto::mac::Mac;

use super::{
    NODE, RELEASE_TAG, Ran, Skip, Source, canonical, check_image, chip_of, espflash,
    espflash_present, features, flash_args, judge_probe, mac_arg, matches, name, obtain,
    probe_args, read, source,
};
use crate::spec;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The node firmware's features, comma-separated, as `cargo build` takes them. Exactly one
    /// of `esp32c5` or `esp32c6`, and only boards of that chip are flashed.
    #[arg(long, value_name = "LIST", required = true)]
    pub features: String,

    /// Build without the firmware's default features.
    #[arg(long)]
    pub no_default_features: bool,

    /// Which board is the bridge, by path, by address or by the end of it (`00:08`), so that it is
    /// never flashed. It must be attached, and the end of an address must match one board. The one
    /// `run` last found is spared as well, and is the only one by default.
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

pub fn run(args: Args) -> Result<()> {
    let features = features(&args.features);
    let chip = chip_of(&NODE, &features)?;
    espflash_present()?;
    let mut bridge =
        bridge_to_spare(args.bridge.as_deref(), BridgeMemory::discover().recall(), args.no_bridge)?;
    bridge.named = bridge.named.map(|(given, spec)| (given, canonical(spec)));
    let source = source(
        &NODE,
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
        Some(obtain(&NODE, &source, &features, args.no_default_features, chip)?)
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

/// Refuse a `--bridge` that names no attached board, since sparing it would spare nothing, and one
/// that several boards end with, since which of them is the bridge is the question it was asked to
/// settle.
fn named_bridge_attached(candidates: &[PortCandidate], spare: &Spare) -> Result<()> {
    let Some((given, spec)) = &spare.named else { return Ok(()) };
    let named: Vec<_> = candidates.iter().filter(|candidate| matches(spec, candidate)).collect();
    ensure!(
        !named.is_empty(),
        "--bridge {given} matches no attached board, so the bridge could not be spared. \
         `wartui ports` lists each board with its address; name the bridge by its address"
    );
    ensure!(
        named.len() == 1,
        "--bridge {given} matches {} attached boards: {}. Name the bridge by more of its address",
        named.len(),
        named.iter().map(|board| board.label()).collect::<Vec<_>>().join(", ")
    );
    Ok(())
}

/// What to do with one attached board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Probe(PortCandidate),
    Skip(PortCandidate, Skip),
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
    use std::path::Path;

    use wartui_bridge::ports::{ESPRESSIF_VID, candidate};

    use super::super::normalise;
    use super::super::release_asset;
    use super::super::testing::{BRIDGE_MAC, NODE_MAC, board};
    use super::*;
    use wartui_proto::mac;

    fn remembered(address: Mac) -> Spare {
        Spare { named: None, remembered: Some(address) }
    }

    #[test]
    fn flash_fleet_skips_bridge_when_remembered_mac_matches() {
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let verdicts = select(&found, &remembered(BRIDGE_MAC), &[]);
        assert_eq!(verdicts[0], Verdict::Skip(found[0].clone(), Skip::Bridge));
        assert_eq!(verdicts[1], Verdict::Probe(found[1].clone()));
    }

    #[test]
    fn flash_fleet_skips_bridge_when_named_by_path_or_device() {
        let mut bridge = board("/dev/ttyACM0", Some(&BRIDGE_MAC));
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
        let found = [board("/dev/ttyACM1", Some(&NODE_MAC))];
        let verdicts = select(&found, &Spare::default(), &[NODE_MAC]);
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
        let spare = bridge_to_spare(Some("/dev/ttyACM3"), Some(BRIDGE_MAC), false).unwrap();
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let verdicts = select(&found, &spare, &[]);
        assert_eq!(verdicts[0], Verdict::Skip(found[0].clone(), Skip::Bridge));
        assert_eq!(verdicts[1], Verdict::Probe(found[1].clone()));
    }

    #[test]
    fn flash_fleet_spares_both_when_named_and_remembered_bridges_differ() {
        let spare = bridge_to_spare(Some("/dev/ttyACM1"), Some(BRIDGE_MAC), false).unwrap();
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let verdicts = select(&found, &spare, &[]);
        assert!(verdicts.iter().all(|v| matches!(v, Verdict::Skip(_, Skip::Bridge))));
    }

    #[test]
    fn flash_fleet_refuses_run_when_bridge_path_matches_no_candidate() {
        let spare =
            bridge_to_spare(Some("/dev/tty.usbmodem2101"), Some(BRIDGE_MAC), false).unwrap();
        let found = [board("/dev/cu.usbmodem2101", Some(&BRIDGE_MAC))];
        let error = named_bridge_attached(&found, &spare).unwrap_err().to_string();
        assert!(error.contains("--bridge /dev/tty.usbmodem2101"), "{error}");
        assert!(error.contains("wartui ports"), "{error}");
    }

    #[test]
    fn flash_fleet_refuses_run_when_bridge_mac_matches_no_candidate() {
        let spare = bridge_to_spare(Some("10:BD:A3:EC:44:C0"), None, false).unwrap();
        let found = [board("/dev/ttyACM1", Some(&NODE_MAC))];
        let error = named_bridge_attached(&found, &spare).unwrap_err().to_string();
        assert!(error.contains("--bridge 10:BD:A3:EC:44:C0"), "{error}");
    }

    #[test]
    fn flash_fleet_accepts_run_when_named_bridge_is_attached() {
        let found = [board("/dev/ttyACM0", Some(&BRIDGE_MAC))];
        for named in ["/dev/ttyACM0", "10:bd:a3:ec:44:c0"] {
            let spare = bridge_to_spare(Some(named), None, false).unwrap();
            named_bridge_attached(&found, &spare).unwrap();
        }
    }

    #[test]
    fn flash_fleet_spares_only_that_board_when_bridge_is_named_by_its_last_octets() {
        let found =
            [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&NODE_MAC))];
        let spare = bridge_to_spare(Some("44:c0"), None, false).unwrap();
        named_bridge_attached(&found, &spare).unwrap();
        let verdicts = select(&found, &spare, &[]);
        assert_eq!(verdicts[0], Verdict::Skip(found[0].clone(), Skip::Bridge));
        assert_eq!(verdicts[1], Verdict::Probe(found[1].clone()));
    }

    #[test]
    fn flash_fleet_refuses_run_when_bridge_tail_matches_several_boards() {
        // Sparing both would leave the fleet half-flashed on a guess; saying which is
        // the bridge is what `--bridge` was for.
        let twin = [0x3C, 0xDC, 0x75, 0x84, 0x44, 0xC0];
        let found = [board("/dev/ttyACM0", Some(&BRIDGE_MAC)), board("/dev/ttyACM1", Some(&twin))];
        let spare = bridge_to_spare(Some("44:C0"), None, false).unwrap();
        let error = named_bridge_attached(&found, &spare).unwrap_err().to_string();
        assert!(error.contains("--bridge 44:C0 matches 2 attached boards"), "{error}");
        assert!(
            error.contains(&mac::full(&BRIDGE_MAC).to_string())
                && error.contains(&mac::full(&twin).to_string()),
            "{error}"
        );
    }

    #[test]
    fn flash_fleet_accepts_run_when_remembered_bridge_is_not_attached() {
        // An unplugged bridge is not a mistake; only a named one that is absent is.
        let found = [board("/dev/ttyACM1", Some(&NODE_MAC))];
        named_bridge_attached(&found, &remembered(BRIDGE_MAC)).unwrap();
        let verdicts = select(&found, &remembered(BRIDGE_MAC), &[]);
        assert_eq!(verdicts, [Verdict::Probe(found[0].clone())]);
    }

    #[test]
    fn flash_fleet_spares_nothing_when_no_bridge_is_passed_and_none_is_remembered() {
        assert_eq!(bridge_to_spare(None, None, true).unwrap(), Spare::default());
    }

    #[test]
    fn flash_fleet_spares_remembered_bridge_when_no_bridge_is_given() {
        // `--no-bridge` lifts the refusal; it never unprotects a board known to be the bridge.
        assert_eq!(bridge_to_spare(None, Some(BRIDGE_MAC), true).unwrap(), remembered(BRIDGE_MAC));
    }

    #[test]
    fn flash_fleet_finds_release_asset_when_features_are_in_any_order() {
        let asset = release_asset(&NODE, &["xiao-external-antenna", "esp32c6"]);
        assert_eq!(asset, Some("wartui-node-fw-esp32c6-xiao.bin"));
        assert_eq!(release_asset(&NODE, &["esp32c5"]), Some("wartui-node-fw-esp32c5.bin"));
        assert_eq!(release_asset(&NODE, &["esp32c6"]), Some("wartui-node-fw-esp32c6.bin"));
    }

    #[test]
    fn flash_fleet_refuses_release_asset_when_features_are_outside_table() {
        assert_eq!(release_asset(&NODE, &["esp32c5", "xiao-external-antenna"]), None);
        assert_eq!(release_asset(&NODE, &["esp32c6", "log"]), None);
        assert_eq!(release_asset(&NODE, &["esp32c5", "t-dongle-c5"]), None);
    }

    #[test]
    fn flash_fleet_chooses_source_when_given_each_combination() {
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        let c5 = ["esp32c5"];
        let found = source(&NODE, Some(image), None, &c5, false, Some("v1")).unwrap();
        assert_eq!(found, Source::File(image.to_owned()));
        let found = source(&NODE, None, Some(dir), &c5, false, Some("v1")).unwrap();
        assert_eq!(found, Source::Build(dir.to_owned()));
        let found = source(&NODE, None, None, &c5, false, None).unwrap();
        assert_eq!(found, Source::Build(normalise(Path::new(NODE.dir))));
        let found = source(&NODE, None, None, &c5, false, Some("")).unwrap();
        assert_eq!(found, Source::Build(normalise(Path::new(NODE.dir))));
        let found = source(&NODE, None, None, &c5, false, Some("v1")).unwrap();
        let asset = "wartui-node-fw-esp32c5.bin";
        assert_eq!(found, Source::Release { tag: "v1".to_owned(), asset });
    }

    #[test]
    fn flash_fleet_shows_default_firmware_dir_without_parent_segments_when_building() {
        let Source::Build(dir) = source(&NODE, None, None, &["esp32c5"], false, None).unwrap()
        else {
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
    fn flash_fleet_refuses_release_source_when_default_features_are_off() {
        let error =
            source(&NODE, None, None, &["esp32c5"], true, Some("v1")).unwrap_err().to_string();
        assert!(error.contains("--firmware-dir") && error.contains("--image"), "{error}");
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
