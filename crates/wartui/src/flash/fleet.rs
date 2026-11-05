//! `wartui flash-fleet` — one node image onto every attached ESP32-C5.
//!
//! **No bridge has to be known.** A P4 bridge's CH343 is told apart by its USB IDs, skipped and
//! never probed, so this command reads no `wartui.toml`. Every other attached Espressif native-USB
//! ESP32-C5 gets node firmware, including any other C5 board on the host: `--skip` is what keeps
//! one, and any other chip fails the cross-check below.
//!
//! **A board that fails the identity cross-check (`super`) is skipped**, neither flashed nor
//! failed, and the rest of the fleet goes ahead. So is a board with under 4 MB of flash, or that
//! does not say how much it has: the node's partition table holds two app slots and needs 4 MB.
//!
//! **Each node's `otadata` is erased after its image is written**, so it boots the `ota_0` the
//! image went to whatever an earlier over-the-air update chose. There is no factory slot to fall
//! back on.
//!
//! **A node counts as flashed only once it is seen to boot.** espflash's reset over the USB
//! Serial/JTAG control lines does not always take, and the old image then runs on from RAM until a
//! power cycle. After the erase, the node's port is read for a line only a boot prints: the
//! bootloader's, or the firmware's banner with this board's address (`boot.rs`). A node not
//! seen to boot gets one `espflash reset` and is watched again.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::Args as ClapArgs;
use wartui_bridge::ports::{PortCandidate, SERIAL_JTAG_PID, is_ch343};
use wartui_bridge::serial::discover_ports;
use wartui_proto::mac::Mac;

use super::boot::seen_booting;
use super::{
    Chip, NODE, RELEASE_TAG, Ran, Skip, Source, check_image, chip_of, erase_otadata_args, espflash,
    espflash_present, feature_args, features, flash_args, judge_node_flash, judge_probe, mac_arg,
    name, obtain_with, probe_args, read, reset_args, source,
};

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The node firmware's features, comma-separated, as `cargo build` takes them. They must
    /// name `esp32c5`, and only boards of that chip are flashed.
    #[arg(long, value_name = "LIST", default_value = "esp32c5")]
    pub features: String,

    /// Leave this board alone. Repeatable.
    #[arg(long = "skip", value_name = "MAC", value_parser = mac_arg)]
    pub skip: Vec<Mac>,

    /// Flash this merged image, which holds flash from address 0 and carries
    /// `firmware/node/partitions.csv`, rather than building or downloading one.
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
    let source =
        source(&NODE, args.image.as_deref(), args.firmware_dir.as_deref(), &features, RELEASE_TAG)?;
    println!("image  {}", source.describe(args.dry_run));

    let image = if args.dry_run {
        // A file named on the command line costs nothing to read, and a wrong one is worth
        // hearing about before the real run.
        if let Source::File(path) = &source {
            check_image(&read(path)?, chip, NODE.firmware.merged)?;
        }
        None
    } else {
        let cargo = feature_args(&features);
        Some(obtain_with(&NODE.firmware, &source, chip, &cargo)?)
    };

    let found = discover_ports().context("listing serial ports")?;
    let mut probe = Vec::new();
    let mut report = Vec::new();
    for verdict in select(&found, &args.skip) {
        match verdict {
            Verdict::Probe(candidate) => probe.push(candidate),
            Verdict::Skip(candidate, why) => report.push((candidate, Outcome::Skipped(why))),
        }
    }
    let jobs = usize::from(args.jobs);
    let probed = in_parallel(&probe, jobs, |candidate| {
        let expected = candidate.mac().expect("select probes only boards with an address");
        judge_probe(expected, chip, &espflash(&probe_args(&candidate.path)))
            .and_then(|info| judge_node_flash(&info))
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
            let flashed =
                in_parallel(&verified, jobs, |candidate| flash_node(candidate, chip, image));
            let failed =
                flashed.iter().filter(|outcome| !matches!(outcome, Outcome::Flashed)).count();
            report.extend(verified.into_iter().zip(flashed));
            failed
        }
    };

    if report.is_empty() {
        println!("No Espressif board or P4 bridge is attached.");
    }
    report.sort_by(|a, b| a.0.path.cmp(&b.0.path));
    for (candidate, outcome) in &report {
        println!("{:<6} {outcome}", name(candidate));
        if let Some(ran) = outcome.failed_run() {
            for line in ran.stdout.lines().chain(ran.stderr.lines()) {
                println!("         {line}");
            }
        }
    }
    ensure!(failed == 0, "{failed} board(s) failed to flash or to reboot");
    Ok(())
}

