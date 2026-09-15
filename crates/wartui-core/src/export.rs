//! WiGLE CSV export, in the v1.6 file format.
//!
//! A network's sightings are folded into recapture windows, and one row is
//! submitted per window: the strongest positioned sighting, with `FirstSeen`
//! from the window's own first sighting — positioned or not, which is a
//! different row and the reason the fold carries both. The window is anchored
//! at that first sighting and closes on the first sighting more than
//! [`ExportFilter::recapture_secs`] after it opened, so a network watched for
//! three hours yields a row an hour, rather than a row for ever (which would
//! score one capture on a leaderboard that counts re-captures) or a row per
//! sighting (which WiGLE would deduplicate its own side anyway). `0` keeps the
//! older shape: one row per network for the whole capture.
//!
//! The fold is in Rust rather than SQL because the anchor rule is sequential —
//! where a window ends decides where the next begins, which no window function
//! can compute without recursion. It streams sightings in network-then-time
//! order and holds one window's state at a time.
//!
//! The file is ordered by when each window opened, not by network, and that sort is
//! left to SQLite: submitted rows go into a temporary table on the export's own
//! connection and come back out in order. SQLite sorts within its page cache and spills
//! the rest to a temporary file, so a longer capture costs temporary disk rather than
//! memory. Held in a `Vec` instead, a full drive's rows took 183 MiB
//! (`docs/store-io-findings.md`). That file goes to `SQLITE_TMPDIR`, then `TMPDIR`,
//! then `/var/tmp`, which on a Pi that boots from its card is the card. Nothing in the
//! store is written: a temporary table belongs to the connection, not the file.
//!
//! Two details are here because WiGLE rejects files without them: the timestamp
//! must be zero-padded (`2026-05-01 13:34:37`, where the node firmware emits
//! `2026-5-1 13:34:37`), and the SSID must be RFC-4180 quoted, since an SSID
//! may contain a comma or a quote and is not required to be text at all.
//!
//! A third is here because the store is not rewritten: a capture taken before
//! `beacon::visible_ssid` existed holds a cloaked network's NUL padding verbatim, so
//! [`record::ssid_text`](crate::record::ssid_text) is applied on the way out. That a
//! capture from before a fix still exports correctly is the whole promise of the
//! export being a view over the store.
//!
//! The columns v1.6 added over v1.4 are derived or honestly blank rather than stored.
//! `Frequency` is computed from the channel a sighting named, because the centre
//! frequency is a function of the channel and the store already keeps the channel —
//! deriving it means every capture already on disk exports with the column filled.
//! `RCOIs` and `MfgrId` carry what the capture actually holds — a Passpoint access
//! point's roaming consortium identifiers, a BLE advertiser's manufacturer
//! identifier — and are blank when it holds none, or when the capture was taken by a
//! build that did not collect them: the store says that with NULL, and a blank
//! column repeats it without claiming the beacon carried nothing. A BLE
//! row's `Frequency` stays blank on purpose: what WiGLE asks for there is a
//! Bluetooth "device type" code, a class-of-device value that only an active inquiry
//! produces, and a node that never transmits while scanning has none to report.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, Row, Statement};
use wartui_proto::beacon::rcoi_text;

use crate::record::ssid_text;

/// The pre-header WiGLE reads for provenance, then the column header.
const COLUMNS: &str = "MAC,SSID,AuthMode,FirstSeen,Channel,Frequency,RSSI,\
CurrentLatitude,CurrentLongitude,AltitudeMeters,AccuracyMeters,RCOIs,MfgrId,Type";

/// The recapture window an export folds a network's sightings into, by
/// default: exactly one hour.
///
/// WDGWars, the leaderboard this default is cut for, scores a capture of a
/// network once per hour per user — "re-scanning the same AP within 1h is
/// silently skipped from scoring; GPS may still be refined" — and this is
/// that cooldown verbatim, with no slack in either direction: the site is
/// the authority on its own rule, and the export's job is to say when the AP
/// was actually scanned. A re-hearing within the hour stays in the row
/// already submitted, where its stronger reading can still refine the row's
/// position — the refinement the rule itself allows — and the hour is
/// inclusive like the rule's, because a sighting opens the next window only
/// *past* the width. Slack under the hour would write rows the site skips
/// anyway; slack over it would fold away re-hearings it counts.
pub const DEFAULT_RECAPTURE_SECS: u64 = 3600;

/// Why an export failed.
#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    /// The query failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The output could not be written.
    #[error("could not write the export: {0}")]
    Io(#[from] std::io::Error),
}

