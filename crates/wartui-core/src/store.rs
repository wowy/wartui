//! The observation store: SQLite, one writer thread, batched transactions.
//!
//! SQLite is the system of record and the WiGLE CSV an export from it. A CSV-only capture cannot be
//! re-exported after a decoder fix, queried mid-run, or asked which node saw a network.
//!
//! One thread owns the connection: SQLite serialises writes anyway, so sharing one buys contention,
//! not throughput. Writes are batched to [`StoreConfig::batch_rows`] or
//! [`StoreConfig::batch_interval`], whichever comes first. A commit per row is an fsync per row, a
//! hundred inserts a second instead of a hundred thousand. The interval keeps a quiet fleet's rows
//! from sitting unwritten. The queue is bounded and drops rather than blocks, because a lost
//! observation is one row while a stalled engine misses everything. Drops are counted and shown.
//!
//! A second thread with its own connection copies the WAL back into the file after each commit
//! ([`Checkpoint::Background`]), so no commit waits for the card. `docs/store-io-findings.md` has
//! the measurements behind the batching, the checkpoint and the lack of any index on sightings.
//!
//! A file holds one run. [`Store::create`] refuses a path that exists, so every export, analysis
//! and upload cutoff is about one file, with no run to pick.
//!
//! A capture from another build is somebody else's file, and the version marker cannot say so: it
//! stays at [`SCHEMA_VERSION`] while [`SCHEMA`] changes shape, and queries against another shape
//! fail or misread. So each file also carries [`SCHEMA_FINGERPRINT`], a hash of the schema text,
//! and [`check_version`] refuses a file whose fingerprint is missing or different. Even a
//! whitespace or comment change refuses other builds' files: the safe direction, with no migration
//! to offer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub use rusqlite::Connection;
use rusqlite::{OpenFlags, OptionalExtension, params};
use wartui_proto::air::{RecordKind, WIRE_VERSION};
use wartui_proto::plan::{ChannelPool, SCAN_CHANNELS};

use crate::engine::{StorePeaks, StoreStats};
use crate::record::{CaptureSettings, Record};

/// The schema version this build writes and reads: 1 until wartui 1.0, as
/// [`wartui_proto::air::WIRE_VERSION`] is. It is the lever for the first capture that must be read
/// in an earlier build's terms, and none is before 1.0. Meanwhile [`SCHEMA_FINGERPRINT`] tells
/// builds apart (see the module docs).
pub const SCHEMA_VERSION: i32 = 1;

/// FNV-1a over [`SCHEMA`], stamped into `kv` as `schema_fingerprint` and checked by
/// [`check_version`] on every open. Not `DefaultHasher`, whose output may change between Rust
/// releases, so a toolchain update would refuse every capture.
const SCHEMA_FINGERPRINT: u64 = fnv1a_64(SCHEMA.as_bytes());

/// The `kv` key [`SCHEMA_FINGERPRINT`] is stored under.
const FINGERPRINT_KEY: &str = "schema_fingerprint";

/// 64-bit FNV-1a: stable, and small enough to run at compile time.
const fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        i += 1;
    }
    hash
}

/// The schema, applied to every capture [`Store::create`] makes.
const SCHEMA: &str = r"
-- The run this file holds, and the only one: `id` is always 1.
CREATE TABLE IF NOT EXISTS capture (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  bridge_mac BLOB,
  bridge_chip TEXT,
  bridge_fw TEXT,
  channel_pool TEXT NOT NULL,
  notes TEXT,
  -- 1 for test data: made with --sim (invented networks) or --lat/--lon (a fixed
  -- position). WDGWars and WiGLE ban both, so it is never uploaded.
  simulated INTEGER NOT NULL
);

-- `capabilities` is the node's most recent heartbeat rendered the way the fleet
-- table shows it (`wartui/1.0;5g`). Null means only that nothing but an
-- observation has been heard yet; see `record::NodeSeen`.
CREATE TABLE IF NOT EXISTS node (
  mac BLOB PRIMARY KEY,
  label TEXT,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  pinned_channels INTEGER,
  capabilities TEXT
);

-- `wifi_dropped` and `ble_dropped` are the node's since-boot refusal counts as
-- the frame carried them: raw, so a reboot shows as the value falling. `beat` is
-- the node's since-boot heartbeat count, raw, and wraps at 2^16.
CREATE TABLE IF NOT EXISTS heartbeat (
  id INTEGER PRIMARY KEY,
  node_mac BLOB NOT NULL,
  rx_at INTEGER NOT NULL,
  counter INTEGER NOT NULL,
  epoch INTEGER NOT NULL,
  rssi INTEGER,
  wifi_dropped INTEGER NOT NULL,
  ble_dropped INTEGER NOT NULL,
  beat INTEGER NOT NULL,
  -- Whether the heartbeat arrived live or was replayed from the bridge's backlog.
  live INTEGER NOT NULL,
  admin_sent INTEGER NOT NULL DEFAULT 0,
  admin_acked INTEGER,
  admin_latency_us INTEGER
);

-- One row per transmitted assignment, written when its outcome is known, so
-- the table is append-only and a retry is a second row rather than an update.
-- `counter` is the engine's monotonic epoch and `wire_version` the byte that
-- actually went out; they differ because the wire field is one byte wide.
-- `channels` is the forty-two-bit SCAN_CHANNELS mask the frame carried, stored as the
-- integer it is: the indices are what the wire said, and which channels they name is
-- the scan table of the build that wrote the row. Nothing reads the column back, and
-- nothing rewrites it when the table changes -- a row records what went out.
CREATE TABLE IF NOT EXISTS assignment (
  id INTEGER PRIMARY KEY,
  node_mac BLOB NOT NULL,
  counter INTEGER NOT NULL,
  wire_version INTEGER NOT NULL,
  channels INTEGER NOT NULL,
  ble INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  delivered_at INTEGER,
  outcome TEXT,
  latency_us INTEGER
);
CREATE INDEX IF NOT EXISTS assign_node ON assignment(node_mac, created_at);

