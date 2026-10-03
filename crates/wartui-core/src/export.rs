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
//! A network is an address and a kind. One address can be both a Wi-Fi network and a
//! BLE advertiser, and WiGLE records those as two things (the `Type` column), so each
//! kind folds into windows of its own and lends identifiers only to its own row.
//!
//! The fold is in Rust rather than SQL because the anchor rule is sequential —
//! where a window ends decides where the next begins, which no window function
//! can compute without recursion. It streams sightings in address, kind, then time
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
//! The [`ExportSummary`] counts are exact rather than estimated, because the fold already
//! walks networks in order and knows where each begins. Their cost is the node and
//! position-source columns riding through the whole-table sort, which
//! [`SELECT_SIGHTINGS`] explains.
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
//! [`ExportFilter::after_uploads`] leaves out every sighting the newest upload the site did
//! not report failed covered, so a repeat upload sends what came after. The cutoff is the last
//! sighting that upload walked, in the order the capture stored them: ids follow commit order,
//! so a frame whose sightings span two commits is split exactly, and a capture still being
//! written loses nothing to the upload. The fold starts afresh at that cutoff: a network
//! heard on both sides of it within the recapture width gets a second row, which WDGWars
//! skips when scoring. An unpositioned sighting walked before the cutoff counts as sent, so a
//! fix that arrives later does not bring it back.
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
use wartui_bridge::ports::mac_text;
use wartui_proto::beacon::rcoi_text;
use wartui_proto::link::Mac;

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
    /// How long after a window opened a sighting still belongs to it, in
    /// seconds; the first sighting later than that opens the next window.
    /// `0` folds a network's whole capture into one row.
    pub recapture_secs: u64,
    /// Leave out sightings at or before the newest upload the capture records, short of a
    /// failed one. Off by default, so `export` and `analyze` read the whole capture.
    pub after_uploads: bool,
}

impl Default for ExportFilter {
    /// Every sighting, folded into [`DEFAULT_RECAPTURE_SECS`] windows.
    fn default() -> Self {
        Self { recapture_secs: DEFAULT_RECAPTURE_SECS, after_uploads: false }
    }
}

/// What an export did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Rows written — one per network, per recapture window that closed on a
    /// positioned sighting.
    pub rows: u64,
    /// Rows left out because no sighting in their window had a position.
    ///
    /// Not an error and not silent: the operator should know how much of a capture is
    /// waiting on a GPS.
    pub unpositioned: u64,
    /// Wi-Fi networks, sightings and rows.
    pub wifi: KindStats,
    /// Bluetooth devices, sightings and rows.
    pub ble: KindStats,
    /// Wi-Fi networks per band.
    pub wifi_bands: Bands,
    /// Sightings by which tier of the position chain answered.
    pub positions: Positions,
    /// Sightings per node, sorted by address.
    pub nodes: Vec<NodeStats>,
    /// The earliest sighting's receive time, in unix milliseconds. `None` when there
    /// are no sightings.
    pub first_rx: Option<i64>,
    /// The latest sighting's receive time, in unix milliseconds.
    pub last_rx: Option<i64>,
    /// The highest observation id walked, which is what an upload records as its cutoff.
    pub last_id: Option<i64>,
}

/// One record kind's share of an export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Distinct addresses heard as this kind. An address heard as both kinds counts
    /// once in each.
    pub networks: u64,
    /// Sightings of this kind.
    pub sightings: u64,
    /// Rows of this kind written to the CSV.
    pub rows: u64,
}

/// Wi-Fi networks per band, each counted once in every band it was heard on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bands {
    /// Channels 1 to 14.
    pub ghz2_4: u64,
    /// Channels 32 to 177.
    pub ghz5: u64,
    /// Any other channel.
    pub other: u64,
}

/// Sightings by position source.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Positions {
    /// Positioned by the GPS.
    pub gps: u64,
    /// Positioned by the static `--lat`/`--lon`.
    pub fixed: u64,
    /// With no position.
    pub none: u64,
}

/// One node's sightings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeStats {
    /// The node's address.
    pub mac: Mac,
    /// Wi-Fi sightings it reported.
    pub wifi: u64,
    /// Bluetooth sightings it reported.
    pub ble: u64,
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

    // Set before the table exists, since changing it discards the connection's temporary
    // tables. A file is the bundled build's default already; the memory bound rests on it.
    conn.pragma_update(None, "temp_store", "FILE")?;
    // One transaction, so the inserts are one commit rather than one each. A failed
    // export rolls the table's creation back with it.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_EXPORT_ROWS)?;

    let mut tally = Tally::default();
    {
        let mut insert = tx.prepare(INSERT_EXPORT_ROW)?;
        let mut stmt = tx.prepare(SELECT_SIGHTINGS)?;
        let mut rows = stmt.query([filter.after_uploads])?;
        let mut window: Option<Window> = None;

        while let Some(row) = rows.next()? {
            let mut candidate = Candidate::of(row)?;
            let (node, pos_source, id) = heard_by(row)?;
            // The first sighting of the capture, or of the next network.
            let new_network = window
                .as_ref()
                .is_none_or(|w| w.best.bssid != candidate.bssid || w.best.kind != candidate.kind);
            tally.see(&candidate, new_network, node, pos_source, id);
            let opens_window = new_network
                || window.as_ref().is_some_and(|w| {
                    recapture_ms > 0 && candidate.rx_at - w.first_seen > recapture_ms
                });
            if opens_window {
                close(&mut window, &mut insert, &mut tally.summary)?;
                window = Some(Window { first_seen: candidate.rx_at, best: candidate });
            } else if let Some(w) = window.as_mut() {
                // Which sighting submits is decided by position and signal alone;
                // the identifiers are the window's, so a loser still lends them.
                if candidate.submits_over(&w.best) {
                    candidate.inherit_identifiers(&mut w.best);
                    w.best = candidate;
                } else {
                    w.best.inherit_identifiers(&mut candidate);
                }
            }
        }
        close(&mut window, &mut insert, &mut tally.summary)?;

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
    Ok(tally.finish())
}