/// What to export.
#[derive(Debug, Clone, Copy)]
pub struct ExportFilter {
    /// Only this session. `None` exports every session, which is usually wanted:
    /// WiGLE deduplicates its own side and more sightings is better data.
    pub session_id: Option<i64>,
    /// How long after a window opened a sighting still belongs to it, in
    /// seconds; the first sighting later than that opens the next window.
    /// `0` folds a network's whole capture into one row.
    pub recapture_secs: u64,
}

impl Default for ExportFilter {
    /// Every session, folded into [`DEFAULT_RECAPTURE_SECS`] windows.
    fn default() -> Self {
        Self { session_id: None, recapture_secs: DEFAULT_RECAPTURE_SECS }
    }
}

/// What an export did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Rows written — one per network, per recapture window that closed on a
    /// positioned sighting.
    pub rows: u64,
    /// Rows left out because no sighting in their window had a position.
    ///
    /// Not an error and not silent: the operator should know how much of a capture is
    /// waiting on a GPS or a `--lat`/`--lon`.
    pub unpositioned: u64,
}

/// Write a WiGLE v1.6 CSV.
///
/// # Errors
/// [`ExportError`] if the query or the write fails.
pub fn wigle_csv<W: Write>(
    conn: &Connection,
    filter: ExportFilter,
    out: &mut W,
    app_version: &str,
) -> Result<ExportSummary, ExportError> {
    // `star=Sol,body=3,subBody=0` is Earth in the notation the pre-header
    // requires: body 3 is the third orbit, subBody 0 no satellite. Captures are
    // taken from the ground.
    writeln!(
        out,
        "WigleWifi-1.6,appRelease={app_version},model=wartui,release={app_version},\
         device=wartui,display=,board=ESP32-C5,brand=wartui,star=Sol,body=3,subBody=0"
    )?;
    writeln!(out, "{COLUMNS}")?;

    // A window wide enough to overflow the millisecond clock is the same as no
    // window at all: nothing in one capture can be that far apart.
    let recapture_ms =
        i64::try_from(filter.recapture_secs.saturating_mul(1000)).unwrap_or(i64::MAX);

    // The two trailer columns travel together in the v8 migration, so one
    // probe decides for both. A probe that cannot run at all is a file the
    // query below will speak for, so it falls through to the current shape.
    let sightings = if crate::store::has_column(conn, "observation", "rcoi").unwrap_or(true) {
        SELECT_SIGHTINGS
    } else {
        SELECT_SIGHTINGS_PRE_V8
    };

    // Set before the table exists, since changing it discards the connection's temporary
    // tables. A file is the bundled build's default already; the memory bound rests on it.
    conn.pragma_update(None, "temp_store", "FILE")?;
    // One transaction, so the inserts are one commit rather than one each. A failed
    // export rolls the table's creation back with it.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_EXPORT_ROWS)?;

    let mut summary = ExportSummary::default();
    {
        let mut insert = tx.prepare(INSERT_EXPORT_ROW)?;
        let mut stmt = tx.prepare(sightings)?;
        let mut rows = stmt.query(rusqlite::params![filter.session_id])?;
        let mut window: Option<Window> = None;

        while let Some(row) = rows.next()? {
            let candidate = Candidate::of(row)?;
            let opens_window = match &window {
                // The first sighting of the capture, or of the next network.
                None => true,
                Some(w) => {
                    w.best.bssid != candidate.bssid
                        || (recapture_ms > 0 && candidate.rx_at - w.first_seen > recapture_ms)
                }
            };
            if opens_window {
                close(&mut window, &mut insert, &mut summary)?;
                window = Some(Window { first_seen: candidate.rx_at, best: candidate });
            } else if let Some(w) = window.as_mut()
                && candidate.submits_over(&w.best)
            {
                w.best = candidate;
            }
        }
        close(&mut window, &mut insert, &mut summary)?;

        // By when each row's window opened, so a file re-exported after a decoder
        // fix diffs cleanly against the one before it; the network breaks ties.
        let mut sorted = tx.prepare(SELECT_EXPORT_ROWS)?;
        let mut rows = sorted.query([])?;
        while let Some(row) = rows.next()? {
            write_row(&Window { first_seen: row.get(13)?, best: Candidate::of(row)? }, out)?;
        }
    }
    tx.execute_batch("DROP TABLE temp.export_row")?;
    tx.commit()?;
    Ok(summary)
}

/// Retire the window in flight, submitting it to the table the rows are sorted in
/// if its best sighting is positioned and counting it if not.
fn close(
    window: &mut Option<Window>,
    insert: &mut Statement<'_>,
    summary: &mut ExportSummary,
) -> rusqlite::Result<()> {
    let Some(Window { first_seen, best }) = window.take() else { return Ok(()) };
    if best.positioned() {
        insert.execute(rusqlite::params![
            best.bssid,
            best.ssid,
            best.security,
            best.channel,
            best.rssi,
            best.lat,
            best.lon,
            best.alt,
            best.accuracy,
            best.kind,
            best.rcoi,
            best.mfgr_id,
            best.rx_at,
            first_seen,
        ])?;
        summary.rows += 1;
    } else {
        summary.unpositioned += 1;
    }
    Ok(())
}