-- Holds `schema_fingerprint`, which `check_version` reads.
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS observation (
  id INTEGER PRIMARY KEY,
  node_mac BLOB NOT NULL,
  rx_at INTEGER NOT NULL,
  link_rssi INTEGER,
  bssid BLOB NOT NULL,
  ssid BLOB,
  security TEXT NOT NULL,
  channel INTEGER NOT NULL,
  rssi INTEGER NOT NULL,
  kind TEXT NOT NULL,
  rcoi BLOB,
  mfgr_id INTEGER,
  lat REAL, lon REAL, alt REAL, accuracy REAL,
  pos_source TEXT NOT NULL,
  pos_at INTEGER,
  raw_body BLOB
);
-- Deliberately no unique constraint on bssid: every sighting is kept, with the node
-- that made it and the signal it saw. Deduplicating at ingest would throw away the
-- coverage data. Nor does the table have an index of any kind, though export groups by
-- bssid: `docs/store-io-findings.md` has the measurements that took both of them out.

-- One row per bridge status reply, which the host polls for every few seconds.
-- `rx_count`, `dropped_tx` and `uptime_ms` are the bridge's since-boot counts as
-- the reply carried them: raw, as `heartbeat` keeps its drop counts, so a bridge
-- reboot shows as the values falling. `host_frames` is how many frames this host had
-- read off the link when the reply arrived, taken with `rx_count` so the two pair
-- closely but not exactly: between two rows, the `rx_count` difference less the
-- `host_frames` difference is frames lost on USB plus the bridge's `dropped_tx`, give
-- or take the frames queued behind the reply, which overtakes them.
CREATE TABLE IF NOT EXISTS bridge_status (
  id INTEGER PRIMARY KEY,
  rx_at INTEGER NOT NULL,
  peer_count INTEGER NOT NULL,
  rx_count INTEGER NOT NULL,
  dropped_tx INTEGER NOT NULL,
  uptime_ms INTEGER NOT NULL,
  host_frames INTEGER NOT NULL
);

-- One row at the start, every 5 s, and at shutdown: the host's own state, sampled on a timer of
-- its own so it carries on while the bridge is gone. `frames` through
-- `admin_windows_missed` and `store_written`/`store_dropped` are counts since the
-- capture began. `lag_peak_us`, `store_queue_peak` and `store_commit_peak_us` are the
-- largest since the previous row. `throttled` is the Raspberry Pi firmware's
-- `get_throttled` word, NULL off a Pi; `soc_temp_mc` is thermal zone 0 in milli-degrees
-- Celsius, NULL when unreadable. `battery_mv` and `battery_ma` are the `battery` hwmon's
-- voltage and current as its driver reports them, NULL on a host without one.
CREATE TABLE IF NOT EXISTS host_status (
  id INTEGER PRIMARY KEY,
  at INTEGER NOT NULL,
  frames INTEGER NOT NULL,
  duplicate_batches INTEGER NOT NULL,
  garbled INTEGER NOT NULL,
  undecodable INTEGER NOT NULL,
  incompatible INTEGER NOT NULL,
  foreign_fleet INTEGER NOT NULL,
  foreign_admin INTEGER NOT NULL,
  admin_windows_missed INTEGER NOT NULL,
  lag_peak_us INTEGER NOT NULL,
  store_written INTEGER NOT NULL,
  store_dropped INTEGER NOT NULL,
  store_queue_peak INTEGER NOT NULL,
  store_commit_peak_us INTEGER NOT NULL,
  throttled INTEGER,
  soc_temp_mc INTEGER,
  battery_mv INTEGER,
  battery_ma INTEGER
);

-- One row per gap in a node's batch `seq`. `rx_at` is when the batch after the gap
-- arrived, not when the lost batches were sent. `lost` is `seq - after_seq - 1`
-- modulo 2^16, and only gaps under 1024 are recorded, the rule the live count
-- follows. Summing `lost` per node gives the fleet table's `lost` column.
CREATE TABLE IF NOT EXISTS batch_gap (
  id INTEGER PRIMARY KEY,
  node_mac BLOB NOT NULL,
  rx_at INTEGER NOT NULL,
  after_seq INTEGER NOT NULL,
  seq INTEGER NOT NULL,
  lost INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS raw_frame (
  id INTEGER PRIMARY KEY,
  rx_at INTEGER NOT NULL,
  src BLOB NOT NULL,
  dst BLOB,
  rssi INTEGER,
  bytes BLOB NOT NULL
);

-- One row per upload the site queued. `through_id` is the last observation id the upload
-- covered; the next upload sends only observations stored after it. Ids follow commit order,
-- so the cutoff is exact even when one frame's sightings span two commits. `result` is NULL
-- until known, then 'done', 'failed' or 'unfollowed'. A job the site reported failed imported
-- nothing and is no cutoff.
CREATE TABLE IF NOT EXISTS upload (
  id INTEGER PRIMARY KEY,
  through_id INTEGER NOT NULL,
  uploaded_at INTEGER NOT NULL,
  job_id INTEGER NOT NULL,
  rows INTEGER NOT NULL,
  result TEXT
);
";

/// Why the store could not be opened or written.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite said no.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The writer thread could not be started.
    #[error("could not start the store writer thread: {0}")]
    Spawn(#[source] std::io::Error),
    /// The path is taken. A capture holds one run, so it is never appended to.
    #[error("{} already exists; a capture holds one run, so name a new --db", .0.display())]
    Exists(PathBuf),
    /// The file is there but this user cannot write it.
    #[error("{} cannot be written", .0.display())]
    ReadOnly(PathBuf),
    /// The database file could not be created.
    #[error("could not create the database file: {0}")]
    Create(#[source] std::io::Error),
    /// The database was written by a wartui with a different schema.
    #[error(
        "database schema is v{found}, but this build writes v{ours}; captures are not \
         portable between wartui builds before 1.0 — start a new file"
    )]
    SchemaMismatch {
        /// What the file says.
        found: i32,
        /// What this build writes.
        ours: i32,
    },
    /// The database carries this build's version marker over another build's schema.
    #[error(
        "database schema {} differs from this build's {ours:016x}; the capture was written \
         by a different wartui build, and captures are not portable between wartui builds \
         before 1.0 — start a new file with a different --db path",
        found.map_or_else(|| "has no fingerprint and".to_owned(), |f| format!("{f:016x}"))
    )]
    SchemaDiffers {
        /// The fingerprint the file carries, if it carries one that parses.
        found: Option<u64>,
        /// This build's.
        ours: u64,
    },
}

