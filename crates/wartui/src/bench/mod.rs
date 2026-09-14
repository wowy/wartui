//! `wartui bench` — run the simulated fleet into a store with nothing drawn, and report
//! what it cost the disk.
//!
//! The store's batching and SQLite's settings were never measured on the kind of card a
//! wardriving box boots from, where many small scattered writes cost far more than the
//! same bytes written in a few long runs. Whether a change helps can only be told by
//! measuring on that card, and a real drive is not repeatable. This runs the pipeline
//! `run` does — simulator, engine, store, through [`drive`] — for a fixed time against a
//! seeded world, so two builds or two settings can be compared on one device.
//!
//! The figures are only comparable if the load is the same throughout, so every node is
//! kept busy for the whole measured window, and the report proves it rather than
//! assuming it. Two things would otherwise leave nodes idle:
//!
//! - **The dedup ring.** The simulator's nodes suppress repeats through the same ring the
//!   firmware links, so a node that hears no more networks than the ring holds reports
//!   them once and goes quiet. [`busy_networks`] sizes the neighbourhood from the node
//!   count so the node with the thinnest share still overflows its ring.
//! - **Joining.** A node parks until assigned, and every node that joins re-cuts the
//!   plan for all of them. [`settle`] waits for the whole fleet to hold its assignment
//!   before the clock, the kernel counters and the commit timings start.
//!
//! Every [`SAMPLE`], each node's observation count is compared with the last, and a node
//! that heard nothing in a window is counted in `idle_node_windows`. Anything but zero
//! means the run did not measure what its profile says.
//!
//! Hidden, because it is for judging changes to the store rather than for capturing.