/// Counts what the export walked over, for its [`ExportSummary`].
///
/// Exact, because the fold walks networks (address and kind) in order and tells the
/// tally where each begins. Memory is bounded by the fleet rather than the capture: the
/// node list holds one entry per node that reported, and the per-network band bits reset
/// when the network changes. Nothing here allocates per sighting.
#[derive(Default)]
struct Tally {
    summary: ExportSummary,
    /// Which bands this network has been heard on, one bit per [`Bands`] field.
    bands: u8,
}

impl Tally {
    /// Count one sighting, the first of its network when `new_network`, heard by `node`,
    /// positioned by the `pos_source` code [`SELECT_SIGHTINGS`] gives it, stored as `id`.
    fn see(&mut self, c: &Candidate, new_network: bool, node: Mac, pos_source: i64, id: i64) {
        let ble = c.kind == "ble";
        let summary = &mut self.summary;
        let stats = if ble { &mut summary.ble } else { &mut summary.wifi };
        stats.sightings += 1;
        if new_network {
            self.bands = 0;
            stats.networks += 1;
        }
        if !ble {
            // The ranges `frequency_column` puts on each ladder.
            let (bit, count) = match c.channel {
                1..=14 => (1, &mut summary.wifi_bands.ghz2_4),
                32..=177 => (2, &mut summary.wifi_bands.ghz5),
                _ => (4, &mut summary.wifi_bands.other),
            };
            if self.bands & bit == 0 {
                self.bands |= bit;
                *count += 1;
            }
        }
        match pos_source {
            0 => summary.positions.gps += 1,
            1 => summary.positions.fixed += 1,
            _ => summary.positions.none += 1,
        }
        let at = match summary.nodes.iter().position(|n| n.mac == node) {
            Some(at) => at,
            None => {
                summary.nodes.push(NodeStats { mac: node, wifi: 0, ble: 0 });
                summary.nodes.len() - 1
            }
        };
        if ble {
            summary.nodes[at].ble += 1;
        } else {
            summary.nodes[at].wifi += 1;
        }
        summary.first_rx = Some(summary.first_rx.map_or(c.rx_at, |t| t.min(c.rx_at)));
        summary.last_rx = Some(summary.last_rx.map_or(c.rx_at, |t| t.max(c.rx_at)));
        summary.last_id = Some(summary.last_id.map_or(id, |t| t.max(id)));
    }

    /// The summary, nodes in address order.
    fn finish(self) -> ExportSummary {
        let mut summary = self.summary;
        summary.nodes.sort_by_key(|node| node.mac);
        summary
    }
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
        if best.kind == "ble" {
            summary.ble.rows += 1;
        } else {
            summary.wifi.rows += 1;
        }
    } else {
        summary.unpositioned += 1;
    }
    Ok(())
}

/// Every sighting of every network in the filter, in fold order: address, kind,
/// then time. The `id` tiebreaker keeps the order — and therefore which sighting a
/// tied window submits — deterministic. Columns 0 to 12 are a [`Candidate`]; the last
/// three feed only the [`Tally`]. The position source arrives as `0` for `gps`, `1` for
/// `static` and `2` otherwise rather than as its token: every column here rides through
/// the sort of the whole table, and a small integer is the narrowest thing a sort row can
/// carry, which measured a quarter of what the node and source columns cost the export.
///
/// `?1` is [`ExportFilter::after_uploads`]. The cutoff is an uncorrelated scalar, so SQLite
/// evaluates it once rather than per sighting.
///
/// There is deliberately no index for this to walk: a scan and a sort read the table in
/// order, and were faster than one random lookup per sighting
/// (`docs/store-io-findings.md` says more).
const SELECT_SIGHTINGS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rcoi, mfgr_id, rx_at,
       node_mac, CASE pos_source WHEN 'gps' THEN 0 WHEN 'static' THEN 1 ELSE 2 END, o.id
FROM observation o
WHERE ?1 = 0
   OR o.id > (SELECT COALESCE(MAX(through_id), 0) FROM upload WHERE result IS NOT 'failed')