/// How the store should behave.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Where the database lives.
    pub path: PathBuf,
    /// Commit once this many rows are pending.
    pub batch_rows: usize,
    /// Commit at least this often, so a quiet fleet's rows still land.
    pub batch_interval: Duration,
    /// Queue depth between the engine and the writer.
    pub queue_depth: usize,
    /// The writer's page cache, in KiB. `None` leaves SQLite's default, about 2 MiB.
    pub cache_kib: Option<u32>,
    /// WAL pages before a commit checkpoints it. `None` leaves SQLite's 1000.
    pub wal_autocheckpoint_pages: Option<u32>,
    /// Page size in bytes, taken only by a new file since WAL keeps its size. `None` leaves
    /// SQLite's 4096.
    pub page_size: Option<u32>,
    /// Keep every batch's wall time for [`Store::close`]. Off unless `wartui bench` asks: it grows
    /// per commit.
    pub timings: bool,
    /// When the WAL is copied back into the database file.
    pub checkpoint: Checkpoint,
}

/// When the WAL is copied back into the database file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Checkpoint {
    /// SQLite's own, inside whichever commit takes the WAL past its limit
    /// ([`StoreConfig::wal_autocheckpoint_pages`]), so that commit waits for the card.
    #[default]
    Inline,
    /// A thread of its own with a connection of its own, so the writer does not wait.
    ///
    /// Woken by each commit, at most once every `every`, it copies the WAL back with a `PASSIVE`
    /// checkpoint that runs beside the writer. Timing matters: SQLite rewinds the WAL only when a
    /// writer starts a transaction and finds every frame copied. A pass that finishes before the
    /// next commit keeps the WAL one commit long. On a timer, passes straddle commits, never catch
    /// up, and the WAL only grows (`docs/store-io-findings.md`). If commits still outpace the card,
    /// the file grows anyway. Past `truncate_at` bytes, once a pass has caught up so nothing is
    /// left to copy under the writer's lock, a `TRUNCATE` rewinds it.
    Background {
        /// The least time between passes. Zero is a pass after every commit.
        every: Duration,
        /// The WAL file size in bytes past which to truncate it.
        truncate_at: u64,
    },
}

/// What the background checkpointer did. Empty under [`Checkpoint::Inline`], whose checkpoints sit
/// inside commit timings.
#[derive(Debug, Clone, Default)]
pub struct CheckpointReport {
    /// Every passive pass, in order.
    pub passes: Vec<CheckpointPass>,
    /// Every truncating checkpoint, in order.
    pub truncations: Vec<CheckpointPass>,
}

/// One checkpoint.
#[derive(Debug, Clone, Copy)]
pub struct CheckpointPass {
    /// When it returned, so a benchmark can leave out its warm-up.
    pub finished_at: Instant,
    /// How long it took.
    pub took: Duration,
    /// Frames in the WAL when it ran.
    pub frames: i64,
    /// Frames it had copied into the database file when it returned.
    pub copied: i64,
    /// Whether SQLite said it could not finish, for want of a lock.
    pub busy: bool,
}

impl StoreConfig {
    /// Defaults for a live capture on a microSD card: commit every second or 16,384 rows, queue up
    /// to 16,384 records, checkpoint from its own thread after each commit, and SQLite's cache and
    /// page size.
    ///
    /// Measured on a Raspberry Pi writing a full drive to a card (`docs/store-io-findings.md`). A
    /// commit a second rewrites each touched page once a second, not ten times, and checkpointing
    /// right after lets the writer rewind the WAL. Together they cut bytes written from 2.8× the
    /// database to 2.2×, and the slowest batch from 149 ms to 58 ms. The queue is about 1.4 s of a
    /// drive's rows for about 2 MiB, against that 58 ms. A crash loses at most the uncommitted
    /// second plus the queue. The checkpoint syncs the WAL once a second, so a power cut loses no
    /// more.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            batch_rows: 16_384,
            batch_interval: Duration::from_secs(1),
            queue_depth: 16_384,
            cache_kib: None,
            wal_autocheckpoint_pages: None,
            page_size: None,
            timings: false,
            // A pass after every commit. Truncation is a valve a one-commit WAL never reaches.
            checkpoint: Checkpoint::Background { every: Duration::ZERO, truncate_at: 64 << 20 },
        }
    }
}

/// What the writer did over the store's life, from [`Store::close`]. For `wartui bench`, which
/// judges store changes on the slow cards they matter on. The view needs only [`StoreStats`].
#[derive(Debug, Clone, Default)]
pub struct StoreReport {
    /// Rows written.
    pub written: u64,
    /// Rows dropped, whether by a full queue or a failed batch.
    pub dropped: u64,
    /// Every committed batch, in order, if [`StoreConfig::timings`] asked.
    pub batches: Vec<BatchTiming>,
    /// What the background checkpointer did, if there was one.
    pub checkpoints: CheckpointReport,
}

/// One committed batch, as [`StoreReport`] keeps it.
#[derive(Debug, Clone, Copy)]
pub struct BatchTiming {
    /// When the commit returned, so a benchmark can leave out its warm-up.
    pub committed_at: Instant,
    /// Rows in the batch.
    pub rows: usize,
    /// Statements and commit together.
    pub batch: Duration,
    /// The `COMMIT` alone, where the WAL is written and SQLite runs any automatic checkpoint.
    pub commit: Duration,
}

/// What to record about the capture being created. Not the bridge's identity: a capture is created
/// before any bridge announces itself, so that arrives later as a [`Record::Bridge`].
#[derive(Debug, Clone, Default)]
pub struct CaptureInfo {
    /// The pool the run started on. The operator can change it mid-run, so each assignment row
    /// records the channels actually sent.
    pub pool: ChannelPool,
    /// Anything the operator wants to remember about this run.
    pub notes: Option<String>,
    /// Whether it holds test data: made with `--sim` or `--lat`/`--lon`.
    pub simulated: bool,
}

