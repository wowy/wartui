//! The observation store: SQLite, one writer thread, batched transactions.
//!
//! SQLite is the system of record and the WiGLE CSV is an export from it, not
//! the other way round. A capture that only ever existed as a CSV cannot be
//! re-exported after a decoder fix, cannot be queried while it is still
//! running, and cannot answer "which node saw this, and how strongly".
//!
//! One thread owns the connection, because SQLite serialises writes regardless and
//! sharing one buys contention rather than throughput. Writes are batched to
//! [`StoreConfig::batch_rows`] or [`StoreConfig::batch_interval`], whichever comes
//! first: committing per row means an fsync per row, which is the difference
//! between a hundred inserts a second and a hundred thousand, and the interval is
//! what keeps a quiet fleet's rows from sitting unwritten. The queue is bounded and
//! drops rather than blocks, because a lost observation is one row while a stalled
//! engine misses everything. Drops are counted and shown.
//!
//! A second thread, with a connection of its own, copies the WAL back into the file right
//! after each commit ([`Checkpoint::Background`]), so no commit waits for the card while a
//! checkpoint syncs. It only copies pages: the writer is still the one thing that writes
//! rows. `docs/store-io-findings.md` has the measurements behind the batching, the
//! checkpoint and the lack of any index on sightings.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::ChannelPool;

use crate::engine::StoreStats;
use crate::record::Record;

/// Bumped whenever the schema changes shape.
///
/// `migrate` carries each step. Every one so far has been lossless, which is the
/// property to keep: a capture is data rather than a deployment.
pub const SCHEMA_VERSION: i32 = 7;

/// The schema, applied to any database that does not already have it.
const SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS session (
  id INTEGER PRIMARY KEY,
  started_at INTEGER NOT NULL,
  ended_at INTEGER,
  bridge_mac BLOB,
  bridge_chip TEXT,
  bridge_fw TEXT,
  espnow_channel INTEGER NOT NULL,
  channel_pool TEXT NOT NULL,
  notes TEXT
);

-- `capabilities` is the node's most recent heartbeat rendered the way the fleet
-- table shows it (`wartui/1.0;ble,5g`). Null means only that nothing but an
-- observation has been heard yet; see `record::NodeSeen`.
CREATE TABLE IF NOT EXISTS node (
  mac BLOB PRIMARY KEY,
  label TEXT,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  pinned_channels INTEGER,
  capabilities TEXT
);

CREATE TABLE IF NOT EXISTS heartbeat (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES session(id),
  node_mac BLOB NOT NULL,
  rx_at INTEGER NOT NULL,
  counter INTEGER NOT NULL,
  rssi INTEGER,
  admin_sent INTEGER NOT NULL DEFAULT 0,
  admin_acked INTEGER,
  admin_latency_us INTEGER
);

-- One row per transmitted assignment, written when its outcome is known, so
-- the table is append-only and a retry is a second row rather than an update.
-- `counter` is the persisted monotonic epoch and `wire_version` the byte that
-- actually went out; they differ because the wire field is one byte wide.
-- `channels` is the forty-bit SCAN_CHANNELS mask the frame carried, stored as the
-- integer it is: the indices are what the wire said, and rendering them depends on
-- a table that could change.
CREATE TABLE IF NOT EXISTS assignment (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES session(id),
  node_mac BLOB NOT NULL,
  counter INTEGER NOT NULL,
  wire_version INTEGER NOT NULL,
  node_index INTEGER NOT NULL,
  node_count INTEGER NOT NULL,
  channels INTEGER NOT NULL,
  ble INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  delivered_at INTEGER,
  outcome TEXT,
  latency_us INTEGER
);
CREATE INDEX IF NOT EXISTS assign_node ON assignment(node_mac, created_at);

-- Holds `assignment_version_counter`, the monotonic epoch of divergence 4.
CREATE TABLE IF NOT EXISTS kv (k TEXT PRIMARY KEY, v TEXT NOT NULL);

