//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard — each row stamped
//! with wherever the host believed it was at that moment: a GPS pinned with
//! `--gps`, else the bridge's GPS receiver, else a GPS found on its own, else
//! `--lat`/`--lon`, else nothing.
//!
//! It also transmits, and without being asked: the planner partitions the pool
//! across the fleet and re-cuts it as the fleet or the pool changes. That is the only thing
//! that decides what a node scans, and nothing on the command line or at the
//! keyboard overrides it. The pool is the bridge's, which reports it in each `Status`;
//! the settings modal only asks the bridge to change it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{capture, config, tui, upload};
use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::Args as ClapArgs;
use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::remember::BridgeMemory;
use wartui_core::engine::{EngineConfig, FleetEngine, StoreStats};
use wartui_core::gps::{Gps, GpsConfig};
use wartui_core::position::PositionChain;
use wartui_core::runtime::{COMMAND_QUEUE, drive, now};
use wartui_core::store::{CaptureInfo, CaptureProvenance, Store, StoreConfig, StoreError};
use wartui_core::ubx;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted. A board named here that answers is remembered as the bridge.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

    /// Read settings from this file instead of the default location
    /// (`~/.config/wartui/wartui.toml`, or `~/Library/Application Support/wartui/wartui.toml`
    /// on macOS). See `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(
        long,
        value_name = "NODES",
        num_args = 0..=1,
        default_missing_value = "3",
        value_parser = crate::sim_nodes()
    )]
    sim: Option<u8>,

    /// Where to keep the capture. A new `wartui-YYYY-MM-DD-HH-MM-SS.db` in the
    /// working directory, named for the second this run started, by default.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Serial port of an NMEA GPS, or `bridge` to read only the bridge's receiver and
    /// open no host port. One is searched for when this is omitted, and whichever is
    /// read is preferred over a `--lat`/`--lon` test position while its fix is recent.
    #[arg(long, value_name = "PATH|bridge", conflicts_with = "no_gps")]
    gps: Option<String>,

    /// Read no receiver at all: neither a host GPS nor the bridge's.
    #[arg(long)]
    no_gps: bool,

    /// Line rate of the GPS. Detected when omitted, by trying each of 9600, 38400,
    /// 4800 and 115200 until one produces valid sentences.
    #[arg(long, value_name = "BAUD")]
    gps_baud: Option<u32>,

    /// How old a GPS fix may be before the position falls back to a `--lat`/`--lon`
    /// test position, or to none.
    #[arg(long, value_name = "SECONDS", default_value_t = 5)]
    gps_max_age: u64,

    /// Latitude to record against every observation. For testing only: a capture
    /// given a fixed position is marked as test data and never uploaded.
    #[arg(long, requires = "lon", allow_hyphen_values = true)]
    lat: Option<f64>,

    /// Longitude to record against every observation. For testing only, as `--lat`.
    #[arg(long, requires = "lat", allow_hyphen_values = true)]
    lon: Option<f64>,

    /// Altitude in meters, recorded alongside a fixed test position.
    #[arg(long, allow_hyphen_values = true)]
    alt: Option<f64>,

    /// Also keep the undecoded bytes of every frame.
    #[arg(long)]
    record_raw: bool,

    /// A note about this run, stored with the capture.
    #[arg(long)]
    notes: Option<String>,
}

/// Which receivers a run reads, from `--no-gps`, `--gps` and `--gps-baud`.
#[derive(Debug, PartialEq, Eq)]
enum GpsChoice {
    /// `--no-gps`: neither the host's nor the bridge's.
    Off,
    /// The bridge's, and a search of the host's ports.
    Search,
    /// The bridge's, and this host port.
    Port(String),
    /// `--gps bridge`: the bridge's only; no host port is opened.
    Bridge,
}

fn gps_choice(no_gps: bool, gps: Option<&str>, baud: Option<u32>) -> Result<GpsChoice> {
    Ok(match gps {
        _ if no_gps => GpsChoice::Off,
        Some("bridge") if baud.is_some() => {
            bail!("--gps-baud sets a host port's line rate; --gps bridge opens none")
        }
        Some("bridge") => GpsChoice::Bridge,
        Some(port) => GpsChoice::Port(port.to_owned()),
        None => GpsChoice::Search,
    })
}

/// Whether the capture holds test data: invented networks or a fixed position.
fn test_data(sim: Option<u8>, lat: Option<f64>) -> bool {
    sim.is_some() || lat.is_some()
}