/// Host identity and applied initial settings, supplied by the executable creating a capture.
///
/// Missing values are unknown, never inferred from a shared package or firmware version. The
/// scan table is recorded independently by the store; it describes the host, not a node build.
#[derive(Debug, Clone, Default)]
pub struct CaptureProvenance {
    /// Executable's package version.
    pub host_version: Option<String>,
    /// Build-time source identity, including local source changes.
    pub host_build: Option<String>,
    /// Release tag, if this is a tagged executable.
    pub release_tag: Option<String>,
    /// Applied settings before the runtime begins, committed atomically with capture metadata.
    pub initial_settings: Option<CaptureSettings>,
}

#[derive(Debug, Default)]
struct Stats {
    written: AtomicU64,
    dropped: AtomicU64,
    /// Records in the queue, counted in by [`Store::offer`] and out by the writer.
    queued: AtomicU64,
    /// The largest `queued` since [`Store::take_peaks`].
    queue_peak: AtomicU64,
    /// The slowest batch since [`Store::take_peaks`], in microseconds.
    commit_peak_us: AtomicU64,
}

/// A handle to the writer thread.
#[derive(Debug)]
pub struct Store {
    tx: Option<SyncSender<Record>>,
    stats: Arc<Stats>,
    join: Option<JoinHandle<StoreReport>>,
    /// The background checkpointer, under [`Checkpoint::Background`]. It stops with the writer,
    /// which holds the only thing that wakes it.
    checkpointer: Option<JoinHandle<CheckpointReport>>,
    /// Passes that caught up, counted live. Not a third field in [`Stats`], so the two threads
    /// writing them do not share a cache line.
    caught_up: Arc<AtomicU64>,
}

impl Store {
    /// Create the database at an unoccupied path and start the writer. `create_new` makes the file
    /// before SQLite sees it, so a taken path, even one another process takes first, is refused.
    ///
    /// # Errors
    /// [`StoreError::Exists`] if the path is taken, otherwise [`StoreError`] if creating the file,
    /// applying the schema or starting the writer fails.
    pub fn create(
        config: &StoreConfig,
        capture: &CaptureInfo,
        started_at_ms: i64,
    ) -> Result<Self, StoreError> {
        Self::create_with_provenance(config, capture, started_at_ms, &CaptureProvenance::default())
    }

    /// Create a capture with durable host provenance in the existing key/value table.
    ///
    /// # Errors
    /// Same as [`Self::create`]. No existing file is altered or migrated.
    pub fn create_with_provenance(
        config: &StoreConfig,
        capture: &CaptureInfo,
        started_at_ms: i64,
        provenance: &CaptureProvenance,
    ) -> Result<Self, StoreError> {
        std::fs::OpenOptions::new().write(true).create_new(true).open(&config.path).map_err(
            |e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => StoreError::Exists(config.path.clone()),
                _ => StoreError::Create(e),
            },
        )?;
        let mut conn = Connection::open(&config.path)?;
        prepare(&conn, config)?;
        // All or nothing. The marker and fingerprint say the tables exist, so a half-applied schema
        // would pass `check_version` and fail on a missing table.
        let tx = conn.transaction()?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.execute(
            "INSERT INTO kv (k, v) VALUES (?1, ?2)",
            params![FINGERPRINT_KEY, format!("{SCHEMA_FINGERPRINT:016x}")],
        )?;
        tx.execute(
            "INSERT INTO capture (id, started_at, channel_pool, notes, simulated) \
             VALUES (1, ?1, ?2, ?3, ?4)",
            params![started_at_ms, pool_name(capture.pool), capture.notes, capture.simulated],
        )?;
        let scan_channels = SCAN_CHANNELS.iter().map(u8::to_string).collect::<Vec<_>>().join(",");
        for (key, value) in [
            ("capture.scan_channels", Some(scan_channels)),
            ("capture.wire_version", Some(WIRE_VERSION.to_string())),
            ("capture.core_version", Some(env!("CARGO_PKG_VERSION").to_owned())),
            ("capture.host_version", provenance.host_version.clone()),
            ("capture.host_build", provenance.host_build.clone()),
            ("capture.release_tag", provenance.release_tag.clone()),
            (
                "capture.settings.initial",
                provenance.initial_settings.map(|settings| settings_text(started_at_ms, settings)),
            ),
        ] {
            if let Some(value) = value {
                tx.execute("INSERT INTO kv (k, v) VALUES (?1, ?2)", params![key, value])?;
            }
        }
        tx.commit()?;

        // One waiting wake-up stands for any number of commits, so the channel holds one.
        let (wake, woken) = match config.checkpoint {
            Checkpoint::Inline => (None, None),
            Checkpoint::Background { .. } => {
                let (wake, woken) = sync_channel(1);
                (Some(wake), Some(woken))
            }
        };

        let (tx, rx) = sync_channel(config.queue_depth);
        let stats = Arc::new(Stats::default());
        let join = std::thread::Builder::new()
            .name("wartui-store".to_owned())
            .spawn({
                let stats = Arc::clone(&stats);
                let config = config.clone();
                move || writer(conn, &rx, &config, &stats, wake.as_ref())
            })
            .map_err(StoreError::Spawn)?;

        let caught_up = Arc::new(AtomicU64::new(0));
        let checkpointer = match (config.checkpoint, woken) {
            (Checkpoint::Background { every, truncate_at }, Some(woken)) => {
                let conn = Connection::open(&config.path)?;
                // Waits out the writer's commit rather than failing a truncation on it.
                conn.busy_timeout(Duration::from_secs(5))?;
                let wal = wal_path(&config.path);
                let counted = Arc::clone(&caught_up);
                let join = std::thread::Builder::new()
                    .name("wartui-checkpoint".to_owned())
                    .spawn(move || checkpointer(&conn, &wal, &woken, every, truncate_at, &counted))
                    .map_err(StoreError::Spawn)?;
                Some(join)
            }
            _ => None,
        };