/// Every sighting of every network in the filter, in fold order: network, then
/// time. The `id` tiebreaker keeps the order — and therefore which sighting a
/// tied window submits — deterministic.
///
/// There is deliberately no index for this to walk: a scan and a sort read the table in
/// order, and were faster than one random lookup per sighting (the store's v6 migration
/// says more).
const SELECT_SIGHTINGS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rcoi, mfgr_id, rx_at
FROM observation o
WHERE ?1 IS NULL OR o.session_id = ?1
ORDER BY o.bssid, o.rx_at, o.id
";

/// The same query for a capture that predates the trailer columns and has not
/// been migrated: both read as NULL, which is what they are — those sightings
/// were heard by builds that took nothing of the kind off the air.
///
/// Deliberately not a migration, because the export opens the file read-only
/// and that is what makes it safe against a capture still running — no write
/// lock to contend with the store's. An older file stays exactly as it is
/// until a later `run` migrates it, and exports truthfully in the meantime.
const SELECT_SIGHTINGS_PRE_V8: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind,
       NULL AS rcoi, NULL AS mfgr_id, rx_at
FROM observation o
WHERE ?1 IS NULL OR o.session_id = ?1
ORDER BY o.bssid, o.rx_at, o.id
";

/// The rows waiting for the sort by window start: a submitted sighting in
/// [`SELECT_SIGHTINGS`]'s column order, so [`Candidate::of`] reads it back, then when
/// its window opened. Dropped first in case an earlier export on this connection
/// left one behind.
const CREATE_EXPORT_ROWS: &str = r"
DROP TABLE IF EXISTS temp.export_row;
CREATE TEMP TABLE export_row (
  bssid BLOB, ssid BLOB, security TEXT, channel INTEGER, rssi INTEGER,
  lat REAL, lon REAL, alt REAL, accuracy REAL, kind TEXT, rcoi BLOB, mfgr_id INTEGER,
  rx_at INTEGER, first_seen INTEGER
);
";

const INSERT_EXPORT_ROW: &str = "INSERT INTO temp.export_row VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, \
?8, ?9, ?10, ?11, ?12, ?13, ?14)";

/// Two windows of one network never open at the same instant, so the order is total.
const SELECT_EXPORT_ROWS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rcoi, mfgr_id, rx_at,
       first_seen
FROM temp.export_row
ORDER BY first_seen, bssid
";

/// One sighting in flight through the fold, carrying everything a submitted
/// row needs so the winner of a window can be written without going back to
/// the database.
struct Candidate {
    bssid: Vec<u8>,
    ssid: Option<Vec<u8>>,
    security: String,
    channel: i64,
    rssi: i64,
    lat: Option<f64>,
    lon: Option<f64>,
    alt: Option<f64>,
    accuracy: Option<f64>,
    kind: String,
    rcoi: Option<Vec<u8>>,
    mfgr_id: Option<u16>,
    rx_at: i64,
}

impl Candidate {
    /// Read one sighting off a query row.
    fn of(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            bssid: row.get(0)?,
            ssid: row.get(1)?,
            security: row.get(2)?,
            channel: row.get(3)?,
            rssi: row.get(4)?,
            lat: row.get(5)?,
            lon: row.get(6)?,
            alt: row.get(7)?,
            accuracy: row.get(8)?,
            kind: row.get(9)?,
            rcoi: row.get(10)?,
            mfgr_id: row.get(11)?,
            rx_at: row.get(12)?,
        })
    }

    /// Whether this sighting carries coordinates a WiGLE row can be written
    /// from.
    const fn positioned(&self) -> bool {
        self.lat.is_some() && self.lon.is_some()
    }

    /// Whether this sighting is the one to submit over `other`: a positioned
    /// one beats an unpositioned one, then the stronger signal, then the
    /// earlier one. Position-first is what keeps a network with one good weaker
    /// sighting from losing it to a stronger sighting that had no fix; the
    /// strongest signal is the sighting closest to the transmitter.
    fn submits_over(&self, other: &Self) -> bool {
        match (self.positioned(), other.positioned()) {
            (true, false) => true,
            (false, true) => false,
            _ => self.rssi > other.rssi || (self.rssi == other.rssi && self.rx_at < other.rx_at),
        }
    }
}

