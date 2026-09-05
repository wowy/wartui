//! The observation store: SQLite, one writer thread, batched transactions.
//!
//! SQLite is the system of record and the WiGLE CSV is an export from it, not
//! the other way round. A capture that only ever existed as a CSV cannot be
//! re-exported after a decoder fix, cannot be queried while it is still
//! running, and cannot answer "which node saw this, and how strongly".
//!
//! Three decisions do most of the work here:
//!
//! **One thread owns the connection.** SQLite serialises writes regardless, so
//! sharing a connection between tasks buys contention rather than throughput.
//! A single owner also means no `Mutex<Connection>` for a caller to hold across
//! an await by accident.
//!
//! **Writes are batched into transactions.** Committing per row means an fsync
//! per row: on the order of a hundred inserts a second. Batching to
//! [`StoreConfig::batch_rows`] or [`StoreConfig::batch_interval`], whichever
//! comes first, is the difference between that and a hundred thousand. The
//! interval is what keeps a quiet fleet's rows from sitting unwritten.
//!
//! **The queue is bounded and drops rather than blocks.** If the disk stalls,
//! the engine must keep running: a lost observation is one row out of many,
//! whereas a stalled engine misses everything, including — once Phase 4 lands —
//! the assignment racing a node's 300 ms window. Drops are counted and shown.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags, params};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::ChannelPool;

use crate::engine::StoreStats;
use crate::record::Record;

/// Bumped whenever the schema changes shape.
pub const SCHEMA_VERSION: i32 = 1;

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

CREATE TABLE IF NOT EXISTS node (
  mac BLOB PRIMARY KEY,
  label TEXT,
  first_seen INTEGER NOT NULL,
  last_seen INTEGER NOT NULL,
  pinned_start_idx INTEGER,
  pinned_end_idx INTEGER
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

CREATE TABLE IF NOT EXISTS assignment (
  id INTEGER PRIMARY KEY,
  session_id INTEGER NOT NULL REFERENCES session(id),
  node_mac BLOB NOT NULL,
  wire_version INTEGER NOT NULL,
  node_index INTEGER NOT NULL,
  node_count INTEGER NOT NULL,
  start_idx INTEGER NOT NULL,
  end_idx INTEGER NOT NULL,
  created_at INTEGER NOT NULL,
  delivered_at INTEGER
);

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
  raw_text BLOB
);
-- Deliberately no unique constraint on bssid: every sighting is kept, with the
-- node that made it and the signal it saw. Deduplication is an export-time
-- question, and answering it at ingest would throw away the coverage data.
CREATE INDEX IF NOT EXISTS obs_bssid ON observation(bssid);
CREATE INDEX IF NOT EXISTS obs_node ON observation(node_mac, rx_at);

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
}

impl StoreConfig {
    /// Defaults tuned for a live capture: 512 rows or 100 ms.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            batch_rows: 512,
            batch_interval: Duration::from_millis(100),
            queue_depth: 4096,
        }
    }
}

/// What to record about the session being opened.
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    /// The bridge's MAC, once known.
    pub bridge_mac: Option<Vec<u8>>,
    /// Which chip it is.
    pub bridge_chip: Option<String>,
    /// Its firmware version.
    pub bridge_fw: Option<String>,
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
    join: Option<JoinHandle<()>>,
    session_id: i64,
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
        prepare(&conn)?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;

        let session_id = insert_session(&mut conn, session, started_at_ms)?;

        let (tx, rx) = sync_channel(config.queue_depth);
        let stats = Arc::new(Stats::default());
        let join = std::thread::Builder::new()
            .name("wartui-store".to_owned())
            .spawn({
                let stats = Arc::clone(&stats);
                let batch_rows = config.batch_rows;
                let batch_interval = config.batch_interval;
                move || writer(conn, &rx, session_id, batch_rows, batch_interval, &stats)
            })
            .map_err(StoreError::Spawn)?;

        Ok(Self { tx: Some(tx), stats, join: Some(join), session_id })
    }

    /// The session rows will be attributed to.
    #[must_use]
    pub const fn session_id(&self) -> i64 {
        self.session_id
    }

    /// Queue records, dropping any that do not fit rather than waiting.
    ///
    /// Never blocks. Returns how many were dropped, which is also counted into
    /// [`Self::stats`] so the UI can show it without the caller threading it
    /// back through.
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
    /// Called explicitly rather than left to `Drop` so a failure to finish the
    /// last transaction is reported rather than swallowed on the way out.
    pub fn close(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        drop(self.tx.take());
        if let Some(join) = self.join.take()
            && join.join().is_err()
        {
            tracing::error!("the store writer thread panicked; the last batch may be lost");
        }
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Open a second connection for reading, which is what export uses.
///
/// WAL is what makes this safe while a capture is running: readers do not block
/// the writer and the writer does not block them, so exporting an hour of data
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
    let found: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::SchemaTooNew { found, ours: SCHEMA_VERSION });
    }
    Ok(conn)
}

fn prepare(conn: &Connection) -> Result<(), rusqlite::Error> {
    // WAL so the export's reader and the ingest writer never wait on each
    // other; NORMAL because losing the tail of a capture to a power cut is an
    // acceptable trade for not fsyncing every commit; a busy timeout so a
    // concurrent export backs off instead of erroring.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(())
}

fn insert_session(
    conn: &mut Connection,
    session: &SessionInfo,
    started_at_ms: i64,
) -> Result<i64, rusqlite::Error> {
    conn.execute(
        "INSERT INTO session
           (started_at, bridge_mac, bridge_chip, bridge_fw, espnow_channel, channel_pool, notes)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            started_at_ms,
            session.bridge_mac,
            session.bridge_chip,
            session.bridge_fw,
            session.espnow_channel,
            pool_name(session.pool),
            session.notes,
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

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
    batch_rows: usize,
    batch_interval: Duration,
    stats: &Stats,
) {
    let mut pending: Vec<Record> = Vec::with_capacity(batch_rows);
    let mut last_flush = Instant::now();

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
                    flush(&mut conn, session_id, &mut pending, stats);
                    last_flush = Instant::now();
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                flush(&mut conn, session_id, &mut pending, stats);
                last_flush = Instant::now();
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    flush(&mut conn, session_id, &mut pending, stats);
    if let Err(e) = conn.execute(
        "UPDATE session SET ended_at = ?1 WHERE id = ?2",
        params![chrono::Utc::now().timestamp_millis(), session_id],
    ) {
        tracing::warn!("could not close out the session row: {e}");
    }
}

fn flush(conn: &mut Connection, session_id: i64, pending: &mut Vec<Record>, stats: &Stats) {
    if pending.is_empty() {
        return;
    }
    let count = pending.len();
    match write_batch(conn, session_id, pending) {
        Ok(()) => {
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

fn write_batch(
    conn: &mut Connection,
    session_id: i64,
    pending: &[Record],
) -> Result<(), rusqlite::Error> {
    let tx = conn.transaction()?;
    for record in pending {
        match record {
            Record::Node(node) => {
                tx.prepare_cached(
                    "INSERT INTO node (mac, first_seen, last_seen) VALUES (?1, ?2, ?3)
                     ON CONFLICT(mac) DO UPDATE SET last_seen = excluded.last_seen",
                )?
                .execute(params![
                    &node.mac[..],
                    node.first_seen_ms,
                    node.last_seen_ms
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
                        raw_text)
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
                    obs.raw_text,
                ])?;
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
    tx.commit()
}