        Ok(Self { tx: Some(tx), stats, join: Some(join), checkpointer, caught_up })
    }

    /// Queue records, dropping what does not fit rather than waiting. Never blocks. Returns how
    /// many were dropped, also counted into [`Self::stats`].
    pub fn submit(&self, records: Vec<Record>) -> usize {
        let dropped =
            records.into_iter().map(|record| self.offer(record)).filter(|&ok| !ok).count();
        self.count_dropped(dropped as u64);
        dropped
    }

    /// Queue one record if it fits, without counting a refusal as a drop. Never blocks. For a
    /// caller that retries, and counts the record lost only once it gives up
    /// ([`Self::count_dropped`]).
    pub fn offer(&self, record: Record) -> bool {
        let Some(tx) = &self.tx else { return false };
        // Counted in before the send, so the writer's count out never runs ahead and wraps. A
        // record that does not fit is counted back out.
        let ahead = self.stats.queued.fetch_add(1, Ordering::Relaxed);
        if tx.try_send(record).is_ok() {
            self.stats.queue_peak.fetch_max(ahead + 1, Ordering::Relaxed);
            true
        } else {
            self.stats.queued.fetch_sub(1, Ordering::Relaxed);
            false
        }
    }

    /// Count records the caller gave up on into [`Self::stats`].
    pub fn count_dropped(&self, count: u64) {
        if count > 0 {
            self.stats.dropped.fetch_add(count, Ordering::Relaxed);
        }
    }

    /// Rows written and rows dropped so far.
    #[must_use]
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            written: self.stats.written.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
        }
    }

    /// The deepest queue and slowest batch since the last call, both then reset. Peaks, because a
    /// sample every few seconds would rarely land on the moment a slow commit backed the queue up.
    #[must_use]
    pub fn take_peaks(&self) -> StorePeaks {
        StorePeaks {
            queue: self.stats.queue_peak.swap(0, Ordering::Relaxed),
            commit_us: self.stats.commit_peak_us.swap(0, Ordering::Relaxed),
        }
    }

    /// Background checkpoints that have caught up so far, each letting the next commit rewind the
    /// WAL. Counted live because [`StoreReport::checkpoints`] arrives only at close. 0 under
    /// [`Checkpoint::Inline`].
    #[must_use]
    pub fn checkpoints_caught_up(&self) -> u64 {
        self.caught_up.load(Ordering::Relaxed)
    }

    /// Flush the queue, record the capture's end and stop the writer. Explicit, not `Drop`, so a
    /// failed last transaction is reported. Returns what the writer did, for the benchmark.
    pub fn close(mut self) -> StoreReport {
        self.shutdown()
    }

    /// A store with no writer, whose queue only the test drains: a full queue on demand.
    #[cfg(test)]
    pub(crate) fn detached(queue_depth: usize) -> (Self, Receiver<Record>) {
        let (tx, rx) = sync_channel(queue_depth);
        let store = Self {
            tx: Some(tx),
            stats: Arc::new(Stats::default()),
            join: None,
            checkpointer: None,
            caught_up: Arc::new(AtomicU64::new(0)),
        };
        (store, rx)
    }

    fn shutdown(&mut self) -> StoreReport {
        drop(self.tx.take());
        let mut report = match self.join.take().map(JoinHandle::join) {
            Some(Ok(report)) => report,
            Some(Err(_)) => {
                tracing::error!("the store writer thread panicked; the last batch may be lost");
                StoreReport::default()
            }
            None => StoreReport::default(),
        };
        // After the writer, whose end stops it, so the last batch gets a checkpoint.
        if let Some(join) = self.checkpointer.take() {
            match join.join() {
                Ok(checkpoints) => report.checkpoints = checkpoints,
                Err(_) => tracing::error!("the store checkpoint thread panicked"),
            }
        }
        report.written = self.stats.written.load(Ordering::Relaxed);
        report.dropped = self.stats.dropped.load(Ordering::Relaxed);
        report
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Open a second connection for reading, as export does. WAL makes this safe mid-capture: exporting
/// an hour of data cannot stall ingest.
///
/// # Errors
/// [`StoreError`] if the file cannot be opened or is from a different wartui.
pub fn open_readonly(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    check_version(&conn)?;
    Ok(conn)
}

/// Open a second connection that may write, as recording an upload does. Never creates the file.
/// WAL and the busy timeout let it write beside a running capture. SQLite silently opens an
/// unwritable file read-only, so that is checked here.
///
/// # Errors
/// [`StoreError`] if the file cannot be opened or written, or is from a different wartui.
pub fn open_readwrite(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_URI,
    )?;
    if conn.is_readonly(rusqlite::MAIN_DB)? {
        return Err(StoreError::ReadOnly(path.to_owned()));
    }
    conn.busy_timeout(Duration::from_secs(5))?;
    check_version(&conn)?;
    Ok(conn)
}

/// An upload the site queued, as [`last_upload`] reads it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UploadRecord {
    /// The site's job.
    pub job_id: u64,
    /// When the site queued it, in unix milliseconds.
    pub uploaded_at_ms: i64,
    /// Rows the upload sent.
    pub rows: u64,
}