CREATE TABLE IF NOT EXISTS observation (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES session(id),
  node_mac BLOB NOT NULL,
  rx_at INTEGER NOT NULL,
  link_rssi INTEGER,
  bssid BLOB NOT NULL,
  ssid BLOB,
  security TEXT NOT NULL,
  channel INTEGER NOT NULL,
  rssi INTEGER NOT NULL,
  kind TEXT NOT NULL,
  lat REAL, lon REAL, alt REAL, accuracy REAL,
  pos_source TEXT NOT NULL,
  pos_at INTEGER,
  raw_body BLOB
);
-- Deliberately no unique constraint on bssid: every sighting is kept, with the node
-- that made it and the signal it saw. Deduplicating at ingest would throw away the
-- coverage data. Nor does the table have an index of any kind, though export groups by
-- bssid: the v6 and v7 steps in `migrate` say why.

CREATE TABLE IF NOT EXISTS raw_frame (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL,
  rx_at INTEGER NOT NULL,
  src BLOB NOT NULL,
  dst BLOB,
  rssi INTEGER,
  channel INTEGER,
  bytes BLOB NOT NULL
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
    /// The database was written by a newer wartui.
    #[error("database schema is v{found}, but this build understands v{ours}")]
    SchemaTooNew {
        /// What the file says.
        found: i32,
        /// What this build writes.
        ours: i32,
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
    /// How many pages the WAL may reach before a commit checkpoints it into the
    /// main file. `None` leaves SQLite's default of 1000.
    pub wal_autocheckpoint_pages: Option<u32>,
    /// Page size in bytes. Only a new file takes it: a WAL database keeps the size
    /// it was created with. `None` leaves SQLite's default of 4096.
    pub page_size: Option<u32>,
    /// Keep the wall time of every batch for [`Store::close`] to report.
    ///
    /// Off unless asked, because the list grows with every commit for as long as the
    /// capture runs. `wartui bench` asks.
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
    /// Woken by each commit, at most once every `every`, it copies the WAL back with a
    /// `PASSIVE` checkpoint, which runs beside the writer instead of holding it up. Right
    /// after a commit is the moment that matters: SQLite rewinds the WAL to its start only
    /// when a writer begins a transaction and finds every frame already copied, so a pass
    /// that finishes before the next commit keeps the WAL the size of a commit. Run on a
    /// timer instead, a pass lands across commits, never catches up, and the WAL only grows
    /// (`docs/store-io-findings.md`). If commits still outpace the card, the file grows
    /// anyway; once it passes `truncate_at` bytes, and a pass has caught up so nothing is
    /// left to copy under the writer's lock, a `TRUNCATE` rewinds it.
    Background {
        /// The least time between passes. Zero is a pass after every commit.
        every: Duration,
        /// The WAL file size in bytes past which to truncate it.
        truncate_at: u64,
    },
}

/// What the background checkpointer did. Empty under [`Checkpoint::Inline`], whose
/// checkpoints happen inside commits and are part of their timings.
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
    /// Defaults tuned for a live capture on a microSD card: commit every second or 16,384
    /// rows, queue up to 16,384 records, and checkpoint the WAL from a thread of its own
    /// right after each commit. SQLite's own cache and page size.
    ///
    /// Measured on a Raspberry Pi writing a full drive to a card
    /// (`docs/store-io-findings.md`). A commit a second rewrites the pages every commit
    /// touches once a second, not ten times, and checkpointing right after it lets the
    /// writer rewind the WAL itself: together they took the bytes written from 2.8× the
    /// database to 2.2× and the slowest batch from 149 ms to 58 ms. The queue is about
    /// 1.4 s of a drive's rows for about 2 MiB, against that 58 ms. The price is the
    /// loss window: a crash loses at most the second not yet committed plus the queue.
    /// The checkpoint also syncs the WAL once a second, so a power cut loses no more.
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
            // A pass after every commit, and the truncation a safety valve that a WAL the
            // size of one commit never reaches.
            checkpoint: Checkpoint::Background { every: Duration::ZERO, truncate_at: 64 << 20 },
        }
    }
}