/// How long a node has to be seen booting after each reset.
const BOOT_WAIT: Duration = Duration::from_secs(10);

/// Write the merged image, erase `otadata`, then watch the node boot, resetting it once more if
/// it does not.
///
/// An earlier over-the-air update leaves `otadata` pointing at whichever slot it wrote, and the
/// merged image puts the app in `ota_0`. The erase makes the bootloader boot `ota_0`, rather
/// than leaving that to how espflash writes the erased gap the image carries at 0xd000.
fn flash_node(candidate: &PortCandidate, chip: Chip, image: &Path) -> Outcome {
    let address = candidate.mac().expect("select probes only boards with an address");
    let port = candidate.path.as_str();
    let written = espflash(&flash_args(port, chip, image));
    if !written.ok {
        return Outcome::Failed(written);
    }
    let erased = espflash(&erase_otadata_args(port, chip));
    if !erased.ok {
        return Outcome::Failed(erased);
    }
    if seen_booting(address, BOOT_WAIT) {
        return Outcome::Flashed;
    }
    let reset = espflash(&reset_args(port, chip));
    if seen_booting(address, BOOT_WAIT) { Outcome::Flashed } else { Outcome::NotRebooted(reset) }
}

/// What to do with one attached board.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Verdict {
    Probe(PortCandidate),
    Skip(PortCandidate, Skip),
}