/// Record an upload the site queued as `job_id`, covering observations through id `through_id`.
/// Returns the row's id, for [`set_upload_result`].
///
/// # Errors
/// If the insert fails.
pub fn record_upload(
    conn: &Connection,
    through_id: i64,
    uploaded_at_ms: i64,
    job_id: u64,
    rows: u64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO upload (through_id, uploaded_at, job_id, rows) VALUES (?1, ?2, ?3, ?4)",
        params![through_id, uploaded_at_ms, job_id as i64, rows as i64],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Record how upload `id` ended: `done`, `failed` or `unfollowed`.
///
/// # Errors
/// If the update fails.
pub fn set_upload_result(conn: &Connection, id: i64, result: &str) -> rusqlite::Result<()> {
    conn.execute("UPDATE upload SET result = ?1 WHERE id = ?2", params![result, id])?;
    Ok(())
}

/// Whether this capture holds test data: made with `--sim` or `--lat`/`--lon`.
///
/// # Errors
/// If the query fails.
pub fn is_simulated(conn: &Connection) -> rusqlite::Result<bool> {
    conn.query_row("SELECT simulated FROM capture WHERE id = 1", [], |row| row.get(0))
}

/// The newest upload the site did not report failed, if any.
///
/// # Errors
/// If the query fails.
pub fn last_upload(conn: &Connection) -> rusqlite::Result<Option<UploadRecord>> {
    conn.query_row(
        "SELECT job_id, uploaded_at, rows FROM upload WHERE result IS NOT 'failed' \
         ORDER BY id DESC LIMIT 1",
        [],
        |row| {
            Ok(UploadRecord {
                job_id: row.get::<_, i64>(0)? as u64,
                uploaded_at_ms: row.get(1)?,
                rows: row.get::<_, i64>(2)? as u64,
            })
        },
    )
    .optional()
}

/// Refuse a database written by any wartui but this one. With no migration before 1.0, a lower
/// marker is as foreign as a higher one. A file reading 0 has not had [`Store::create`] commit its
/// schema yet. A file with this build's marker must also carry this build's [`SCHEMA_FINGERPRINT`],
/// and one with none is as foreign.
fn check_version(conn: &Connection) -> Result<(), StoreError> {
    let found: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match found {
        0 => Ok(()),
        SCHEMA_VERSION => match stored_fingerprint(conn)? {
            Some(SCHEMA_FINGERPRINT) => Ok(()),
            found => Err(StoreError::SchemaDiffers { found, ours: SCHEMA_FINGERPRINT }),
        },
        found => Err(StoreError::SchemaMismatch { found, ours: SCHEMA_VERSION }),
    }
}

/// The fingerprint a file carries. `None` without a `kv` table, its row, or a hex `u64` value.
fn stored_fingerprint(conn: &Connection) -> Result<Option<u64>, rusqlite::Error> {
    let has_kv: bool = conn.query_row(
        "SELECT EXISTS (SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'kv')",
        [],
        |row| row.get(0),
    )?;
    if !has_kv {
        return Ok(None);
    }
    let value = conn.query_row("SELECT v FROM kv WHERE k = ?1", [FINGERPRINT_KEY], |row| {
        row.get::<_, String>(0)
    });
    match value {
        Ok(v) => Ok(u64::from_str_radix(&v, 16).ok()),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The background checkpointer's loop, woken by commits until the writer is gone. It always makes
/// one last pass, so a stopped store leaves no WAL for the next open to replay.
fn checkpointer(
    conn: &Connection,
    wal: &Path,
    woken: &Receiver<()>,
    every: Duration,
    truncate_at: u64,
    caught_up_count: &AtomicU64,
) -> CheckpointReport {
    let mut report = CheckpointReport::default();
    let mut last_pass: Option<Instant> = None;
    // Too soon after the last pass, but still owed one, or the last commit before the fleet goes
    // quiet sits uncopied and unsynced, since the writer's own checkpoint is off.
    let mut owed = false;
    loop {
        let stopping = if owed {
            // A commit may wake it early again. The wait resumes for what is left.
            let left =
                last_pass.map_or(Duration::ZERO, |last| every.saturating_sub(last.elapsed()));
            matches!(woken.recv_timeout(left), Err(RecvTimeoutError::Disconnected))
        } else {
            woken.recv().is_err()
        };
        // Put off: a pass straight after the last would copy almost nothing and sync again for it.
        if !stopping && last_pass.is_some_and(|last| last.elapsed() < every) {
            owed = true;
            continue;
        }
        owed = false;
        last_pass = Some(Instant::now());
        let caught_up = match checkpoint(conn, "PASSIVE") {
            Ok(pass) => {
                let caught_up = !pass.busy && pass.copied >= pass.frames;
                report.passes.push(pass);
                caught_up
            }
            Err(e) => {
                tracing::warn!("a background checkpoint failed: {e}");
                false
            }
        };
        if caught_up {
            caught_up_count.fetch_add(1, Ordering::Relaxed);
        }
        // Only once a pass caught up, so the truncation holds the writer's lock only to rewind.
        if caught_up && std::fs::metadata(wal).is_ok_and(|m| m.len() > truncate_at) {
            match checkpoint(conn, "TRUNCATE") {
                Ok(pass) => report.truncations.push(pass),
                Err(e) => tracing::warn!("truncating the WAL failed: {e}"),
            }
        }
        if stopping {
            return report;
        }
    }
}

/// Run one checkpoint in `mode` and say what it did.
fn checkpoint(conn: &Connection, mode: &str) -> Result<CheckpointPass, rusqlite::Error> {
    let started = Instant::now();
    let (busy, frames, copied) =
        conn.query_row(&format!("PRAGMA wal_checkpoint({mode})"), [], |r| {
            Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?))
        })?;
    let finished_at = Instant::now();
    Ok(CheckpointPass { finished_at, took: finished_at - started, frames, copied, busy })
}

/// SQLite's WAL for a database: the name with `-wal` on the end, not an extension.
fn wal_path(db: &Path) -> PathBuf {
    let mut name = db.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn prepare(conn: &Connection, config: &StoreConfig) -> Result<(), rusqlite::Error> {
    // First, because a new file's page size is fixed when its header is written, and
    // switching it to WAL writes the header.
    if let Some(bytes) = config.page_size {
        conn.pragma_update(None, "page_size", bytes)?;
    }
    // WAL so the export's reader and the ingest writer never wait on each other;
    // NORMAL because losing a capture's tail to a power cut beats fsyncing every
    // commit; a busy timeout so a concurrent export backs off instead of erroring.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    if let Some(kib) = config.cache_kib {
        // Negative means KiB, not pages, whatever the page size.
        conn.pragma_update(None, "cache_size", -i64::from(kib))?;
    }
    if matches!(config.checkpoint, Checkpoint::Background { .. }) {
        // The checkpointer's job now; a commit that ran one would wait for the card.
        conn.pragma_update(None, "wal_autocheckpoint", 0)?;
    } else if let Some(pages) = config.wal_autocheckpoint_pages {
        conn.pragma_update(None, "wal_autocheckpoint", pages)?;
    }
    Ok(())
}

/// The pool's name as stored, which is not its name on screen: `ChannelPool`'s `Display` says "US",
/// and the `channel_pool` column says "us".
const fn pool_name(pool: ChannelPool) -> &'static str {
    match pool {
        ChannelPool::Us => "us",
        ChannelPool::Eu => "eu",
        ChannelPool::All => "all",
    }
}