/// What the writer did over the store's life, returned by [`Store::close`].
///
/// For `wartui bench`, which is how a change to the store is judged on the slow cards
/// it matters on. The view only ever needs [`StoreStats`].
#[derive(Debug, Clone, Default)]
pub struct StoreReport {
    /// Rows written.
    pub written: u64,
    /// Rows dropped, whether by a full queue or a failed batch.
    pub dropped: u64,
    /// Every committed batch, in order. Empty unless [`StoreConfig::timings`] asked
    /// for it.
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
    /// The `COMMIT` alone: where the WAL is written, and where SQLite runs an
    /// automatic checkpoint.
    pub commit: Duration,
}

/// What to record about the session being opened.
///
/// The bridge's own identity is deliberately absent: a session is opened before
/// any bridge has announced itself, so it arrives later as a
/// [`Record::Bridge`].
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// The ESP-NOW control channel.
    pub espnow_channel: u8,
    /// Which channels the fleet was told to scan.
    pub pool: ChannelPool,
    /// Anything the operator wants to remember about this run.
    pub notes: Option<String>,
}

#[derive(Debug, Default)]
struct Stats {
    written: AtomicU64,
    dropped: AtomicU64,
}

/// A handle to the writer thread.
#[derive(Debug)]
pub struct Store {
    tx: Option<SyncSender<Record>>,
    stats: Arc<Stats>,
    join: Option<JoinHandle<StoreReport>>,
    /// The background checkpointer, under [`Checkpoint::Background`]. It stops when the
    /// writer does, because the writer holds the only thing that wakes it.
    checkpointer: Option<JoinHandle<CheckpointReport>>,
    session_id: i64,
    assignment_base: u64,
}

impl Store {
    /// Open (or create) the database and start the writer.
    ///
    /// # Errors
    /// [`StoreError`] if the file cannot be opened, the schema cannot be
    /// applied, or the writer thread cannot be started.
    pub fn open(
        config: &StoreConfig,
        session: &SessionInfo,
        started_at_ms: i64,
    ) -> Result<Self, StoreError> {
        let mut conn = Connection::open(&config.path)?;
        prepare(&conn, config)?;
        // Before the schema, not after: `CREATE TABLE IF NOT EXISTS` no-ops
        // against a newer file's tables rather than failing, so an older build
        // would append old-shaped rows and stamp the version marker back down.
        let found = check_version(&conn)?;
        // All of it or none of it, version marker included. A migration is several
        // statements that only make sense together — v2's assignment rebuild
        // renames the old table before writing the new one — and the marker is
        // what decides whether they run again. Committed piecemeal, a failure part
        // way through leaves a file that is neither shape and still stamped old,
        // which every later open would re-enter and die in.
        let tx = conn.transaction()?;
        migrate(&tx, found)?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;

        let session_id = insert_session(&mut conn, session, started_at_ms)?;
        let assignment_base = reserve_versions(&mut conn)?;

        // The writer wakes the checkpointer after each commit. One wake-up waiting stands for
        // any number of commits, so the channel holds one.
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
                move || writer(conn, &rx, session_id, &config, &stats, wake.as_ref())
            })
            .map_err(StoreError::Spawn)?;

        let checkpointer = match (config.checkpoint, woken) {
            (Checkpoint::Background { every, truncate_at }, Some(woken)) => {
                let conn = Connection::open(&config.path)?;
                // Waits out the writer's commit rather than failing a truncation on it.
                conn.busy_timeout(Duration::from_secs(5))?;
                let wal = wal_path(&config.path);
                let join = std::thread::Builder::new()
                    .name("wartui-checkpoint".to_owned())
                    .spawn(move || checkpointer(&conn, &wal, &woken, every, truncate_at))
                    .map_err(StoreError::Spawn)?;
                Some(join)
            }
            _ => None,
        };

        Ok(Self {
            tx: Some(tx),
            stats,
            join: Some(join),
            checkpointer,
            session_id,
            assignment_base,
        })
    }

    /// The session rows will be attributed to.
    #[must_use]
    pub const fn session_id(&self) -> i64 {
        self.session_id
    }

    /// The last assignment epoch any wartui is known to have used against this
    /// database. The engine allocates from `base + 1` upwards.
    #[must_use]
    pub const fn assignment_base(&self) -> u64 {
        self.assignment_base
    }

    /// Queue records, dropping any that do not fit rather than waiting.
    ///
    /// Never blocks. Returns how many were dropped, also counted into
    /// [`Self::stats`] so the UI need not thread it back through.
    pub fn submit(&self, records: Vec<Record>) -> usize {
        let Some(tx) = &self.tx else { return records.len() };
        let mut dropped = 0usize;
        for record in records {
            if tx.try_send(record).is_err() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.stats.dropped.fetch_add(dropped as u64, Ordering::Relaxed);
        }
        dropped
    }

    /// Rows written and rows dropped so far.
    #[must_use]
    pub fn stats(&self) -> StoreStats {
        StoreStats {
            written: self.stats.written.load(Ordering::Relaxed),
            dropped: self.stats.dropped.load(Ordering::Relaxed),
        }
    }

    /// Flush everything queued, close the session and stop the writer.
    ///
    /// Called explicitly rather than left to `Drop`, so a failure to finish the last
    /// transaction is reported rather than swallowed. Returns what the writer did,
    /// which only the benchmark reads.
    pub fn close(mut self) -> StoreReport {
        self.shutdown()
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
        // After the writer, whose end is what stops it, so its last batch has a checkpoint
        // to land in.
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

/// Open a second connection for reading, which is what export uses.
///
/// WAL is what makes this safe while a capture is running: exporting an hour of data
/// mid-run cannot stall ingest.
///
/// # Errors
/// [`StoreError`] if the file cannot be opened or is from a newer wartui.
pub fn open_readonly(path: &Path) -> Result<Connection, StoreError> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    check_version(&conn)?;
    Ok(conn)
}