pub async fn run(args: Args) -> Result<()> {
    // A broken `wartui.toml` stops the run, naming the file and the problem.
    let config = config::load(args.config.as_deref())?;
    // Where the settings modal saves to: the same file `load` just read.
    let config_path = config::path(args.config.as_deref());
    // A first run gets a file holding the empty API key, so there is a place to paste
    // one. Not under `--sim`, which leaves no files behind, and a directory that cannot
    // be written is no reason to stop a capture.
    if let Some(path) = &config_path
        && args.sim.is_none()
        && let Err(error) = config::create_if_missing(path)
    {
        eprintln!("warning: could not create {}: {error:#}", path.display());
    }

    // Whether the capture was named by hand decides what the parting line can tell them
    // to type: a dated name is the newest in this directory, which is what `export`
    // finds on its own, and one chosen by hand has to be given back.
    let named_db = args.db.is_some();
    let choice = gps_choice(args.no_gps, args.gps.as_deref(), args.gps_baud)?;
    let db = args.db.unwrap_or_else(|| capture::dated_path(Local::now()));
    // So a run doomed to be refused never transmits; `Store::create` is the atomic guard.
    if db.exists() {
        return Err(StoreError::Exists(db).into());
    }

    let position = match (args.lat, args.lon) {
        (Some(lat), Some(lon)) => PositionChain::fixed(lat, lon, args.alt),
        (None, None) => PositionChain::empty(),
        // `requires` should have caught this, but a position half-given is
        // worse than none: it would silently record a wrong place.
        _ => bail!("--lat and --lon must be given together"),
    };

    // The receiver is started before the link so that the first observations
    // of the capture have a chance of being positioned; it takes a few seconds
    // to answer, and nothing waits for it either way.
    // Not under `--sim` unless a receiver was named. A simulated fleet is the way to
    // work on wartui with nothing plugged in, and a search that opens every serial
    // port on a developer's machine four times over — resetting whatever is wired for
    // auto-reset on DTR — is not what that command is for. Naming one with `--gps` is
    // still honored: driving the simulator against a real receiver is how the
    // position tier gets exercised without a fleet.
    let looking = match &choice {
        GpsChoice::Search => args.sim.is_none(),
        GpsChoice::Port(_) => true,
        GpsChoice::Off | GpsChoice::Bridge => false,
    };
    let max_age = Duration::from_secs(args.gps_max_age);
    let gps = looking.then(|| {
        let config = match &choice {
            GpsChoice::Port(port) => GpsConfig::pinned(port),
            // A `--bridge` given as a path is opened as given, whatever it is, so
            // the search is told to leave it alone. The fleet's boards are already
            // outside the search: `ports::could_be_a_receiver` leaves out
            // `ports::is_wartui_board`, so neither a node nor the bridge is ever read as
            // a receiver.
            _ => GpsConfig::search().reserving(args.bridge.iter().cloned().collect()),
        };
        let config = match args.gps_baud {
            Some(baud) => config.at_baud(baud),
            None => config,
        };
        Gps::spawn(GpsConfig { max_age, ..config })
    });
    // Built whatever the bridge turns out to be, simulated included: a bridge without a
    // receiver sends no bytes, and this one then says nothing.
    let bridge_gps = (choice != GpsChoice::Off).then(|| Gps::bridged(max_age));
    let mut position = position.with_max_age(max_age);
    if let Some(gps) = &gps {
        position = position.with_gps(gps.clone());
    }
    if let Some(gps) = &bridge_gps {
        position = position.with_bridge_gps(gps.clone());
    }

    // Seeded from the file, and saved back by the view when the transport changes it. The
    // simulator never changes it, so a simulated run leaves `[bridge]` as it found it.
    let memory = BridgeMemory::holding(config.bridge.mac).on_port(config.bridge.serial.clone());
    memory.set_enabled(config.bridge.remember.unwrap_or(true));
    let link = crate::open(args.bridge.as_deref(), args.sim, memory.clone())?;

    let started = now();
    let info = CaptureInfo { notes: args.notes.clone(), simulated: test_data(args.sim, args.lat) };
    let store_config = StoreConfig::new(&db);

    let gps_rate_hz = config.gps.bridge_rate_hz.unwrap_or(ubx::DEFAULT_RATE_HZ);
    let excluded_ssids = config::excluded_ssid_bytes(&config.excluded_ssids).collect();
    let upload = tui::UploadTarget { db: db.clone(), base: upload::BASE.to_owned() };
    let settings = tui::Settings {
        config_path,
        saved: config,
        bridge_memory: memory,
        simulated: args.sim.is_some(),
        upload,
    };
    let config = EngineConfig {
        record_raw: args.record_raw,
        position,
        excluded_ssids,
        gps_rate_hz,
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);
    let provenance = CaptureProvenance {
        host_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        host_commit: option_env!("WARTUI_HOST_COMMIT").map(str::to_owned),
        release_tag: option_env!("WARTUI_RELEASE_TAG").map(str::to_owned),
        initial_settings: Some(engine.capture_settings()),
    };
    let store = Store::create_with_provenance(&store_config, &info, started.unix_ms, &provenance)
        .map_err(|e| match e {
        // Already names the path and says what to do.
        StoreError::Exists(_) => anyhow::Error::new(e),
        e => anyhow::Error::new(e).context(format!("creating {}", db.display())),
    })?;

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);

    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    let outcome = tui::run(snapshot_rx, command_tx, stop_tx, settings).await;
    // Always waited on, even when the view failed: this is what commits the
    // last batch and writes the capture's end time.
    capture.await.context("the capture task panicked")?;
    if let Some(gps) = &gps {
        // Worth having even though the reader's timeout is short: a thread left
        // reading a port the next run wants to open is a confusing failure.
        gps.stop();
    }

    outcome?;
    println!("Capture written to {}", db.display());
    if info.simulated {
        println!(
            "It holds test data (made with --sim or --lat/--lon), so it is never uploaded. \n\
             Do not submit its export to WiGLE, WDGWars or any other service."
        );
    }
    // Without a GPS fix the capture cannot be uploaded.
    let counters = |gps: &Option<Gps>| gps.as_ref().map(|gps| gps.view().counters);
    let bridge_counters = counters(&bridge_gps).unwrap_or_default();
    let fixes = counters(&gps).unwrap_or_default().fixes + bridge_counters.fixes;
    if args.lat.is_none() && fixes == 0 {
        // The view says so throughout as well; this is for a terminal that never
        // came up, and so the last thing on screen says why the export is empty.
        let found = gps.as_ref().and_then(|gps| gps.view().settled);
        let bridge_heard = bridge_counters.sentences > 0;
        match (&choice, bridge_heard, found) {
            (GpsChoice::Off, ..) => println!(
                "No position was given, so nothing in it can be uploaded. Drop --no-gps to \n\
                 look for a receiver."
            ),
            // Both receivers talked; neither fixed.
            (_, true, Some((port, baud))) => println!(
                "Neither the bridge's GPS receiver nor the one on {port} at {baud} reported a \n\
                 fix, so nothing in this capture can be uploaded. Check that they can see the sky."
            ),
            // The bridge's receiver talked; no fix.
            (_, true, None) => println!(
                "The bridge's GPS receiver never reported a fix, so nothing in this capture \n\
                 can be uploaded. Check that it can see the sky."
            ),
            (GpsChoice::Bridge, false, _) => println!(
                "Nothing came from a GPS receiver on the bridge, so nothing in this capture \n\
                 can be uploaded. Check that the bridge has one, or leave --gps off and let \n\
                 wartui look on this host."
            ),
            // Receiver found; no fix.
            (_, _, Some((port, baud))) => println!(
                "The receiver on {port} at {baud} never reported a fix, so nothing in this \n\
                 capture can be uploaded. Check that it can see the sky."
            ),
            // No receiver found at specified port
            (GpsChoice::Port(port), _, None) => println!(
                "Nothing on {port} answered as an NMEA receiver, so nothing in this capture \n\
                 can be uploaded. Check the path, or leave --gps off and let wartui look."
            ),
            (GpsChoice::Search, _, None) => println!(
                "No receiver was found and no position was given, so nothing in this capture \n\
                 can be uploaded. Attach a receiver and run again."
            ),
        }
    }
    if named_db {
        println!("Export it with: wartui export --db {}", db.display());
    } else {
        println!("Export it with: wartui export");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{GpsChoice, gps_choice, test_data};

    #[test]
    fn run_cmd_reads_only_bridge_receiver_when_gps_is_bridge() {
        assert_eq!(gps_choice(false, Some("bridge"), None).unwrap(), GpsChoice::Bridge);
    }

    #[test]
    fn run_cmd_refuses_gps_baud_when_gps_is_bridge() {
        assert!(gps_choice(false, Some("bridge"), Some(9600)).is_err());
    }

    #[test]
    fn run_cmd_pins_host_port_when_gps_names_one() {
        assert_eq!(
            gps_choice(false, Some("/dev/ttyACM0"), Some(9600)).unwrap(),
            GpsChoice::Port("/dev/ttyACM0".to_owned())
        );
    }

    #[test]
    fn run_cmd_searches_when_gps_is_omitted() {
        assert_eq!(gps_choice(false, None, None).unwrap(), GpsChoice::Search);
    }

    #[test]
    fn run_cmd_reads_no_receiver_when_no_gps() {
        assert_eq!(gps_choice(true, None, None).unwrap(), GpsChoice::Off);
    }

    #[test]
    fn run_cmd_marks_test_data_when_simulated() {
        assert!(test_data(Some(3), None));
    }

    #[test]
    fn run_cmd_marks_test_data_when_position_fixed() {
        assert!(test_data(None, Some(37.0)));
    }

    #[test]
    fn run_cmd_marks_test_data_when_simulated_with_position_fixed() {
        assert!(test_data(Some(3), Some(37.0)));
    }

    #[test]
    fn run_cmd_leaves_capture_unmarked_when_neither_given() {
        assert!(!test_data(None, None));
    }
}