mod io;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args as ClapArgs, ValueEnum};
use tokio::sync::{mpsc, oneshot, watch};
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_core::engine::{Command, EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::position::PositionChain;
use wartui_core::runtime::{COMMAND_QUEUE, drive, now};
use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
use wartui_proto::link::Mac;
use wartui_proto::plan::{ChannelPool, DEDUP_RING, NUM_SCAN_CHANNELS};

/// How often each node's progress is checked during the measured window.
///
/// Longer than the slowest sweep a fleet of one to twenty nodes has, so a node doing its
/// job always reports something inside one.
const SAMPLE: Duration = Duration::from_secs(2);

/// How long the fleet may take to hold its assignments before the run is abandoned.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(120);

/// The pool the benchmark fleet scans.
const POOL: ChannelPool = ChannelPool::Us;

/// A named load, so runs on different machines are comparable.
#[derive(Debug, Clone, Copy, ValueEnum, Default)]
pub enum Profile {
    /// Ten nodes in real time, one of them scanning Bluetooth: a busy street, sustained.
    #[default]
    Drive,
    /// The fleet maximum of twenty nodes in real time, one on Bluetooth, to find where
    /// the store stops keeping up and starts dropping.
    Burst,
}

impl Profile {
    /// Nodes, time scale and Bluetooth chance. The neighbourhood follows from the
    /// node count; see [`busy_networks`].
    const fn load(self) -> (u8, f64, f64) {
        match self {
            Self::Drive => (10, 1.0, 1.0),
            Self::Burst => (20, 1.0, 1.0),
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Drive => "drive",
            Self::Burst => "burst",
        }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// Where to write the benchmark's database. Put it on the card being measured.
    #[arg(long, value_name = "PATH")]
    db: PathBuf,

    /// Delete the database and its `-wal` and `-shm` first. Without this an existing
    /// file is refused, because appending to one measures a different thing.
    #[arg(long)]
    fresh: bool,

    /// How long to measure, in seconds, once the fleet is at work.
    #[arg(long, value_name = "SECONDS", default_value_t = 60)]
    duration: u64,

    /// The load to run. The flags below override its parts.
    #[arg(long, value_enum, default_value_t = Profile::Drive)]
    profile: Profile,

    /// Simulated nodes.
    #[arg(long, value_name = "N")]
    nodes: Option<u8>,

    /// Simulated time scale; 1 is real time.
    #[arg(long, value_name = "X")]
    speed: Option<f64>,

    /// Wi-Fi networks in the simulated neighbourhood. By default, enough that every
    /// node overflows its dedup ring on every sweep.
    #[arg(long, value_name = "N")]
    networks: Option<u16>,

    /// Chance per channel dwell that the Bluetooth node hears an advertiser.
    #[arg(long, value_name = "P")]
    ble_chance: Option<f64>,

    /// Seed for the simulated neighbourhood.
    #[arg(long, value_name = "N")]
    seed: Option<u64>,

    /// Also keep every frame's undecoded bytes, as `run --record-raw` does.
    #[arg(long)]
    record_raw: bool,

    /// Commit at least this often, in milliseconds.
    #[arg(long, value_name = "MS")]
    commit_interval: Option<u64>,

    /// Commit once this many rows are pending.
    #[arg(long, value_name = "ROWS")]
    commit_rows: Option<usize>,

    /// Records that may wait between the engine and the writer before dropping.
    #[arg(long, value_name = "RECORDS")]
    queue_depth: Option<usize>,

    /// SQLite page cache for the writer, in MiB.
    #[arg(long, value_name = "MIB")]
    cache_mib: Option<u32>,

    /// WAL pages before a commit checkpoints.
    #[arg(long, value_name = "PAGES")]
    wal_autocheckpoint: Option<u32>,

    /// Page size for the new database, in bytes.
    #[arg(long, value_name = "BYTES")]
    page_size: Option<u32>,

    /// Print the report as one JSON object, for tabulating repeated runs.
    #[arg(long)]
    json: bool,
}

pub async fn run(args: Args) -> Result<()> {
    make_room(&args.db, args.fresh)?;

    let (nodes, speed, ble_chance) = args.profile.load();
    let nodes = args.nodes.unwrap_or(nodes);
    if nodes == 0 {
        bail!("--nodes must be at least 1");
    }
    let sim = SimConfig {
        node_count: nodes,
        speed: args.speed.unwrap_or(speed),
        wifi_networks: args.networks.unwrap_or_else(|| busy_networks(nodes, POOL)),
        ble_chance: args.ble_chance.unwrap_or(ble_chance),
        seed: args.seed.unwrap_or(SimConfig::default().seed),
        ..SimConfig::default()
    };

    let mut store_config = StoreConfig::new(&args.db);
    if let Some(ms) = args.commit_interval {
        store_config.batch_interval = Duration::from_millis(ms);
    }
    if let Some(rows) = args.commit_rows {
        store_config.batch_rows = rows;
    }
    if let Some(depth) = args.queue_depth {
        store_config.queue_depth = depth;
    }
    store_config.cache_kib = args.cache_mib.map(|mib| mib.saturating_mul(1024));
    store_config.wal_autocheckpoint_pages = args.wal_autocheckpoint;
    store_config.page_size = args.page_size;
    store_config.timings = true;

    // The directory rather than the file, which does not exist yet.
    let dir = args.db.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let device = io::Device::holding(dir);

    let link = SimTransport::new(sim.clone()).start().context("starting the simulator")?;
    let started = now();
    let session = SessionInfo {
        espnow_channel: 6,
        pool: POOL,
        notes: Some(format!("wartui bench, profile {}", args.profile.name())),
    };
    let store = Store::open(&store_config, &session, started.unix_ms)
        .with_context(|| format!("opening {}", args.db.display()))?;
    let engine = FleetEngine::new(
        EngineConfig {
            pool: POOL,
            record_raw: args.record_raw,
            // Somewhere, so every row is exportable and the export timing below is of a
            // real export. Null Island, so nobody mistakes it for a place.
            position: PositionChain::fixed(0.0, 0.0, None),
            assignment_base: store.assignment_base(),
            ..EngineConfig::default()
        },
        started,
    );

    let (snapshot_tx, mut snapshot_rx) =
        watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
    let (stop_tx, stop_rx) = oneshot::channel();
    let (command_tx, command_rx) = mpsc::channel(COMMAND_QUEUE);
    let capture = tokio::spawn(drive(link, store, engine, snapshot_tx, command_rx, stop_rx));
    // Naming a node before it has been heard is allowed, and this one is the only
    // source of addresses that never dedup.
    let ble_node = SimTransport::node_mac(0);
    command_tx
        .send(Command::AssignBle { mac: Some(ble_node) })
        .await
        .context("the capture stopped before it began")?;

    if !args.json {
        eprintln!("waiting for {nodes} nodes to take their assignments ...");
    }
    let warmup = Instant::now();
    let Ok(settled) = tokio::time::timeout(
        SETTLE_TIMEOUT,
        settle(&mut snapshot_rx, usize::from(nodes), ble_node),
    )
    .await
    else {
        let _ = stop_tx.send(());
        let _ = capture.await;
        bail!(
            "the fleet did not settle within {}s; nothing was measured",
            SETTLE_TIMEOUT.as_secs()
        );
    };
    let before = settled.context("the capture stopped while the fleet was joining")?;
    let warmup = warmup.elapsed();

    // Everything from here is the measured window.
    let measured_from = Instant::now();
    let process_before = io::ProcessIo::read();
    let device_before = device.as_ref().and_then(io::Device::stat);
    if !args.json {
        eprintln!("benchmarking for {}s into {} ...", args.duration, args.db.display());
    }

    let busy = watch_the_fleet(&snapshot_rx, &before, Duration::from_secs(args.duration)).await;
    let last = snapshot_rx.borrow().clone();

    let _ = stop_tx.send(());
    // Includes the final flush and the checkpoint SQLite runs when the last connection
    // closes, both of which a real capture pays too.
    let store_report = capture.await.context("the capture task panicked")?;
    let elapsed = measured_from.elapsed();
    drop(command_tx);

    let process =
        process_before.zip(io::ProcessIo::read()).map(|(before, after)| after.since(before));
    // What SQLite handed the kernel may not have reached the card yet. Syncing puts the
    // delayed writeback into this run's count rather than into whatever runs next.
    let device_io = device.as_ref().and_then(|device| {
        let _ = std::process::Command::new("sync").status();
        Some(device.stat()?.since(device_before?))
    });
    let peak_rss = io::peak_rss_kib();

    let db_bytes = std::fs::metadata(&args.db).map(|m| m.len()).ok();
    let wal_bytes = std::fs::metadata(with_suffix(&args.db, "-wal")).map(|m| m.len()).ok();

    // Last, so reading the capture back cannot count against its writing.
    let conn = open_readonly(&args.db).context("reopening the benchmark database")?;
    let observation_rows: i64 =
        conn.query_row("SELECT COUNT(*) FROM observation", [], |r| r.get(0))?;
    let exporting = Instant::now();
    let exported =
        wigle_csv(&conn, ExportFilter::default(), &mut std::io::sink(), env!("CARGO_PKG_VERSION"))?;
    let export_time = exporting.elapsed();

    // The warm-up's commits are left out along with its I/O. A batch that began
    // before the window and committed inside it counts, which is one batch in hundreds.
    let batches: Vec<_> =
        store_report.batches.iter().filter(|b| b.committed_at >= measured_from).collect();
    let rows_written: u64 = batches.iter().map(|b| b.rows as u64).sum();
    let rows_dropped = store_report.dropped.saturating_sub(before.store.dropped);

    let mut report = Report::default();
    report.put("wartui", env!("CARGO_PKG_VERSION"));
    report.put("os", format!("{} {}", std::env::consts::OS, std::env::consts::ARCH));
    report.put("kernel", io::kernel_release());
    report.put("device", device.as_ref().map(|d| d.name.clone()));
    report.put("profile", args.profile.name());
    report.put("duration_s", args.duration);

    report.put("sim_nodes", u64::from(sim.node_count));
    report.put("sim_speed", sim.speed);
    report.put("sim_networks", u64::from(sim.wifi_networks));
    report.put("sim_ble_chance", sim.ble_chance);
    report.put("sim_seed", sim.seed);
    report.put("record_raw", if args.record_raw { "on" } else { "off" });

    report.put("commit_rows", store_config.batch_rows);
    report.put("commit_interval_ms", millis(store_config.batch_interval));
    report.put("queue_depth", store_config.queue_depth);
    report.put("cache_kib", store_config.cache_kib.map(u64::from));
    report.put("wal_autocheckpoint_pages", store_config.wal_autocheckpoint_pages.map(u64::from));
    report.put("page_size", store_config.page_size.map(u64::from));

    let seconds = elapsed.as_secs_f64();
    report.put("warmup_s", warmup.as_secs_f64());
    report.put("elapsed_s", seconds);
    report.put("idle_node_windows", busy.idle_windows);
    report.put("node_obs_per_s_min", ratio(busy.slowest, seconds));
    report.put("node_obs_per_s_max", ratio(busy.fastest, seconds));
    report.put("frames", last.counters.frames.saturating_sub(before.counters.frames));
    report.put(
        "observations_heard",
        last.counters.observations.saturating_sub(before.counters.observations),
    );
    report.put("rows_written", rows_written);
    report.put("rows_dropped", rows_dropped);
    report.put("rows_per_s", ratio(rows_written, seconds));

    let commits = batches.len();
    report.put("commits", commits);
    report.put("commits_per_s", ratio(commits as u64, seconds));
    report.put("rows_per_commit", ratio(rows_written, commits as f64));
    let mut batch_times: Vec<Duration> = batches.iter().map(|b| b.batch).collect();
    let mut commit_times: Vec<Duration> = batches.iter().map(|b| b.commit).collect();
    for (name, times) in [("batch", &mut batch_times), ("commit", &mut commit_times)] {
        times.sort_unstable();
        for (label, p) in [("p50", 50), ("p95", 95), ("p99", 99), ("max", 100)] {
            report.put_owned(format!("{name}_ms_{label}"), percentile(times, p).map(ms));
        }
    }

    report.put("write_syscalls", process.map(|p| p.write_calls));
    report.put("write_mib", process.map(|p| mib(p.written)));
    report.put("storage_write_mib", process.map(|p| mib(p.storage_bytes)));

    report.put("device_writes", device_io.map(|d| d.writes));
    report.put("device_write_mib", device_io.map(|d| mib(d.sectors * 512)));
    report
        .put("device_kib_per_write", device_io.and_then(|d| ratio(d.sectors / 2, d.writes as f64)));
    report.put("device_write_ms", device_io.map(|d| d.write_ms));
    report.put("device_flushes", device_io.and_then(|d| d.flushes));
    report.put("peak_rss_mib", peak_rss.map(|kib| mib(kib * 1024)));

    report.put("db_mib", db_bytes.map(mib));
    report.put("wal_mib", wal_bytes.map(mib));
    report.put("observation_rows", u64::try_from(observation_rows).unwrap_or(0));
    report.put("export_networks", exported.networks);
    report.put("export_ms", ms(export_time));

    if args.json {
        println!("{}", report.json());
    } else {
        print!("{}", report.human());
        println!();
        if busy.idle_windows > 0 {
            println!(
                "WARNING: {} node-windows of {}s heard nothing, so the fleet was not busy \
                 throughout. Raise --networks or lower --nodes.",
                busy.idle_windows,
                SAMPLE.as_secs()
            );
        }
        println!("— is SQLite's default, or a counter this system does not keep.");
        if device_io.is_some() {
            println!(
                "Device counters include every other writer on {}; compare medians of 3+ runs.",
                device.as_ref().map_or("the device", |d| d.name.as_str())
            );
        } else {
            println!("No kernel I/O counters here; they are read from Linux's /proc and /sys.");
        }
    }
    Ok(())
}

/// Enough networks that every node hears more than its dedup ring holds, on every sweep.
///
/// Past that point a node re-reports everything it hears each sweep, not merely the
/// overflow: eviction is oldest-inserted and a node sweeps in the same order every time,
/// so each address it reports evicts exactly the one it will come to next. At or below
/// it, a node reports its share once and falls silent.
///
/// Networks are scattered over the whole scan table but the plan deals only the pool,
/// round-robin, so the node to size for is the one holding the fewest channels. Half as
/// much again as the ring covers a thin draw of networks on that node's channels.
fn busy_networks(nodes: u8, pool: ChannelPool) -> u16 {
    let fewest = usize::from((pool.channel_count() / u16::from(nodes.max(1))).max(1));
    let per_channel = (DEDUP_RING * 3 / 2).div_ceil(fewest);
    u16::try_from(per_channel * usize::from(NUM_SCAN_CHANNELS)).unwrap_or(u16::MAX)
}

/// Wait until the fleet is at work, and return the snapshot it was at work in.
///
/// At work means every node has been heard, holds an acknowledged assignment with
/// nothing waiting to be re-sent, and has reported a sighting; and the Bluetooth node
/// holds its scan. Nodes join one at a time and each join re-cuts every assignment, so
/// nothing short of the whole fleet being quiet about its assignments will do.
async fn settle(
    snapshots: &mut watch::Receiver<Arc<Snapshot>>,
    nodes: usize,
    ble_node: Mac,
) -> Result<Arc<Snapshot>, watch::error::RecvError> {
    let settled = snapshots
        .wait_for(|snapshot| {
            snapshot.nodes.len() == nodes
                && snapshot.nodes.iter().all(|node| {
                    let state = &node.state;
                    !state.dirty
                        && state.observations > 0
                        && state.confirmed.as_ref().is_some_and(|confirmed| {
                            !confirmed.channels.is_empty()
                                && (state.mac != ble_node || confirmed.ble)
                        })
                })
        })
        .await?;
    Ok(Arc::clone(&settled))
}

/// How busy the fleet stayed over the measured window.
#[derive(Debug, Default)]
struct Busy {
    /// Windows of [`SAMPLE`] in which a node reported nothing, summed over nodes.
    idle_windows: u64,
    /// Observations from the least productive node.
    slowest: u64,
    /// Observations from the most productive node.
    fastest: u64,
}

/// Sleep out the measured window, checking every [`SAMPLE`] that each node is still
/// reporting.
async fn watch_the_fleet(
    snapshots: &watch::Receiver<Arc<Snapshot>>,
    start: &Snapshot,
    duration: Duration,
) -> Busy {
    let counts = |snapshot: &Snapshot| -> HashMap<Mac, u64> {
        snapshot.nodes.iter().map(|node| (node.state.mac, node.state.observations)).collect()
    };
    let first = counts(start);
    let mut previous = first.clone();
    let mut busy = Busy::default();

    let deadline = tokio::time::Instant::now() + duration;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            break;
        }
        let step = SAMPLE.min(deadline - now);
        tokio::time::sleep(step).await;
        let current = counts(&snapshots.borrow());
        // A trailing sliver shorter than a sweep would accuse a node doing its job.
        if step == SAMPLE {
            busy.idle_windows += first
                .keys()
                .filter(|&mac| current.get(mac).copied().unwrap_or(0) <= previous[mac])
                .count() as u64;
        }
        previous = current;
    }

    let totals = first.iter().map(|(mac, &from)| previous[mac].saturating_sub(from));
    busy.slowest = totals.clone().min().unwrap_or(0);
    busy.fastest = totals.max().unwrap_or(0);
    busy
}