/// Refuse a database written by a newer wartui, and report what this one is.
///
/// A fresh file reads 0, which is older than anything and therefore fine.
fn check_version(conn: &Connection) -> Result<i32, StoreError> {
    let found: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew { found, ours: SCHEMA_VERSION });
    }
    Ok(found)
}

/// Bring an older database up to the current shape.
///
/// Called after [`check_version`] has ruled out a newer file and before [`SCHEMA`]
/// is applied: `CREATE TABLE IF NOT EXISTS` will not widen an existing table, so
/// anything structural happens here.
fn migrate(conn: &Connection, found: i32) -> Result<(), StoreError> {
    let has_assignment = has_table(conn, "assignment")?;

    // v1 declared this table but could not transmit, so it is provably empty and
    // dropping it is simpler than three `ALTER TABLE`s.
    if found == 1 && has_assignment {
        conn.execute_batch("DROP TABLE assignment")?;
    }

    // v2 rows are real and worth keeping: `start_idx`/`end_idx` name a contiguous
    // run, which a mask can say exactly, so the bounds convert rather than being
    // thrown away. `ble` is 0 because no v2 build could ask for it. Rebuilt rather
    // than `ALTER TABLE`d, so no later reader has to decide which pair to believe.
    if found == 2 && has_assignment {
        conn.execute_batch(
            "ALTER TABLE assignment RENAME TO assignment_v2;
             CREATE TABLE assignment (
               id INTEGER PRIMARY KEY,
               session_id INTEGER NOT NULL REFERENCES session(id),
               node_mac BLOB NOT NULL,
               counter INTEGER NOT NULL,
               wire_version INTEGER NOT NULL,
               node_index INTEGER NOT NULL,
               node_count INTEGER NOT NULL,
               channels INTEGER NOT NULL,
               ble INTEGER NOT NULL,
               created_at INTEGER NOT NULL,
               delivered_at INTEGER,
               outcome TEXT,
               latency_us INTEGER
             );
             INSERT INTO assignment
               (id, session_id, node_mac, counter, wire_version, node_index, node_count,
                channels, ble, created_at, delivered_at, outcome, latency_us)
             SELECT id, session_id, node_mac, counter, wire_version, node_index, node_count,
                    ((1 << (end_idx - start_idx + 1)) - 1) << start_idx, 0,
                    created_at, delivered_at, outcome, latency_us
             FROM assignment_v2;
             DROP TABLE assignment_v2;",
        )?;
    }

    // Added in v4. Every row already in the file predates the token, so null is
    // the truthful value for all of them: those nodes were never asked. The
    // `has_table` guard is for a v1 file, which has no `node` table at all.
    if (1..=3).contains(&found)
        && has_table(conn, "node")?
        && !has_column(conn, "node", "capabilities")?
    {
        conn.execute_batch("ALTER TABLE node ADD COLUMN capabilities TEXT")?;
    }

    // Never written by anything in any version, so replaced rather than kept.
    if (1..=2).contains(&found) && has_column(conn, "node", "pinned_start_idx")? {
        conn.execute_batch(
            "ALTER TABLE node DROP COLUMN pinned_start_idx;
             ALTER TABLE node DROP COLUMN pinned_end_idx;
             ALTER TABLE node ADD COLUMN pinned_channels INTEGER;",
        )?;
    }

    // v5. The column keeps the observation exactly as it arrived, and what arrives
    // stopped being text: older rows hold a comma-separated line, newer ones the
    // frame. Renamed rather than dropped because the old rows are still the truest
    // record of what those nodes sent, and rather than left alone because a column
    // called `raw_text` holding binary gets read wrong once and quietly.
    if (1..=4).contains(&found)
        && has_table(conn, "observation")?
        && has_column(conn, "observation", "raw_text")?
    {
        conn.execute_batch("ALTER TABLE observation RENAME COLUMN raw_text TO raw_body")?;
    }

    // v6. `obs_bssid` cost more than it bought, on both sides of the store. Its key is a
    // random address, so every commit dirtied leaf pages across the whole index, each
    // written once to the WAL and again at checkpoint. With it, the process wrote fourteen
    // times as much for the same rows, and a microSD card dropped two rows in three where
    // without it the card dropped none. Export, the one reader that groups by address, was
    // faster without it as well: walking the index reads the table once per sighting in
    // random order, where a scan and a sort read it in order. The numbers are in
    // `docs/store-io-findings.md`. An index is derived data, so dropping it loses nothing.
    if (1..=5).contains(&found) {
        conn.execute_batch("DROP INDEX IF EXISTS obs_bssid")?;
    }

    // v7. `obs_node` answered a question nothing asked. No query anywhere filtered or
    // sorted sightings by node, yet the index was about a sixth of a full drive's file
    // (48 MiB of 292) and every commit rewrote the last page of each node's run in it, to
    // the WAL and again at checkpoint. A per-node query that wants it later can build it
    // over a finished capture far more cheaply than capture could keep it up.
    if (1..=6).contains(&found) {
        conn.execute_batch("DROP INDEX IF EXISTS obs_node")?;
    }
    Ok(())
}

