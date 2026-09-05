//! `wartui run` — capture a fleet into the store and watch it happen.
//!
//! This is the whole of the tool at this phase: it listens, it writes rows, and
//! it draws what it heard. It cannot transmit, so it can be pointed at a fleet
//! that is already doing something useful without changing what that is.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use tokio::sync::{oneshot, watch};
use wartui_core::engine::{EngineConfig, FleetEngine, StoreStats};
use wartui_core::position::PositionChain;
use wartui_core::runtime::{drive, now};
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

    /// Which channels the fleet should scan. Recorded now; enforced from
    /// Phase 4, when wartui is allowed to transmit.
    #[arg(long, value_enum, default_value_t = PoolArg::Us)]
    pool: PoolArg,

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
    if args.lat.is_none() {
        // Said once, up front, rather than discovered at export time when the
        // capture is over and the chance to fix it has gone.
        eprintln!(
            "No position given, so observations will be recorded without one and \
             WiGLE will not accept them. Pass --lat and --lon, or wait for the GPS \
             chain in Phase 6."
        );
    }

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
    let session = SessionInfo {
        espnow_channel: args.channel,
        pool,
        notes: args.notes.clone(),
        ..Default::default()
    };
    let store = Store::open(&StoreConfig::new(&args.db), &session, started.unix_ms)
        .with_context(|| format!("opening {}", args.db.display()))?;

    let config = EngineConfig { pool, record_raw: args.record_raw, position, ..Default::default() };
    let engine = FleetEngine::new(config, started);

    let (snapshot_tx, snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();

    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, stop_rx));
    let outcome = tui::run(snapshot_rx, stop_tx).await;
    // Always waited on, even when the view failed: this is what commits the
    // last batch and writes the session's end time.
    capture.await.context("the capture task panicked")?;

    outcome?;
    println!("Capture written to {}", args.db.display());
    println!("Export it with: wartui export --db {} --wigle out.csv", args.db.display());
    Ok(())
}
