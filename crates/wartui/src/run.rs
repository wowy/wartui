//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard — each row stamped
//! with wherever the host believed it was at that moment: a GPS, found on its own
//! or pinned with `--gps`, else `--lat`/`--lon`, else nothing.
//!
//! It also transmits, and without being asked: the planner partitions the pool
//! across the fleet and re-cuts it as the fleet or the pool changes. That is the only thing
//! that decides what a node scans, and nothing on the command line or at the
//! keyboard overrides it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
use wartui_proto::plan::ChannelPool;
use wartui_proto::tx_power::DEFAULT_TX_POWER_QUARTER_DBM;

use crate::{capture, config, tui, upload};

/// Which channels the fleet is meant to scan.
///
/// Spelled `us`, `eu` and `all` in `wartui.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PoolArg {
    /// FCC-permitted unlicensed channels: 2.4 GHz 1–11 and 5 GHz 36–165.
    Us,
    /// ETSI-permitted unlicensed channels: 2.4 GHz 1–13 and 5 GHz 36–140.
    Eu,
    /// Every channel the node firmware knows about, matching stock behavior.
    #[default]
    All,
}

impl From<PoolArg> for ChannelPool {
    fn from(arg: PoolArg) -> Self {
        match arg {
            PoolArg::Us => Self::Us,
            PoolArg::Eu => Self::Eu,
            PoolArg::All => Self::All,
        }
    }
}