/// Whether the file has this table at all. A v1 database predates most of them,
/// and `ALTER TABLE` on one that is not there aborts the whole migration.
fn has_table(conn: &Connection, table: &str) -> Result<bool, StoreError> {
    let mut stmt =
        conn.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
    Ok(stmt.exists(params![table])?)
}

/// Whether a table already has a column, so a migration can be run once.
fn has_column(conn: &Connection, table: &str, column: &str) -> Result<bool, StoreError> {
    let mut stmt = conn.prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
    Ok(stmt.exists(params![table, column])?)
}

/// How far ahead of the last used epoch to move the persisted counter at open.
///
/// The counter is persisted because a re-used epoch is not only ignored
/// but still acknowledged, so the host believes an assignment landed that the node
/// discarded. Writing the counter forward before issuing anything means a crash can
/// only ever skip epochs. Skipping is free; repeating is the bug.
///
/// Sixty-four holds as long as one assignment row in every sixty-four survives the
/// store's lossy queue, since each re-books the block from its own counter. Losing
/// sixty-four consecutively means a queue full for the whole capture, which the
/// view is already shouting about.
const VERSION_RESERVATION: u64 = 64;

/// Read the persisted assignment epoch and immediately book a block of them.
///
/// Under `BEGIN IMMEDIATE`, so the read and the write cannot interleave with another
/// wartui opening the same file: two processes reading the same base would book the
/// same block and hand the same epoch to the same node.
fn reserve_versions(conn: &mut Connection) -> Result<u64, StoreError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let base: u64 = tx
        .query_row("SELECT v FROM kv WHERE k = 'assignment_version_counter'", [], |row| {
            row.get::<_, String>(0)
        })
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    tx.execute(
        "INSERT INTO kv (k, v) VALUES ('assignment_version_counter', ?1)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![(base + VERSION_RESERVATION).to_string()],
    )?;
    tx.commit()?;
    Ok(base)
}

