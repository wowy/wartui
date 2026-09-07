//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard — each row stamped
//! with wherever the host believed it was at that moment: a GPS on
//! `--gps`, else `--lat`/`--lon`, else nothing.
//!
//! It also transmits, and by default without being asked: the planner
//! partitions the pool across the fleet and re-cuts it as the fleet changes,
//! which is wartui at its full job of replacing the mesh's core. `p` takes
//! that back and `a`/`A` then assign the selected node a range by hand.
//! `--manual` starts with the planner off, and nothing reaches the air until a
//! key is pressed.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{EngineConfig, FleetEngine, StoreStats};
use wartui_core::gps::{Gps, GpsConfig};
use wartui_core::position::PositionChain;
use wartui_core::runtime::{COMMAND_QUEUE, drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig};
use wartui_proto::plan::ChannelPool;

use crate::tui;

/// Which channels the fleet is meant to scan.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum PoolArg {
    /// FCC-permitted unlicensed channels: 2.4 GHz 1–11 and 5 GHz 36–165.
    #[default]
    Us,
    /// Every channel the node firmware knows about, matching stock behaviour.
    All,
}

impl From<PoolArg> for ChannelPool {
    fn from(arg: PoolArg) -> Self {
        match arg {
            PoolArg::Us => Self::Us,
            PoolArg::All => Self::All,
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Serial port of the bridge. Discovered automatically if omitted.
    #[arg(long, value_name = "PATH")]
    port: Option<String>,

    /// Use the built-in simulator with this many fake nodes instead of hardware.
    #[arg(long, value_name = "NODES", num_args = 0..=1, default_missing_value = "3")]
    sim: Option<u8>,

    /// Make this many of the simulated nodes ESP32-C6s, which have no 5 GHz
    /// radio. Counted from the end of the fleet.
    #[arg(long, value_name = "NODES", default_value_t = 0, requires = "sim")]
    sim_c6: u8,

    /// Where to keep the capture.
    #[arg(long, value_name = "PATH", default_value = "wartui.db")]
    db: PathBuf,

    /// Which channels the fleet should scan. Recorded with the session, and
    /// the set the view's assignment keys choose from.
    #[arg(long, value_enum, default_value_t = PoolArg::Us)]
    pool: PoolArg,

    /// Do not partition the pool across the fleet. Channel ranges are then
    /// only what `a` and `A` assign by hand, and nothing goes out unasked.
    /// Toggled in the view either way with `p`.
    #[arg(long, alias = "no-auto")]
    manual: bool,

    /// Accepted and ignored: partitioning the pool is what wartui does unless
    /// `--manual` says otherwise. Kept because it used to be how you asked.
    #[arg(long, hide = true, conflicts_with = "manual")]
    auto: bool,

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
    let link = crate::open(args.port.as_deref(), args.sim, args.sim_c6)?;

    let started = now();
    let session = SessionInfo { espnow_channel: args.channel, pool, notes: args.notes.clone() };
    let store = Store::open(&StoreConfig::new(&args.db), &session, started.unix_ms)
        .with_context(|| format!("opening {}", args.db.display()))?;

    let config = EngineConfig {
        pool,
        auto: !args.manual,
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
        // The reader is blocked on a serial read with a short timeout, so this
        // is the difference between exiting now and exiting a fifth of a
        // second later. Worth having anyway: a thread left reading a port the
        // next run wants to open is a confusing failure.
        gps.stop();
    }

    outcome?;
    println!("Capture written to {}", args.db.display());
    // A receiver that was asked for and never answered leaves exactly as
    // unusable a capture as no position at all, so what matters is whether a
    // fix ever landed, not whether one was configured.
    let fixes = gps.as_ref().map_or(0, |gps| gps.view().counters.fixes);
    if args.lat.is_none() && fixes == 0 {
        // The view says so throughout the run as well; this is for the case
        // where the terminal never came up, and so that the last thing on
        // screen is the reason the export will be empty.
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
    println!("Export it with: wartui export --db {} --wigle out.csv", args.db.display());
    Ok(())
}
