//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard — each row stamped
//! with wherever the host believed it was at that moment: a GPS, found on its own
//! or pinned with `--gps`, else `--lat`/`--lon`, else nothing.
//!
//! It also transmits, and without being asked: the planner partitions the pool
//! across the fleet and re-cuts it as the fleet changes. That is the only thing
//! that decides what a node scans, and nothing on the command line or at the
//! keyboard overrides it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use chrono::Local;
use clap::{Args as ClapArgs, ValueEnum};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{EngineConfig, FleetEngine, StoreStats};
use wartui_core::gps::{Gps, GpsConfig};
use wartui_core::position::PositionChain;
use wartui_core::runtime::{COMMAND_QUEUE, drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig};
use wartui_proto::plan::ChannelPool;

use crate::{capture, tui};

/// Which channels the fleet is meant to scan.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum PoolArg {
    /// FCC-permitted unlicensed channels: 2.4 GHz 1–11 and 5 GHz 36–165.
    Us,
    /// ETSI-permitted unlicensed channels: 2.4 GHz 1–13 and 5 GHz 36–140.
    Eu,
    /// Every channel the node firmware knows about, matching stock behaviour.
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

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The bridge, as a device path or as the board's address. Detected if omitted.
    #[arg(long, value_name = "PATH|MAC")]
    pub(crate) bridge: Option<String>,

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

    /// Which channels the fleet should scan. Recorded with the session, and
    /// the set the planner partitions across it.
    #[arg(long, value_enum, default_value_t = PoolArg::All)]
    pool: PoolArg,

    /// The mesh's ESP-NOW control channel.
    #[arg(long, default_value_t = 6)]
    channel: u8,

    /// Serial port of an NMEA GPS. One is searched for when this is omitted, and
    /// whichever is read is preferred over `--lat`/`--lon` while its fix is recent.
    #[arg(long, value_name = "PATH", conflicts_with = "no_gps")]
    gps: Option<String>,

    /// Do not look for a receiver at all.
    #[arg(long)]
    no_gps: bool,

    /// Line rate of the GPS. Detected when omitted, by trying each of 9600, 38400,
    /// 4800 and 115200 until one produces valid sentences.
    #[arg(long, value_name = "BAUD")]
    gps_baud: Option<u32>,

    /// How old a GPS fix may be before the position falls back to `--lat`.
    #[arg(long, value_name = "SECONDS", default_value_t = 5)]
    gps_max_age: u64,

    /// Latitude to record against every observation.
    #[arg(long, requires = "lon", allow_hyphen_values = true)]
    lat: Option<f64>,

    /// Longitude to record against every observation.
    #[arg(long, requires = "lat", allow_hyphen_values = true)]
    lon: Option<f64>,

    /// Altitude in metres, recorded alongside a static position.
    #[arg(long, allow_hyphen_values = true)]
    alt: Option<f64>,

    /// Also keep the undecoded bytes of every frame.
    #[arg(long)]
    record_raw: bool,

    /// Commit to the store at least this often, in milliseconds; 1000 by default. A crash
    /// loses at most this much of the capture, plus whatever is still queued.
    #[arg(long, value_name = "MS")]
    commit_interval: Option<u64>,

    /// A note about this run, stored with the session.
    #[arg(long)]
    notes: Option<String>,
}

pub async fn run(args: Args) -> Result<()> {
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
    // still honoured: driving the simulator against a real receiver is how the
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
        Gps::spawn(config)
    });
    let position = match &gps {
        Some(gps) => position.with_gps(gps.clone(), Duration::from_secs(args.gps_max_age)),
        None => position,
    };

    let pool: ChannelPool = args.pool.into();
    let link = crate::open(args.bridge.as_deref(), args.sim, args.sim_c6)?;

    let started = now();
    // Whether the capture was named by hand decides what the parting line can tell them
    // to type: a dated name is the newest in this directory, which is what `export`
    // finds on its own, and one chosen by hand has to be given back.
    let named_db = args.db.is_some();
    let db = args.db.unwrap_or_else(|| capture::dated_path(Local::now()));
    let session = SessionInfo { espnow_channel: args.channel, pool, notes: args.notes.clone() };
    let mut store_config = StoreConfig::new(&db);
    if let Some(ms) = args.commit_interval {
        // At least a millisecond: a zero wait would spin the writer whenever it is idle.
        store_config.batch_interval = Duration::from_millis(ms.max(1));
    }
    let store = Store::open(&store_config, &session, started.unix_ms)
        .with_context(|| format!("opening {}", db.display()))?;

    let config = EngineConfig {
        pool,
        record_raw: args.record_raw,
        position,
        // Epochs continue from wherever this database left off. Reusing one a
        // node already holds would be ignored on the air and acknowledged
        // anyway, which is indistinguishable from success.
        assignment_base: store.assignment_base(),
        ..Default::default()
    };
    let engine = FleetEngine::new(config, started);

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);

    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    let outcome = tui::run(snapshot_rx, command_tx, stop_tx).await;
    // Always waited on, even when the view failed: this is what commits the
    // last batch and writes the session's end time.
    capture.await.context("the capture task panicked")?;
    if let Some(gps) = &gps {
        // Worth having even though the reader's timeout is short: a thread left
        // reading a port the next run wants to open is a confusing failure.
        gps.stop();
    }

    outcome?;
    println!("Capture written to {}", db.display());
    // A receiver asked for and never answered leaves as unusable a capture as no
    // position at all, so what matters is whether a fix landed.
    let fixes = gps.as_ref().map_or(0, |gps| gps.view().counters.fixes);
    if args.lat.is_none() && fixes == 0 {
        // The view says so throughout as well; this is for a terminal that never
        // came up, and so the last thing on screen says why the export is empty.
        let found = gps.as_ref().and_then(|gps| gps.view().settled);
        match (args.no_gps, &args.gps, found) {
            (true, _, _) => println!(
                "No position was given, so nothing in it can go to WiGLE. Drop --no-gps to \n\
                 look for a receiver, or run again with --lat and --lon."
            ),
            // One was read and never got a fix. Naming it is the useful half: the
            // rate is already known to be right, so the sky is what is left.
            (_, _, Some((port, baud))) => println!(
                "The receiver on {port} at {baud} never reported a fix, so nothing in this \n\
                 capture can go to WiGLE. Check that it can see the sky, and run with --lat \n\
                 and --lon as well so a run like this still has a position."
            ),
            // Named and never found: the port is the operator's to check.
            (_, Some(port), None) => println!(
                "Nothing on {port} answered as an NMEA receiver, so nothing in this capture \n\
                 can go to WiGLE. Check the path, or leave --gps off and let wartui look."
            ),
            (_, None, None) => println!(
                "No receiver was found and no position was given, so nothing in it can go to \n\
                 WiGLE. Attach a receiver, or run again with --lat and --lon."
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