/// The background checkpointer's loop, woken by the writer's commits until the writer is
/// gone.
///
/// Always one last pass on the way out, so a stopped store does not leave behind a WAL
/// the next open has to replay.
fn checkpointer(
    conn: &Connection,
    wal: &Path,
    woken: &Receiver<()>,
    every: Duration,
    truncate_at: u64,
) -> CheckpointReport {
    let mut report = CheckpointReport::default();
    let mut last_pass: Option<Instant> = None;
    loop {
        let stopping = woken.recv().is_err();
        // Skipped rather than held back: a pass delayed until the interval is up would
        // start partway to the next commit, which is when it is least likely to catch up.
        if !stopping && last_pass.is_some_and(|last| last.elapsed() < every) {
            continue;
        }
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
        // Only once a pass has caught up: the truncation then holds the writer's lock to
        // rewind the file, not to copy and sync whatever the pass left behind.
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
        // Negative means KiB rather than pages, so the figure holds whatever the
        // page size is.
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

fn insert_session(
    conn: &mut Connection,
    session: &SessionInfo,
    started_at_ms: i64,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO session (started_at, espnow_channel, channel_pool, notes)
         VALUES (?1, ?2, ?3, ?4)",
        params![started_at_ms, session.espnow_channel, pool_name(session.pool), session.notes,],
    )?;
    Ok(conn.last_insert_rowid())
}

/// The pool's name as stored, which is not its name on screen: `ChannelPool`'s
/// `Display` says "US", and captures already on disk say "us". Leave these.
const fn pool_name(pool: ChannelPool) -> &'static str {
    match pool {
        ChannelPool::Us => "us",
        ChannelPool::All => "all",
    }
}

const fn kind_name(kind: RecordKind) -> &'static str {
    match kind {
        RecordKind::Wifi => "wifi",
        RecordKind::Ble => "ble",
    }
}