ORDER BY o.bssid, o.kind, o.rx_at, o.id
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

/// Two windows of one network never open at the same instant, but one address's Wi-Fi
/// and BLE windows can, so `kind` breaks that tie and the order is total.
const SELECT_EXPORT_ROWS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rcoi, mfgr_id, rx_at,
       first_seen
FROM temp.export_row
ORDER BY first_seen, bssid, kind
";

/// The node that heard a [`SELECT_SIGHTINGS`] row, its position source and its id.
fn heard_by(row: &Row<'_>) -> rusqlite::Result<(Mac, i64, i64)> {
    Ok((row.get(13)?, row.get_ref(14)?.as_i64()?, row.get_ref(15)?.as_i64()?))
}

/// One sighting in flight through the fold, carrying everything a submitted
/// row needs so the winner of a window can be written without going back to
/// the database.
struct Candidate {
    bssid: Mac,
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

    /// Take `other`'s roaming consortium and manufacturer identifier wherever this
    /// sighting has none of its own. The fold only pairs sightings of one kind.
    ///
    /// The sighting that wins a window is picked for its position and signal, and
    /// whether a packet carried a trailer has nothing to do with either: a beacon
    /// or probe response may omit the element, and one advertisement of many
    /// carries the manufacturer data. Without this a stronger sighting that lacked
    /// them would blank a column the store can fill — the loss the node's own BLE
    /// ring already guards against by merging a later report's identifier in.
    fn inherit_identifiers(&mut self, other: &mut Self) {
        if self.rcoi.is_none() {
            self.rcoi = other.rcoi.take();
        }
        if self.mfgr_id.is_none() {
            self.mfgr_id = other.mfgr_id;
        }
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
        // Uppercase and colon-separated, as WiGLE and the node firmware's SD logs write it.
        mac_text(&best.bssid),
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
/// The channel is the one the access point announces in its own beacon, or the
/// one the node was parked on when the beacon announces none
/// (`wartui_proto::beacon::parse_mgmt`). An access point can announce a channel
/// no pool tunes, so the column covers the 2.4 GHz channels — 14 the odd one out — and the
/// 5 GHz ladder from 32 to 177. Blank for a channel on neither ladder, and
/// blank for every BLE row, where the column means something only an active
/// inquiry could produce (see the module docs).
fn frequency_column(channel: i64, kind: &str) -> String {
    if kind != "wifi" {
        return String::new();
    }
    match channel {
        1..=13 => format!("{}", 2407 + 5 * channel),
        14 => "2484".to_owned(),
        32..=177 => format!("{}", 5000 + 5 * channel),
        _ => String::new(),
    }
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
    fn timestamp_formatter_pads_zeros_when_formatting_utc_datetime() {
        // The whole reason this function exists. The node firmware emits
        // `2026-5-1 13:34:37` for this instant, which WiGLE rejects.
        assert_eq!(timestamp(1_777_642_477_000), "2026-05-01 13:34:37");
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn csv_quoter_quotes_only_special_characters_when_formatting_fields() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("has,comma"), "\"has,comma\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote(""), "");
    }

    #[test]
    fn mac_formatter_formats_uppercase_hex_with_colons_when_rendering_address() {
        // The `MAC` column WiGLE reads. A change to `mac_text` must not change it.
        assert_eq!(mac_text(&[0x02, 0x00, 0x5E, 0x10, 0x57, 0x84]), "02:00:5E:10:57:84");
    }

    #[test]
    fn frequency_column_computes_mhz_from_wifi_channels_when_generating_export() {
        assert_eq!(frequency_column(1, "wifi"), "2412");
        assert_eq!(frequency_column(6, "wifi"), "2437");
        assert_eq!(frequency_column(13, "wifi"), "2472");
        // Channel 14 is the one 2.4 GHz channel that breaks the 5 MHz ladder.
        assert_eq!(frequency_column(14, "wifi"), "2484");
        // An access point can announce these below the pools' 36.
        assert_eq!(frequency_column(32, "wifi"), "5160");
        assert_eq!(frequency_column(33, "wifi"), "5165");
        assert_eq!(frequency_column(34, "wifi"), "5170");
        assert_eq!(frequency_column(35, "wifi"), "5175");
        assert_eq!(frequency_column(36, "wifi"), "5180");
        assert_eq!(frequency_column(165, "wifi"), "5825");
        assert_eq!(frequency_column(177, "wifi"), "5885");
    }

    #[test]
    fn frequency_column_returns_empty_string_when_given_ble_or_unmapped_channels() {
        // A BLE row's frequency column means a "device type" code a passive scan
        // cannot produce, so it is blank whatever the channel field holds.
        assert_eq!(frequency_column(0, "ble"), "");
        // A channel on neither band's ladder is not something to guess a frequency for.
        assert_eq!(frequency_column(0, "wifi"), "");
        assert_eq!(frequency_column(15, "wifi"), "");
        assert_eq!(frequency_column(31, "wifi"), "");
        assert_eq!(frequency_column(200, "wifi"), "");
    }
}