/// The fold's state for one network's current window: when it opened, and the
/// sighting that would be submitted if the window closed now.
struct Window {
    first_seen: i64,
    best: Candidate,
}

fn write_row<W: Write>(row: &Window, out: &mut W) -> Result<(), ExportError> {
    let best = &row.best;
    let channel = best.channel;
    let rssi = best.rssi;
    let lat = best.lat.unwrap_or(0.0);
    let lon = best.lon.unwrap_or(0.0);
    let frequency = frequency_column(channel, &best.kind);
    // The WiGLE spellings of the two captured columns: the roaming consortium
    // body as hex identifiers, the company identifier as a number. A capture
    // from before they were collected stores NULL, and a blank column says so.
    let rcois = best.rcoi.as_deref().map_or_else(String::new, |body| rcoi_text(body).to_string());
    let mfgr = best.mfgr_id.map_or_else(String::new, |id| id.to_string());

    writeln!(
        out,
        "{},{},{},{},{channel},{frequency},{rssi},{lat},{lon},{},{},{rcois},{mfgr},{}",
        mac(&best.bssid),
        quote(&ssid_text(best.ssid.as_deref().unwrap_or_default())),
        quote(&best.security),
        timestamp(row.first_seen),
        best.alt.unwrap_or(0.0),
        best.accuracy.unwrap_or(0.0),
        if best.kind == "ble" { "BLE" } else { "WIFI" },
    )?;
    Ok(())
}

/// The centre frequency of the channel a sighting named, as WiGLE's `Frequency`
/// column wants it: a function of the channel, so derived on the way out rather
/// than stored, which fills the column for captures recorded before it existed.
///
/// Blank wherever the channel names no frequency this fleet's radios can tune —
/// the two bands the pools cover and nothing else, with channel 14's odd one out
/// — and blank for every BLE row, where the column means something only an
/// active inquiry could produce (see the module docs).
fn frequency_column(channel: i64, kind: &str) -> String {
    if kind != "wifi" {
        return String::new();
    }
    match channel {
        1..=13 => format!("{}", 2407 + 5 * channel),
        14 => "2484".to_owned(),
        36..=177 => format!("{}", 5000 + 5 * channel),
        _ => String::new(),
    }
}

/// Uppercase colon-separated, which is what WiGLE and the node firmware's own
/// SD logs both use.
fn mac(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// Zero-padded UTC, to the second.
///
/// The node firmware writes `2026-5-1 13:34:37` straight from `getDatetime()`,
/// which WiGLE rejects. Building it from a real timestamp rather than
/// reformatting a string is what keeps that from happening again.
fn timestamp(unix_ms: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(unix_ms).map_or_else(
        || "1970-01-01 00:00:00".to_owned(),
        |dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true).replace('T', " ").replace('Z', ""),
    )
}

/// RFC-4180: quote only when necessary, and double any embedded quote.
fn quote(field: &str) -> String {
    if field.contains([',', '"', '\r', '\n']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_are_zero_padded() {
        // The whole reason this function exists. The node firmware emits
        // `2026-5-1 13:34:37` for this instant, which WiGLE rejects.
        assert_eq!(timestamp(1_777_642_477_000), "2026-05-01 13:34:37");
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn only_troublesome_fields_are_quoted() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("has,comma"), "\"has,comma\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote(""), "");
    }

    #[test]
    fn macs_are_uppercase_and_colon_separated() {
        assert_eq!(mac(&[0x02, 0x00, 0x5E, 0x10, 0x57, 0x84]), "02:00:5E:10:57:84");
    }

    #[test]
    fn wifi_channels_map_to_their_centre_frequencies() {
        assert_eq!(frequency_column(1, "wifi"), "2412");
        assert_eq!(frequency_column(6, "wifi"), "2437");
        assert_eq!(frequency_column(13, "wifi"), "2472");
        // Channel 14 is the one 2.4 GHz channel that breaks the 5 MHz ladder.
        assert_eq!(frequency_column(14, "wifi"), "2484");
        assert_eq!(frequency_column(36, "wifi"), "5180");
        assert_eq!(frequency_column(165, "wifi"), "5825");
        assert_eq!(frequency_column(177, "wifi"), "5885");
    }

    #[test]
    fn frequencies_are_blank_where_no_frequency_was_named() {
        // A BLE row's frequency column means a "device type" code a passive scan
        // cannot produce, so it is blank whatever the channel field holds.
        assert_eq!(frequency_column(0, "ble"), "");
        // A channel no pool contains is not something to guess a frequency for.
        assert_eq!(frequency_column(0, "wifi"), "");
        assert_eq!(frequency_column(15, "wifi"), "");
        assert_eq!(frequency_column(200, "wifi"), "");
    }
}