impl From<ChannelPool> for PoolArg {
    fn from(pool: ChannelPool) -> Self {
        match pool {
            ChannelPool::Us => Self::Us,
            ChannelPool::Eu => Self::Eu,
            ChannelPool::All => Self::All,
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The bridge, as a device path, the board's address, or the end of it (`00:08`). Detected
    /// if omitted. A board named here that answers is remembered as the bridge.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

    /// Read settings from this file instead of the default location
    /// (`~/.config/wartui/wartui.toml`, or `~/Library/Application Support/wartui/wartui.toml`
    /// on macOS). Only `run` and `upload` read it; see `crates/wartui/README.md`
    /// § "Config file".
    #[arg(long, value_name = "PATH")]
    pub(crate) config: Option<PathBuf>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,

    /// Make this many of the simulated nodes ESP32-C6s, which have no 5 GHz
    /// radio. Counted from the end of the fleet.
    #[arg(long, value_name = "NODES", default_value_t = 0, requires = "sim")]
    sim_c6: u8,

    /// Where to keep the capture. A new `wartui-YYYY-MM-DD-HH-MM.db` in the
    /// working directory, named for the minute this run started, by default.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Serial port of an NMEA GPS. One is searched for when this is omitted, and
    /// whichever is read is preferred over a `--lat`/`--lon` test position while its
    /// fix is recent.
    #[arg(long, value_name = "PATH", conflicts_with = "no_gps")]
    gps: Option<String>,

    /// Do not look for a receiver at all.
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

/// The fleet's two transmit powers in ESP-IDF quarter-dBm units: `(nodes, bridge)`.
///
/// Whole dBm in, 2 to 20, which maps exactly onto the 8 to 80 quarter-dBm the host
/// permits; the wire carries quarter-dBm, so the conversion happens once here rather
/// than on either side of it. Nodes and bridge are completely independent, each
/// resolved on its own: the file beats the default.
///
/// ```text
/// nodes  = file.fleet  ∨ default
/// bridge = file.bridge ∨ default
/// ```
fn tx_powers(file: &config::TxPower) -> (i8, i8) {
    let quarter = |dbm: i8| dbm * 4;
    let nodes = file.fleet.map_or(DEFAULT_TX_POWER_QUARTER_DBM, quarter);
    let bridge = file.bridge.map_or(DEFAULT_TX_POWER_QUARTER_DBM, quarter);
    (nodes, bridge)
}

/// Which channel pool the fleet starts on: the file beats the default, as
/// [`tx_powers`] resolves the transmit powers.
fn pool(file: Option<PoolArg>) -> ChannelPool {
    file.unwrap_or_default().into()
}

/// Whether the capture holds test data: invented networks or a fixed position.
fn test_data(sim: Option<u8>, lat: Option<f64>) -> bool {
    sim.is_some() || lat.is_some()
}

pub async fn run(args: Args) -> Result<()> {
    // Only `run` and `upload` read `--config`, so a broken `wartui.toml` cannot stop
    // `ports` / `status` / `reset` / `export` / `sniff` from working.
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
    let looking = !args.no_gps && (args.sim.is_none() || args.gps.is_some());
    let gps = looking.then(|| {
        let config = match &args.gps {
            Some(port) => GpsConfig::pinned(port),
            // A `--bridge` given as a path is opened as given, whatever it is, so
            // the search is told to leave it alone. Espressif boards are already
            // outside the search: `ports::could_be_a_receiver` is the complement of
            // what the bridge sweeps by, which is what keeps a node from being read
            // by one half of wartui while the other half transmits into it.
            None => GpsConfig::search().reserving(args.bridge.iter().cloned().collect()),
        };
        let config = match args.gps_baud {
            Some(baud) => config.at_baud(baud),
            None => config,
        };
        let config = GpsConfig { max_age: Duration::from_secs(args.gps_max_age), ..config };
        Gps::spawn(config)
    });
    let position = match &gps {
        Some(gps) => position.with_gps(gps.clone(), Duration::from_secs(args.gps_max_age)),
        None => position,
    };

    let pool = pool(config.pool);
    // The simulator never touches the real file. Elsewhere the operator decides whether
    // there is one at all, and a `remember = false` written by hand forgets it here.
    let memory = if args.sim.is_some() {
        BridgeMemory::none()
    } else {
        crate::memory(args.bridge.as_deref(), crate::Remember::Named)
    };
    memory.set_enabled(config.bridge.remember.unwrap_or(true));
    let link = crate::open(args.bridge.as_deref(), args.sim, args.sim_c6, memory.clone())?;

    let started = now();
    let info =
        CaptureInfo { pool, notes: args.notes.clone(), simulated: test_data(args.sim, args.lat) };
    let store_config = StoreConfig::new(&db);

    let (tx_power, bridge_tx_power) = tx_powers(&config.tx_power);
    let remember_ble = config.bluetooth.remember.unwrap_or(true);
    let preferred_ble = config.bluetooth.node;
    let upload = tui::UploadTarget { db: db.clone(), base: upload::BASE.to_owned() };
    let settings = tui::Settings { config_path, saved: config, bridge_memory: memory, upload };
    let config = EngineConfig {
        pool,
        record_raw: args.record_raw,
        position,
        tx_power,
        bridge_tx_power,
        remember_ble,
        preferred_ble,
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);
    let provenance = CaptureProvenance {
        host_version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        host_build: Some(env!("WARTUI_BUILD_ID").to_owned()),
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
    let fixes = gps.as_ref().map_or(0, |gps| gps.view().counters.fixes);
    if args.lat.is_none() && fixes == 0 {
        // The view says so throughout as well; this is for a terminal that never
        // came up, and so the last thing on screen says why the export is empty.
        let found = gps.as_ref().and_then(|gps| gps.view().settled);
        match (args.no_gps, &args.gps, found) {
            (true, _, _) => println!(
                "No position was given, so nothing in it can be uploaded. Drop --no-gps to \n\
                 look for a receiver."
            ),
            // Receiver found; no fix.
            (_, _, Some((port, baud))) => println!(
                "The receiver on {port} at {baud} never reported a fix, so nothing in this \n\
                 capture can be uploaded. Check that it can see the sky."
            ),
            // No receiver found at specified port
            (_, Some(port), None) => println!(
                "Nothing on {port} answered as an NMEA receiver, so nothing in this capture \n\
                 can be uploaded. Check the path, or leave --gps off and let wartui look."
            ),
            (_, None, None) => println!(
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
    use super::{PoolArg, pool, test_data, tx_powers};
    use crate::config::TxPower;
    use wartui_proto::plan::ChannelPool;
    use wartui_proto::tx_power::DEFAULT_TX_POWER_QUARTER_DBM;

    #[test]
    fn run_cmd_applies_default_tx_power_when_file_names_none() {
        assert_eq!(
            tx_powers(&TxPower::default()),
            (DEFAULT_TX_POWER_QUARTER_DBM, DEFAULT_TX_POWER_QUARTER_DBM)
        );
    }

    #[test]
    fn run_cmd_leaves_bridge_at_default_tx_power_when_file_names_only_fleet() {
        // Nodes and bridge are independent: a fleet power alone never touches
        // the bridge, which stays at the default.
        let file = TxPower { fleet: Some(10), bridge: None };
        assert_eq!(tx_powers(&file), (40, DEFAULT_TX_POWER_QUARTER_DBM));
    }

    #[test]
    fn run_cmd_leaves_nodes_at_default_tx_power_when_file_names_only_bridge() {
        let file = TxPower { fleet: None, bridge: Some(17) };
        assert_eq!(tx_powers(&file), (DEFAULT_TX_POWER_QUARTER_DBM, 68));
    }

    #[test]
    fn run_cmd_applies_both_file_tx_powers_when_file_names_both() {
        let file = TxPower { fleet: Some(10), bridge: Some(17) };
        assert_eq!(tx_powers(&file), (40, 68));
    }

    #[test]
    fn run_cmd_applies_file_pool_when_file_names_one() {
        assert_eq!(pool(Some(PoolArg::Eu)), ChannelPool::Eu);
    }

    #[test]
    fn run_cmd_applies_default_pool_when_file_names_none() {
        assert_eq!(pool(None), ChannelPool::All);
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
