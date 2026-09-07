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

use rusqlite::{Connection, OpenFlags, TransactionBehavior, params};
use wartui_proto::air::RecordKind;
use wartui_proto::plan::ChannelPool;

use crate::engine::StoreStats;
use crate::record::Record;

/// Bumped whenever the schema changes shape.
///
/// v2 added `assignment.outcome` and `assignment.latency_us`, because Phase 4
/// transmits and an assignment that was sent is not the same thing as one that
/// landed.
///
/// v3 replaced `assignment.start_idx`/`end_idx` with a `channels` bitmask and
/// added `ble`, because an assignment stopped being a contiguous range. A v2
/// row's bounds convert into a mask exactly, so the migration is lossless.
pub const SCHEMA_VERSION: i32 = 4;

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

-- `capabilities` is the token from the node's most recent heartbeat, verbatim.
-- Null means it never sent one, which is what a stock node and any wartui node
-- built before Phase 2 both look like — and is why such a node has heartbeats
-- here and no assignment rows at all.
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

-- One row per transmitted MSG_ADMIN, written when its outcome is known, so
-- the table is append-only and a retry is a second row rather than an update.
-- `counter` is the persisted monotonic epoch and `wire_version` the byte that
-- actually went out; they differ because the wire field is one byte wide.
-- `channels` is the forty-bit SCAN_CHANNELS mask the frame carried, stored as
-- the integer it is rather than as a rendered channel list: the indices are
-- what the wire said, and turning them into channel numbers is the reader's
-- job and depends on a table that could change.
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
    join: Option<JoinHandle<()>>,
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
        prepare(&conn)?;
        // Before the schema, not after. `CREATE TABLE IF NOT EXISTS` no-ops
        // against a newer file's tables rather than failing, so an older build
        // would append v1-shaped rows into a v2 database and then stamp the
        // version marker back down to 1 — leaving neither build able to tell
        // that it had happened.
        let found = check_version(&conn)?;
        // All of it or none of it, version marker included. A migration is
        // several statements that only make sense together — v2's assignment
        // rebuild renames the old table before it has written the new one —
        // and the marker is what decides whether they run again. Committed
        // piecemeal, a failure part way through would leave a file that is
        // neither shape and still stamped with the old version, so every later
        // open would re-enter the migration and die on the rename against a
        // table that is already there. A rollback leaves the file exactly as it
        // was found, which the next open can migrate again.
        let tx = conn.transaction()?;
        migrate(&tx, found)?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;

        let session_id = insert_session(&mut conn, session, started_at_ms)?;
        let assignment_base = reserve_versions(&mut conn)?;

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

        Ok(Self { tx: Some(tx), stats, join: Some(join), session_id, assignment_base })
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
/// Only ever called after [`check_version`] has ruled out a newer file, and
/// before [`SCHEMA`] is applied — `CREATE TABLE IF NOT EXISTS` will not widen a
/// table that already exists, so anything structural has to happen here.
fn migrate(conn: &Connection, found: i32) -> Result<(), StoreError> {
    let has_assignment = has_table(conn, "assignment")?;

    // v1 declared this table but nothing in a v1 build could ever write to it:
    // that release could not transmit, and an assignment row is only written
    // for a frame that went out. So it is provably empty and dropping it loses
    // nothing, which is a great deal simpler than three `ALTER TABLE`s.
    if found == 1 && has_assignment {
        conn.execute_batch("DROP TABLE assignment")?;
    }

    // v2 rows are real and worth keeping. Their `start_idx`/`end_idx` name a
    // contiguous run, which is exactly what a mask can say, so the bounds
    // convert rather than being thrown away: bits `start..=end` set, which for
    // a 40-bit field is `((1 << (end - start + 1)) - 1) << start`. `ble` is 0
    // because no v2 build could ask for it.
    //
    // Rebuilt rather than `ALTER TABLE`d, because leaving the old columns in
    // place would mean every later reader deciding which pair to believe.
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

    // Never written by anything, in any version: a v2 build declared them and
    // no code read or set them. Replaced rather than kept so the node table
    // does not carry a shape the rest of the schema stopped using.
    if (1..=2).contains(&found) && has_column(conn, "node", "pinned_start_idx")? {
        conn.execute_batch(
            "ALTER TABLE node DROP COLUMN pinned_start_idx;
             ALTER TABLE node DROP COLUMN pinned_end_idx;
             ALTER TABLE node ADD COLUMN pinned_channels INTEGER;",
        )?;
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
/// Divergence 4 exists because a node adopts an assignment only when its
/// version *differs* from the one it holds, so re-using an epoch after a
/// restart is silently ignored — and worse, the ack still arrives, so the host
/// believes an assignment landed that the node discarded. Writing the counter
/// forward before issuing anything means a crash can only ever skip epochs,
/// never repeat one. Skipping is free; repeating is the bug.
///
/// Sixty-four holds as long as one assignment row in every sixty-four survives
/// the store's lossy queue, since each one re-books the block from its own
/// counter. Assignments are written at operator-keypress rate and are a few
/// dozen bytes, so losing sixty-four consecutively means the queue has been
/// full for the whole capture — a state the view is already shouting about.
const VERSION_RESERVATION: u64 = 64;

/// Read the persisted assignment epoch and immediately book a block of them.
///
/// Under `BEGIN IMMEDIATE`, so the read and the write cannot interleave with
/// another wartui opening the same file. Two processes that both read the same
/// base would both book the same block and then hand the same epoch to the same
/// node — a frame the node discards on its `!=` and acknowledges anyway, which
/// is the one failure this whole mechanism exists to prevent.
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
    tx.commit()
}