/// Versioned key/value text, with no arbitrary strings needing escaping. Sequence lives in the
/// key so equal or reversed timestamps cannot overwrite a settings change.
fn settings_text(at_ms: i64, settings: CaptureSettings) -> String {
    let mac = |value: Option<wartui_proto::mac::Mac>| {
        value.map_or_else(|| "none".to_owned(), |mac| wartui_proto::mac::full(&mac).to_string())
    };
    format!(
        "version=1;at_ms={at_ms};pool={};nodes_tx_power_quarter_dbm={};\
         bridge_tx_power_quarter_dbm={};record_raw={};gps={};gps_max_age_ms={};\
         remember_ble={};preferred_ble={};ble_node={};topology_timeout_ms={};\
         status_interval_ms={};admin_timeout_ms={}",
        pool_name(settings.pool),
        settings.tx_power,
        settings.bridge_tx_power,
        settings.record_raw,
        settings.gps,
        settings.gps_max_age.as_millis(),
        settings.remember_ble,
        mac(settings.preferred_ble),
        mac(settings.ble_node),
        settings.topology_timeout.as_millis(),
        settings.status_interval.as_millis(),
        settings.admin_timeout.as_millis(),
    )
}

/// The `kind` column's token for `kind`.
pub(crate) const fn kind_name(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Wifi => "wifi",
        RecordKind::Ble => "ble",
    }
}

/// The kind a `kind` column token names, or `None` for a token [`kind_name`] never writes.
pub(crate) fn parse_kind(name: &str) -> Option<RecordKind> {
    match name {
        "wifi" => Some(RecordKind::Wifi),
        "ble" => Some(RecordKind::Ble),
        _ => None,
    }
}