fn writer(
    mut conn: Connection,
    rx: &Receiver<Record>,
    session_id: i64,
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
        flush(conn, session_id, pending, stats, config.timings.then_some(&mut report));
        // A full channel already has a wake-up waiting, and that one covers this commit.
        if committing && let Some(wake) = wake {
            let _ = wake.try_send(());
        }
    };

    loop {
        match rx.recv_timeout(batch_interval) {
            Ok(record) => {
                pending.push(record);
                // Both conditions matter. Without the row count a burst commits
                // one enormous transaction; without the elapsed check a steady
                // trickle that never reaches the count would sit in memory
                // indefinitely, because the timeout only fires when the queue
                // goes quiet.
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
        "UPDATE session SET ended_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().timestamp_millis(), session_id],
    ) {
        tracing::warn!("could not close out the session row: {e}");
    }
    report
}

fn flush(
    conn: &mut Connection,
    session_id: i64,
    pending: &mut Vec<Record>,
    stats: &Stats,
    report: Option<&mut StoreReport>,
) {
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    let started = Instant::now();
    match write_batch(conn, session_id, pending) {
        Ok(commit) => {
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
            // The alternative is retrying forever behind a queue that is still
            // filling, which turns a full disk into a wedged capture.
            stats.dropped.fetch_add(count as u64, Ordering::Relaxed);
            tracing::error!("dropping {count} rows: {e}");
        }
    }
    pending.clear();
}

/// Write one batch in one transaction, returning how long the commit alone took.
fn write_batch(
    conn: &mut Connection,
    session_id: i64,
    pending: &[Record],
) -> Result<Duration, rusqlite::Error> {
    let tx = conn.transaction()?;
    for record in pending {
        match record {
            Record::Node(node) => {
                tx.prepare_cached(
                    "INSERT INTO node (mac, first_seen, last_seen, capabilities)
                       VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(mac) DO UPDATE SET
                       last_seen = excluded.last_seen,
                       -- Most frames are observations and carry no token, so
                       -- writing `excluded` straight in would erase what the
                       -- last heartbeat said on the very next line collected.
                       -- The column is therefore the last token ever seen from
                       -- this node rather than the last one it sent: a board
                       -- reflashed to stock keeps it here for the rest of the
                       -- capture, while the engine, which re-reads it from
                       -- every heartbeat, correctly stops believing it.
                       capabilities = coalesce(excluded.capabilities, node.capabilities)",
                )?
                .execute(params![
                    &node.mac[..],
                    node.first_seen_ms,
                    node.last_seen_ms,
                    node.capabilities
                ])?;
            }
            Record::Heartbeat(hb) => {
                tx.prepare_cached(
                    "INSERT INTO heartbeat (session_id, node_mac, rx_at, counter, rssi)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                )?
                .execute(params![
                    session_id,
                    &hb.node_mac[..],
                    hb.rx_at_ms,
                    hb.counter,
                    hb.link_rssi
                ])?;
            }
            Record::Observation(obs) => {
                tx.prepare_cached(
                    "INSERT INTO observation
                       (session_id, node_mac, rx_at, link_rssi, bssid, ssid, security,
                        channel, rssi, kind, lat, lon, alt, accuracy, pos_source, pos_at,
                        raw_body)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)",
                )?
                .execute(params![
                    session_id,
                    &obs.node_mac[..],
                    obs.rx_at_ms,
                    obs.link_rssi,
                    &obs.bssid[..],
                    obs.ssid,
                    obs.security,
                    obs.channel,
                    obs.rssi,
                    kind_name(obs.kind),
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
                // Which dongle produced this capture. Written when the bridge
                // announces itself, which is always after the session row
                // exists, and rewritten if it announces again.
                tx.prepare_cached(
                    "UPDATE session SET bridge_mac = ?2, bridge_chip = ?3, bridge_fw = ?4
                     WHERE id = ?1",
                )?
                .execute(params![
                    session_id,
                    &bridge.mac[..],
                    bridge.chip.as_str(),
                    bridge.fw_version.as_str()
                ])?;
            }
            Record::Assignment(a) => {
                tx.prepare_cached(
                    "INSERT INTO assignment
                       (session_id, node_mac, counter, wire_version, node_index, node_count,
                        channels, ble, created_at, delivered_at, outcome, latency_us)
                     VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                )?
                .execute(params![
                    session_id,
                    &a.node_mac[..],
                    a.counter,
                    a.wire_version,
                    a.node_index,
                    a.node_count,
                    i64::try_from(a.channels.bits()).unwrap_or(0),
                    a.ble,
                    a.created_at_ms,
                    a.delivered_at_ms,
                    a.outcome.as_str(),
                    a.latency_us,
                ])?;
                // Keep the persisted epoch ahead of what has actually been
                // used, so the reservation taken at open is refreshed as the
                // session spends it. `max` rather than a plain write: rows
                // reach the writer in order, but a batch that partly failed
                // must never walk the counter backwards.
                tx.prepare_cached(
                    "INSERT INTO kv (k, v) VALUES ('assignment_version_counter', ?1)
                     ON CONFLICT(k) DO UPDATE SET
                       v = CAST(max(CAST(v AS INTEGER), CAST(excluded.v AS INTEGER)) AS TEXT)",
                )?
                .execute(params![(a.counter + VERSION_RESERVATION).to_string()])?;
            }
            Record::Raw(raw) => {
                tx.prepare_cached(
                    "INSERT INTO raw_frame (session_id, rx_at, src, dst, rssi, channel, bytes)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )?
                .execute(params![
                    session_id,
                    raw.rx_at_ms,
                    &raw.src[..],
                    &raw.dst[..],
                    raw.rssi,
                    raw.channel,
                    raw.bytes,
                ])?;
            }
        }
    }
    let committing = Instant::now();
    tx.commit()?;
    Ok(committing.elapsed())
}