/// Which attached boards are worth probing. Reads nothing and opens nothing.
fn select(candidates: &[PortCandidate], skips: &[Mac]) -> Vec<Verdict> {
    candidates
        .iter()
        .map(|candidate| {
            let skip = if is_ch343(candidate) {
                Some(Skip::P4Bridge)
            } else if candidate.pid != Some(SERIAL_JTAG_PID) {
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
    /// Not seen to boot, with the backup `espflash reset`'s run.
    NotRebooted(Ran),
    Failed(Ran),
}

impl Outcome {
    /// The espflash run whose full output belongs beneath this outcome's line.
    fn failed_run(&self) -> Option<&Ran> {
        match self {
            Self::Failed(ran) => Some(ran),
            Self::NotRebooted(ran) if !ran.ok => Some(ran),
            _ => None,
        }
    }
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Skipped(why) => write!(f, "skipped: {why}"),
            Self::WouldFlash => f.write_str("would flash"),
            Self::Flashed => f.write_str("flashed"),
            Self::NotRebooted(ran) if ran.ok => {
                f.write_str("flashed, but not seen to reboot: power-cycle it to run the new image")
            }
            Self::NotRebooted(ran) => write!(
                f,
                "flashed, but not seen to reboot, and espflash reset failed: {}; power-cycle it \
                 to run the new image",
                ran.last_line()
            ),
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

    use wartui_bridge::ports::{CH343_PID, CH343_VID, ESPRESSIF_VID, candidate};

    use super::super::normalize;
    use super::super::release_asset;
    use super::super::testing::{NODE_MAC, board, ran};
    use super::*;

    #[test]
    fn flash_fleet_skips_board_when_no_mac_is_reported() {
        let found = [board("/dev/ttyACM0", None)];
        let verdicts = select(&found, &[]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::NoAddress)]);
    }

    #[test]
    fn flash_fleet_skips_board_when_named_with_skip() {
        let found = [board("/dev/ttyACM1", Some(&NODE_MAC))];
        let verdicts = select(&found, &[NODE_MAC]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::Operator)]);
    }

    #[test]
    fn flash_fleet_skips_board_when_product_is_not_serial_jtag() {
        let found = [candidate("/dev/ttyACM1", Some(ESPRESSIF_VID), Some(0x0002), Some("x"))];
        let verdicts = select(&found, &[]);
        assert_eq!(verdicts, [Verdict::Skip(found[0].clone(), Skip::NotSerialJtag(Some(2)))]);
    }

    #[test]
    fn flash_fleet_skips_board_when_it_is_a_p4_bridge() {
        let p4 = candidate("/dev/ttyACM3", Some(CH343_VID), Some(CH343_PID), None);
        let verdicts = select(std::slice::from_ref(&p4), &[]);
        assert_eq!(verdicts, [Verdict::Skip(p4, Skip::P4Bridge)]);
        assert!(Skip::P4Bridge.to_string().contains("wartui flash-bridge"));
    }

    #[test]
    fn flash_fleet_skips_p4_bridge_and_probes_node_when_no_bridge_is_known() {
        let p4 = candidate("/dev/ttyACM0", Some(CH343_VID), Some(CH343_PID), Some("5CF7153520"));
        let node = board("/dev/ttyACM1", Some(&NODE_MAC));
        let verdicts = select(&[p4.clone(), node.clone()], &[]);
        assert_eq!(verdicts, [Verdict::Skip(p4, Skip::P4Bridge), Verdict::Probe(node)]);
    }

    #[test]
    fn flash_fleet_finds_release_asset_when_features_are_in_any_order() {
        assert_eq!(release_asset(&NODE, &["esp32c5"]), Some("wartui-node-fw-esp32c5.bin"));
        assert_eq!(
            release_asset(&NODE, &["esp32c5", "esp32c5"]),
            Some("wartui-node-fw-esp32c5.bin")
        );
    }

    #[test]
    fn flash_fleet_refuses_release_asset_when_features_are_outside_table() {
        assert_eq!(release_asset(&NODE, &["esp32c5", "extra"]), None);
        assert_eq!(release_asset(&NODE, &["esp32c6"]), None);
        assert_eq!(release_asset(&NODE, &["esp32c5", "esp32c6"]), None);
    }

    #[test]
    fn flash_fleet_chooses_source_when_given_each_combination() {
        let image = Path::new("fw.bin");
        let dir = Path::new("fw");
        let c5 = ["esp32c5"];
        let found = source(&NODE, Some(image), None, &c5, Some("v1")).unwrap();
        assert_eq!(found, Source::File(image.to_owned()));
        let found = source(&NODE, None, Some(dir), &c5, Some("v1")).unwrap();
        assert_eq!(found, Source::Build(dir.to_owned()));
        let found = source(&NODE, None, None, &c5, None).unwrap();
        assert_eq!(found, Source::Build(normalize(Path::new(NODE.firmware.dir))));
        let found = source(&NODE, None, None, &c5, Some("")).unwrap();
        assert_eq!(found, Source::Build(normalize(Path::new(NODE.firmware.dir))));
        let found = source(&NODE, None, None, &c5, Some("v1")).unwrap();
        let asset = "wartui-node-fw-esp32c5.bin";
        assert_eq!(found, Source::Release { tag: "v1".to_owned(), asset });
    }

    #[test]
    fn flash_fleet_shows_default_firmware_dir_without_parent_segments_when_building() {
        let Source::Build(dir) = source(&NODE, None, None, &["esp32c5"], None).unwrap() else {
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
    fn flash_fleet_refuses_release_source_when_features_are_outside_table() {
        let error =
            source(&NODE, None, None, &["esp32c5", "extra"], Some("v1")).unwrap_err().to_string();
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

    #[test]
    fn flash_fleet_advises_power_cycle_when_node_is_not_seen_to_reboot() {
        let shown = Outcome::NotRebooted(ran(true, "")).to_string();
        assert_eq!(shown, "flashed, but not seen to reboot: power-cycle it to run the new image");
    }

    #[test]
    fn flash_fleet_names_reset_error_when_backup_reset_fails() {
        let outcome = Outcome::NotRebooted(ran(false, "Error: port busy\n"));
        assert_eq!(
            outcome.to_string(),
            "flashed, but not seen to reboot, and espflash reset failed: Error: port busy; \
             power-cycle it to run the new image"
        );
        assert!(outcome.failed_run().is_some());
    }
}
