//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! It listens, it writes rows, and it draws what it heard. It also transmits,
//! but only when asked: `a` and `A` in the view assign the selected node a
//! channel range, `p` hands the fleet to the auto-assignment planner, and
//! nothing else this command does reaches the air. `--auto` starts with the
//! planner already running, which is wartui at its full job of replacing the
//! mesh's core.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_core::engine::{EngineConfig, FleetEngine, StoreStats};
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

    /// Where to keep the capture.
    #[arg(long, value_name = "PATH", default_value = "wartui.db")]
    db: PathBuf,

    /// Which channels the fleet should scan. Recorded with the session, and
    /// the set the view's assignment keys choose from.
    #[arg(long, value_enum, default_value_t = PoolArg::Us)]
    pool: PoolArg,

    /// Partition the pool across the fleet without being asked, re-cutting it
    /// whenever the set of heartbeating nodes changes. Toggled in the view
    /// with `p`.
    #[arg(long)]
    auto: bool,

    /// The mesh's ESP-NOW control channel.
    #[arg(long, default_value_t = 6)]
    channel: u8,

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

    let pool: ChannelPool = args.pool.into();
    let link = crate::open(args.port.as_deref(), args.sim)?;

    let started = now();
    let session = SessionInfo { espnow_channel: args.channel, pool, notes: args.notes.clone() };
    let store = Store::open(&StoreConfig::new(&args.db), &session, started.unix_ms)
        .with_context(|| format!("opening {}", args.db.display()))?;

    let config = EngineConfig {
        pool,
        auto: args.auto,
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

    outcome?;
    println!("Capture written to {}", args.db.display());
    if args.lat.is_none() {
        // The view says so throughout the run as well; this is for the case
        // where the terminal never came up, and so that the last thing on
        // screen is the reason the export will be empty.
        println!(
            "No position was given, so nothing in it can go to WiGLE. Run again with \n\
             --lat and --lon, or wait for the GPS chain in Phase 6."
        );
    }
    println!("Export it with: wartui export --db {} --wigle out.csv", args.db.display());
    Ok(())
}