/// Refuse to reuse a database unless told to, and clear it out when told.
fn make_room(db: &Path, fresh: bool) -> Result<()> {
    if !fresh {
        if db.exists() {
            bail!(
                "{} already exists; pass --fresh to replace it, since a run into a used \
                 file measures a different thing",
                db.display()
            );
        }
        return Ok(());
    }
    for path in [db.to_path_buf(), with_suffix(db, "-wal"), with_suffix(db, "-shm")] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(())
}

/// SQLite's companion files are the database's name with a suffix, not an extension.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// The nearest-rank percentile of an already sorted list.
fn percentile(sorted: &[Duration], p: usize) -> Option<Duration> {
    let rank = (sorted.len() * p).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[allow(clippy::cast_precision_loss)]
fn mib(bytes: u64) -> f64 {
    bytes as f64 / (1024.0 * 1024.0)
}

#[allow(clippy::cast_precision_loss)]
fn ratio(count: u64, per: f64) -> Option<f64> {
    (per > 0.0).then(|| count as f64 / per)
}

/// One benchmark's figures, in order, rendered either for a person or for `jq`.
#[derive(Debug, Default)]
struct Report(Vec<(String, Value)>);

#[derive(Debug)]
enum Value {
    Count(u64),
    Real(f64),
    Text(String),
    Absent,
}

impl From<u64> for Value {
    fn from(n: u64) -> Self {
        Self::Count(n)
    }
}

impl From<usize> for Value {
    fn from(n: usize) -> Self {
        Self::Count(u64::try_from(n).unwrap_or(u64::MAX))
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        if n.is_finite() { Self::Real(n) } else { Self::Absent }
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl<T: Into<Self>> From<Option<T>> for Value {
    fn from(value: Option<T>) -> Self {
        value.map_or(Self::Absent, Into::into)
    }
}

impl Report {
    fn put(&mut self, key: &str, value: impl Into<Value>) {
        self.put_owned(key.to_owned(), value);
    }

    fn put_owned(&mut self, key: String, value: impl Into<Value>) {
        self.0.push((key, value.into()));
    }

    fn human(&self) -> String {
        let width = self.0.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
        let mut out = String::new();
        for (key, value) in &self.0 {
            let value = match value {
                Value::Count(n) => n.to_string(),
                Value::Real(n) => format!("{n:.2}"),
                Value::Text(s) => s.clone(),
                Value::Absent => "—".to_owned(),
            };
            let _ = writeln!(out, "{key:width$}  {value}");
        }
        out
    }

    fn json(&self) -> String {
        let fields: Vec<String> = self
            .0
            .iter()
            .map(|(key, value)| {
                let value = match value {
                    Value::Count(n) => n.to_string(),
                    Value::Real(n) => format!("{n:.3}"),
                    Value::Text(s) => json_string(s),
                    Value::Absent => "null".to_owned(),
                };
                format!("{}:{value}", json_string(key))
            })
            .collect();
        format!("{{{}}}", fields.join(","))
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wartui_proto::plan::{ChannelPool, DEDUP_RING, MAX_NODES, NUM_SCAN_CHANNELS};

    use super::{Report, busy_networks, percentile};

    #[test]
    fn every_node_in_any_fleet_is_given_more_networks_than_its_ring_holds() {
        for pool in [ChannelPool::Us, ChannelPool::All] {
            for nodes in 1..=u8::try_from(MAX_NODES).unwrap() {
                let per_channel =
                    usize::from(busy_networks(nodes, pool)) / usize::from(NUM_SCAN_CHANNELS);
                // Rounded down, as the dealer does to the node with the fewest.
                let fewest = usize::from(pool.channel_count() / u16::from(nodes));
                assert!(
                    per_channel * fewest > DEDUP_RING,
                    "{nodes} nodes on {pool:?}: {per_channel} a channel over {fewest} channels"
                );
            }
        }
    }

    #[test]
    fn percentiles_are_nearest_rank_and_the_hundredth_is_the_slowest() {
        let times: Vec<Duration> = (1..=10).map(Duration::from_millis).collect();
        assert_eq!(percentile(&times, 50), Some(Duration::from_millis(5)));
        assert_eq!(percentile(&times, 95), Some(Duration::from_millis(10)));
        assert_eq!(percentile(&times, 100), Some(Duration::from_millis(10)));
        assert_eq!(percentile(&[], 50), None);
    }

    #[test]
    fn the_json_report_is_one_object_with_nulls_for_what_was_not_measured() {
        let mut report = Report::default();
        report.put("device", Some("mmcblk0p2"));
        report.put("kernel", None::<String>);
        report.put("rows", 12u64);
        report.put("rate", 1.5);
        report.put("note", "say \"hi\"\n");
        // Built rather than typed, so the escape under test is not itself an escape
        // the source file has to survive.
        let newline = format!("{}u000a", '\\');
        assert_eq!(
            report.json(),
            format!(
                r#"{{"device":"mmcblk0p2","kernel":null,"rows":12,"rate":1.500,"note":"say \"hi\"{newline}"}}"#
            )
        );
    }
}
