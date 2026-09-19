//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard — each row stamped
//! with wherever the host believed it was at that moment: a GPS on
//! `--gps`, else `--lat`/`--lon`, else nothing.
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

    /// Serial port of an NMEA GPS, read continuously and preferred over
    /// `--lat`/`--lon` whenever it has a recent fix.
    #[arg(long, value_name = "PATH")]
    gps: Option<String>,

    /// Line rate of the GPS. Most receivers ship at 9600; u-blox modules are
    /// often 38400.
    #[arg(long, value_name = "BAUD", default_value_t = 9600)]
    gps_baud: u32,

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
    let gps = args
        .gps
        .as_ref()
        .map(|port| Gps::spawn(GpsConfig { port: port.clone(), baud: args.gps_baud }));
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
        if args.gps.is_some() {
            println!(
                "The GPS never reported a fix, so nothing in this capture can go to WiGLE.\n\
                 Check --gps-baud, that the receiver can see the sky, and run \n\
                 with --lat and --lon as well so a run like this still has a position."
            );
        } else {
            println!(
                "No position was given, so nothing in it can go to WiGLE. Run again with \n\
                 --gps /dev/cu.your-receiver, or with --lat and --lon."
            );
        }
    }
    if named_db {
        println!("Export it with: wartui export --db {}", db.display());
    } else {
        println!("Export it with: wartui export");
    }
    Ok(())
}