fn writer(
    mut conn: Connection,
    rx: &Receiver<Record>,
    config: &StoreConfig,
    stats: &Stats,
    wake: Option<&SyncSender<()>>,
) -> StoreReport {
    let (batch_rows, batch_interval) = (config.batch_rows, config.batch_interval);
    let mut pending: Vec<Record> = Vec::with_capacity(batch_rows);
    let mut last_flush = Instant::now();
    let mut report = StoreReport::default();
    let mut flush = |conn: &mut Connection, pending: &mut Vec<Record>| {
        let committing = !pending.is_empty();
        flush(conn, pending, stats, config.timings.then_some(&mut report));
        // A full channel already has a wake-up waiting, and that one covers this commit.
        if committing && let Some(wake) = wake {
            let _ = wake.try_send(());
        }
    };

    loop {
        match rx.recv_timeout(batch_interval) {
            Ok(record) => {
                stats.queued.fetch_sub(1, Ordering::Relaxed);
                pending.push(record);
                // Without the row count a burst commits one enormous transaction. Without the
                // elapsed check a trickle below the count sits in memory forever, since the timeout
                // fires only when the queue goes quiet.
                if pending.len() >= batch_rows || last_flush.elapsed() >= batch_interval {
                    flush(&mut conn, &mut pending);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush(&mut conn, &mut pending);
                last_flush = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    flush(&mut conn, &mut pending);
    if let Err(e) = conn.execute(
        "UPDATE capture SET ended_at = ?1 WHERE id = 1",
        params![chrono::Utc::now().timestamp_millis()],
    ) {
        tracing::warn!("could not close out the capture row: {e}");
    }
    report
}

fn flush(
    conn: &mut Connection,
    pending: &mut Vec<Record>,
    stats: &Stats,
    report: Option<&mut StoreReport>,
) {
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    let started = Instant::now();
    match write_batch(conn, pending) {
        Ok(commit) => {
            let batch_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX);
            stats.commit_peak_us.fetch_max(batch_us, Ordering::Relaxed);
            if let Some(report) = report {
                report.batches.push(BatchTiming {
                    committed_at: Instant::now(),
                    rows: count,
                    batch: started.elapsed(),
                    commit,
                });
            }
            stats.written.fetch_add(count as u64, Ordering::Relaxed);
        }
        Err(e) => {
            // Retrying forever behind a filling queue would turn a full disk into a wedged capture.
            stats.dropped.fetch_add(count as u64, Ordering::Relaxed);
            tracing::error!("dropping {count} rows: {e}");
        }
    }
    pending.clear();
}

/// A count as SQLite's signed integer, saturating: no count here comes near the limit.
fn column(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

/// Write one batch in one transaction, returning how long the commit alone took.
fn write_batch(conn: &mut Connection, pending: &[Record]) -> Result<Duration, rusqlite::Error> {
    let tx = conn.transaction()?;
    for record in pending {
        match record {
            Record::Node(node) => {
                tx.prepare_cached(
                    "INSERT INTO node (mac, first_seen, last_seen, capabilities)
                       VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(mac) DO UPDATE SET
                       last_seen = excluded.last_seen,
                       -- Most frames are observations with no token, so writing `excluded` would
                       -- erase the last heartbeat's token at once. The column is the last token
                       -- ever seen: a board reflashed to stock keeps it here all capture, while the
                       -- engine, re-reading each heartbeat, stops believing it.
                       capabilities = coalesce(excluded.capabilities, node.capabilities)",
                )?
                .execute(params![
                    &node.mac[..],
                    node.first_seen_ms,
                    node.last_seen_ms,
                    node.capabilities
                ])?;
            }
            Record::CaptureSettings { sequence, at_ms, settings } => {
                tx.prepare_cached("INSERT INTO kv (k, v) VALUES (?1, ?2)")?.execute(params![
                    format!("capture.settings.{sequence:020}"),
                    settings_text(*at_ms, *settings),
                ])?;
            }
            Record::Heartbeat(hb) => {
                tx.prepare_cached(
                    "INSERT INTO heartbeat
                       (node_mac, rx_at, counter, epoch, rssi, wifi_dropped, ble_dropped,
                        beat, live)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                )?
                .execute(params![
                    &hb.node_mac[..],
                    hb.rx_at_ms,
                    hb.counter,
                    hb.epoch,
                    hb.link_rssi,
                    hb.wifi_dropped,
                    hb.ble_dropped,
                    hb.beat,
                    hb.live
                ])?;
            }
            Record::Observation(obs) => {
                tx.prepare_cached(
                    "INSERT INTO observation
                       (node_mac, rx_at, link_rssi, bssid, ssid, security, channel, rssi,
                        kind, rcoi, mfgr_id, lat, lon, alt, accuracy, pos_source, pos_at,
                        raw_body)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                )?
                .execute(params![
                    &obs.node_mac[..],
                    obs.rx_at_ms,
                    obs.link_rssi,
                    &obs.bssid[..],
                    obs.ssid,
                    obs.security,
                    obs.channel,
                    obs.rssi,
                    kind_name(obs.kind),
                    obs.rcoi,
                    obs.mfgr_id,
                    obs.fix.lat,
                    obs.fix.lon,
                    obs.fix.alt,
                    obs.fix.accuracy,
                    obs.fix.source.as_str(),
                    obs.fix.at_ms,
                    obs.raw_body,
                ])?;
            }
            Record::Bridge(bridge) => {
                // Which dongle produced this capture. Written when the bridge announces itself,
                // always after the capture row exists, and again if it re-announces.
                tx.prepare_cached(
                    "UPDATE capture SET bridge_mac = ?1, bridge_chip = ?2, bridge_fw = ?3
                     WHERE id = 1",
                )?
                .execute(params![
                    &bridge.mac[..],
                    bridge.chip.as_str(),
                    bridge.fw_version.as_str()
                ])?;
            }
            Record::Assignment(a) => {
                tx.prepare_cached(
                    "INSERT INTO assignment
                       (node_mac, counter, wire_version, channels, ble, created_at,
                        delivered_at, outcome, latency_us)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                )?
                .execute(params![
                    &a.node_mac[..],
                    // Saturating upwards: the column records which epoch went out, and one reading
                    // lower than an epoch already spent is the failure this counter prevents.
                    i64::try_from(a.counter).unwrap_or(i64::MAX),
                    a.wire_version,
                    i64::try_from(a.channels.bits()).unwrap_or(0),
                    a.ble,
                    a.created_at_ms,
                    a.delivered_at_ms,
                    a.outcome.as_str(),
                    a.latency_us,
                ])?;
            }
            Record::BridgeStatus(status) => {
                tx.prepare_cached(
                    "INSERT INTO bridge_status
                       (rx_at, peer_count, rx_count, dropped_tx, uptime_ms, host_frames)
                     VALUES (?1,?2,?3,?4,?5,?6)",
                )?
                .execute(params![
                    status.rx_at_ms,
                    status.peer_count,
                    status.rx_count,
                    status.dropped_tx,
                    status.uptime_ms,
                    column(status.host_frames),
                ])?;
            }
            Record::HostStatus(host) => {
                tx.prepare_cached(
                    "INSERT INTO host_status
                       (at, frames, duplicate_batches, garbled, undecodable, incompatible,
                        foreign_fleet, foreign_admin, admin_windows_missed, lag_peak_us,
                        store_written, store_dropped, store_queue_peak, store_commit_peak_us,
                        throttled, soc_temp_mc, battery_mv, battery_ma)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
                )?
                .execute(params![
                    host.at_ms,
                    column(host.frames),
                    column(host.duplicate_batches),
                    column(host.garbled),
                    column(host.undecodable),
                    column(host.incompatible),
                    column(host.foreign_fleet),
                    column(host.foreign_admin),
                    column(host.admin_windows_missed),
                    column(host.lag_peak_us),
                    column(host.store_written),
                    column(host.store_dropped),
                    column(host.store_queue_peak),
                    column(host.store_commit_peak_us),
                    host.throttled,
                    host.soc_temp_mc,
                    host.battery_mv,
                    host.battery_ma,
                ])?;
            }
            Record::BatchGap(gap) => {
                tx.prepare_cached(
                    "INSERT INTO batch_gap (node_mac, rx_at, after_seq, seq, lost)
                     VALUES (?1,?2,?3,?4,?5)",
                )?
                .execute(params![
                    &gap.node_mac[..],
                    gap.rx_at_ms,
                    gap.after_seq,
                    gap.seq,
                    gap.lost,
                ])?;
            }
            Record::Raw(raw) => {
                tx.prepare_cached(
                    "INSERT INTO raw_frame (rx_at, src, dst, rssi, bytes)
                     VALUES (?1,?2,?3,?4,?5)",
                )?
                .execute(params![
                    raw.rx_at_ms,
                    &raw.src[..],
                    &raw.dst[..],
                    raw.rssi,
                    raw.bytes,
                ])?;
            }
        }
    }
    let committing = Instant::now();
    tx.commit()?;
    Ok(committing.elapsed())
}

#[cfg(test)]
mod tests {
    use super::{Store, fnv1a_64};
    use crate::record::{BridgeStatusSeen, Record};

    fn status(rx_at_ms: i64) -> Record {
        Record::BridgeStatus(BridgeStatusSeen {
            rx_at_ms,
            peer_count: 0,
            rx_count: 0,
            dropped_tx: 0,
            uptime_ms: 0,
            host_frames: 0,
        })
    }

    #[test]
    fn store_offer_leaves_drops_uncounted_when_queue_is_full() {
        let (store, _rx) = Store::detached(1);
        assert!(store.offer(status(0)));
        assert!(!store.offer(status(1)));
        assert_eq!(store.stats().dropped, 0);
        // `submit` still counts what it could not queue.
        assert_eq!(store.submit(vec![status(2)]), 1);
        assert_eq!(store.stats().dropped, 1);
    }

    #[test]
    fn fnv1a_64_matches_reference_when_hashing_known_vectors() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }
}
