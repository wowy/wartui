//! WiGLE CSV export, in the v1.6 file format.
//!
//! A network's sightings fold into recapture windows, one row each: the strongest positioned
//! sighting, with `FirstSeen` from the window's first sighting, positioned or not. Those can be
//! different sightings, so the fold carries both. A window opens at its first sighting and closes
//! on the first sighting more than [`ExportFilter::recapture_secs`] later. A network watched for
//! three hours yields a row an hour. One row in all would score one capture on a leaderboard that
//! counts re-captures, and a row per sighting WiGLE would deduplicate anyway. `0` gives one row per
//! network for the whole capture.
//!
//! A network is an address and a kind. One address can be both a Wi-Fi network and a BLE
//! advertiser, which WiGLE records as two things (the `Type` column), so each kind has its own
//! windows and lends identifiers only to its own row. A sighting stored with any other kind is left
//! out, counted in [`ExportSummary::unknown_kind`] and warned about once.
//!
//! The fold is in Rust, not SQL, because the anchor rule is sequential: where one window ends
//! decides where the next begins, which no window function computes without recursion. It streams
//! sightings in address, kind, then time order, holding one window at a time.
//!
//! The file is ordered by window start, not network, and SQLite does that sort: submitted rows go
//! into a temporary table on the export's connection and come back in order. SQLite spills past its
//! page cache to a temporary file, so a longer capture costs temporary disk, not memory. In a
//! `Vec`, a full drive's rows took 183 MiB (`docs/store-io-findings.md`). The file goes to
//! `SQLITE_TMPDIR`, then `TMPDIR`, then `/var/tmp`, which on a Pi booting from its card is the
//! card. The store is not written: a temporary table belongs to the connection.
//!
//! The [`ExportSummary`] counts are exact, because the fold walks networks in order and knows where
//! each begins. Their cost is the node and position-source columns riding through the sort
//! ([`SELECT_SIGHTINGS`]).
//!
//! WiGLE rejects files without two details: a zero-padded timestamp (`2026-05-01 13:34:37`, where
//! the node firmware emits `2026-5-1 13:34:37`), and RFC-4180 quoting of the SSID, which may hold a
//! comma or quote and need not be text. [`record::ssid_text`](crate::record::ssid_text) also strips
//! a cloaked network's NUL padding on the way out, so no SSID reaches the file padded.
//!
//! [`ExportFilter::after_uploads`] leaves out everything the newest non-failed upload covered, so a
//! repeat upload sends only what came after. The cutoff is the last sighting that upload walked, in
//! stored order. Ids follow commit order, so a frame split across two commits is cut exactly, and a
//! capture still being written loses nothing. The fold restarts at the cutoff, so a network heard
//! on both sides within the recapture width gets a second row, which WDGWars skips when scoring. An
//! unpositioned sighting before the cutoff counts as sent, so a later fix does not bring it back.
//!
//! The columns v1.6 added over v1.4 are derived or blank, never stored. `Frequency` is a function
//! of the stored channel. `RCOIs` and `MfgrId` carry a Passpoint access point's roaming consortium
//! identifiers and a BLE advertiser's manufacturer identifier. They are blank when the capture
//! holds none: the store's NULL becomes a blank without claiming the beacon carried nothing. A BLE
//! row's `Frequency` is blank on purpose: WiGLE wants a Bluetooth class-of-device code there, which
//! only an active inquiry produces, and a node never transmits while scanning.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, Row, Statement};
use wartui_proto::air::RecordKind;
use wartui_proto::beacon::rcoi_text;
use wartui_proto::mac::{self, Mac};

use crate::record::ssid_text;
use crate::store::{kind_name, parse_kind};

/// The pre-header WiGLE reads for provenance, then the column header.
const COLUMNS: &str = "MAC,SSID,AuthMode,FirstSeen,Channel,Frequency,RSSI,\
CurrentLatitude,CurrentLongitude,AltitudeMeters,AccuracyMeters,RCOIs,MfgrId,Type";

/// The default recapture window: exactly one hour.
///
/// WDGWars, the leaderboard this default is cut for, scores a network once per hour per user:
/// "re-scanning the same AP within 1h is silently skipped from scoring; GPS may still be refined".
/// This is that cooldown verbatim. The site is the authority on its rule, and the export's job is
/// to say when the AP was scanned. A re-hearing within the hour stays in the row already submitted,
/// where a stronger reading can still refine its position, as the rule allows. The hour is
/// inclusive like the rule's: only a sighting *past* the width opens the next window. Less would
/// write rows the site skips. More would fold away re-hearings it counts.
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
    /// How long after a window opened a sighting still belongs to it, in seconds. `0` folds a
    /// network's whole capture into one row.
    pub recapture_secs: u64,
    /// Leave out sightings covered by the newest non-failed upload. Off by default, so `export` and
    /// `analyze` read the whole capture.
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
    /// Rows written: one per network per window with a positioned sighting.
    pub rows: u64,
    /// Rows left out because no sighting in their window had a position. Not an error, and not
    /// silent: the operator should know how much is waiting on a GPS.
    pub unpositioned: u64,
    /// Sightings left out because their stored kind is neither Wi-Fi nor BLE. They count towards
    /// nothing else here except [`Self::last_id`].
    pub unknown_kind: u64,
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
    /// The earliest sighting's receive time, in unix milliseconds, or `None` with no sightings.
    pub first_rx: Option<i64>,
    /// The latest sighting's receive time, in unix milliseconds.
    pub last_rx: Option<i64>,
    /// The highest observation id walked, which is what an upload records as its cutoff.
    pub last_id: Option<i64>,
}

/// One record kind's share of an export.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KindStats {
    /// Distinct addresses heard as this kind. One heard as both counts once in each.
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
    // `star=Sol,body=3,subBody=0` is Earth in the pre-header's notation: the third orbit, no
    // satellite.
    writeln!(
        out,
        "WigleWifi-1.6,appRelease={app_version},model=wartui,release={app_version},\
         device=wartui,display=,board=ESP32-C5,brand=wartui,star=Sol,body=3,subBody=0"
    )?;
    writeln!(out, "{COLUMNS}")?;

    // A window wide enough to overflow the millisecond clock means no window.
    let recapture_ms =
        i64::try_from(filter.recapture_secs.saturating_mul(1000)).unwrap_or(i64::MAX);

    // Set before the table exists, since changing it discards temporary tables. The bundled build
    // defaults to a file, and the memory bound rests on it.
    conn.pragma_update(None, "temp_store", "FILE")?;
    // One transaction, so the inserts are one commit and a failed export rolls back the table too.
    let tx = conn.unchecked_transaction()?;
    tx.execute_batch(CREATE_EXPORT_ROWS)?;

    let mut tally = Tally::default();
    {
        let mut insert = tx.prepare(INSERT_EXPORT_ROW)?;
        let mut stmt = tx.prepare(SELECT_SIGHTINGS)?;
        let mut rows = stmt.query([filter.after_uploads])?;
        let mut window: Option<Window> = None;

        while let Some(row) = rows.next()? {
            let (node, pos_source, id) = heard_by(row)?;
            let Some(mut candidate) = Candidate::of(row)? else {
                tally.skip(id);
                continue;
            };
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
                // Position and signal alone pick the submitted sighting. The identifiers are the
                // window's, so a loser still lends them.
                if candidate.submits_over(&w.best) {
                    candidate.inherit_identifiers(&mut w.best);
                    w.best = candidate;
                } else {
                    w.best.inherit_identifiers(&mut candidate);
                }
            }
        }
        close(&mut window, &mut insert, &mut tally.summary)?;
        if tally.summary.unknown_kind > 0 {
            tracing::warn!(
                sightings = tally.summary.unknown_kind,
                "left out sightings whose kind is neither wifi nor ble"
            );
        }

        // By window start, so a re-export after a decoder fix diffs cleanly. The network breaks
        // ties.
        let mut sorted = tx.prepare(SELECT_EXPORT_ROWS)?;
        let mut rows = sorted.query([])?;
        while let Some(row) = rows.next()? {
            // Every row here was written by `close` with `kind_name`, so it parses.
            let Some(best) = Candidate::of(row)? else { continue };
            write_row(&Window { first_seen: row.get(13)?, best }, out)?;
        }
    }
    tx.execute_batch("DROP TABLE temp.export_row")?;
    tx.commit()?;
    Ok(tally.finish())
}

/// Counts what the export walked over, for its [`ExportSummary`].
///
/// Exact, because the fold walks networks (address and kind) in order and says where each begins.
/// Memory is bounded by the fleet, not the capture: one entry per reporting node, and band bits
/// reset per network. Nothing allocates per sighting.
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
        let ble = c.kind == RecordKind::Ble;
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

    /// Count a sighting stored as `id` whose kind did not parse. It still moves
    /// [`ExportSummary::last_id`], so an upload's cutoff covers it rather than walking it again.
    fn skip(&mut self, id: i64) {
        let summary = &mut self.summary;
        summary.unknown_kind += 1;
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
            kind_name(best.kind),
            best.rcoi,
            best.mfgr_id,
            best.rx_at,
            first_seen,
        ])?;
        summary.rows += 1;
        match best.kind {
            RecordKind::Wifi => summary.wifi.rows += 1,
            RecordKind::Ble => summary.ble.rows += 1,
        }
    } else {
        summary.unpositioned += 1;
    }
    Ok(())
}

/// Every sighting in the filter, in fold order: address, kind, then time, with `id` breaking ties
/// so the submitted sighting is deterministic. Columns 0 to 12 are a [`Candidate`], and the last
/// three feed only the [`Tally`]. The position source arrives as `0` for `gps`, `1` for `static`
/// and `2` otherwise: every column rides through the whole-table sort, and a small integer is the
/// narrowest a sort row carries. That measured a quarter of what the node and source columns cost
/// the export.
///
/// `?1` is [`ExportFilter::after_uploads`], an uncorrelated scalar evaluated once, not per
/// sighting. No index on purpose: a scan and a sort beat one random lookup per sighting
/// (`docs/store-io-findings.md`).
const SELECT_SIGHTINGS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rcoi, mfgr_id, rx_at,
       node_mac, CASE pos_source WHEN 'gps' THEN 0 WHEN 'static' THEN 1 ELSE 2 END, o.id
FROM observation o
WHERE ?1 = 0
   OR o.id > (SELECT COALESCE(MAX(through_id), 0) FROM upload WHERE result IS NOT 'failed')
ORDER BY o.bssid, o.kind, o.rx_at, o.id
";

/// Rows waiting for the sort by window start: a submitted sighting in [`SELECT_SIGHTINGS`]'s column
/// order, for [`Candidate::of`] to read back, then its window's start. Dropped first in case an
/// earlier export on this connection left one.
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

/// One sighting in flight through the fold, carrying all a submitted row needs, so the winner is
/// written without another query.
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
    kind: RecordKind,
    rcoi: Option<Vec<u8>>,
    mfgr_id: Option<u16>,
    rx_at: i64,
}

impl Candidate {
    /// Read one sighting off a query row, or `None` when its kind does not parse.
    fn of(row: &Row<'_>) -> rusqlite::Result<Option<Self>> {
        let Some(kind) = row.get_ref(9)?.as_str().ok().and_then(parse_kind) else {
            return Ok(None);
        };
        Ok(Some(Self {
            bssid: row.get(0)?,
            ssid: row.get(1)?,
            security: row.get(2)?,
            channel: row.get(3)?,
            rssi: row.get(4)?,
            lat: row.get(5)?,
            lon: row.get(6)?,
            alt: row.get(7)?,
            accuracy: row.get(8)?,
            kind,
            rcoi: row.get(10)?,
            mfgr_id: row.get(11)?,
            rx_at: row.get(12)?,
        }))
    }

    /// Take `other`'s roaming consortium and manufacturer identifier where this sighting has none.
    /// The fold only pairs sightings of one kind.
    ///
    /// The winner is picked by position and signal, which have nothing to do with trailers: a
    /// beacon or probe response may omit the element, and only some advertisements carry
    /// manufacturer data. Without this, a stronger sighting lacking them would blank a column the
    /// store can fill, the loss the node's BLE ring already guards against by merging in a later
    /// report's identifier.
    fn inherit_identifiers(&mut self, other: &mut Self) {
        if self.rcoi.is_none() {
            self.rcoi = other.rcoi.take();
        }
        if self.mfgr_id.is_none() {
            self.mfgr_id = other.mfgr_id;
        }
    }

    /// Whether this sighting has coordinates for a WiGLE row.
    const fn positioned(&self) -> bool {
        self.lat.is_some() && self.lon.is_some()
    }

    /// Whether to submit this sighting over `other`: positioned beats unpositioned, then stronger
    /// signal, then earlier. Position first keeps a network's one good weaker sighting from losing
    /// to a stronger unfixed one. The strongest signal is the closest to the transmitter.
    fn submits_over(&self, other: &Self) -> bool {
        match (self.positioned(), other.positioned()) {
            (true, false) => true,
            (false, true) => false,
            _ => self.rssi > other.rssi || (self.rssi == other.rssi && self.rx_at < other.rx_at),
        }
    }
}

/// The fold's state for one network's window: when it opened, and the sighting it would submit now.
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
    let frequency = frequency_column(channel, best.kind);
    // WiGLE's spellings: the roaming consortium body as hex identifiers, the company identifier as
    // a number. NULL in the store is a blank here.
    let rcois = best.rcoi.as_deref().map_or_else(String::new, |body| rcoi_text(body).to_string());
    let mfgr = best.mfgr_id.map_or_else(String::new, |id| id.to_string());

    writeln!(
        out,
        "{},{},{},{},{channel},{frequency},{rssi},{lat},{lon},{},{},{rcois},{mfgr},{}",
        // Uppercase and colon-separated, as WiGLE and the node firmware's SD logs write it.
        mac::full(&best.bssid),
        quote(&ssid_text(best.ssid.as_deref().unwrap_or_default())),
        quote(&best.security),
        timestamp(row.first_seen),
        best.alt.unwrap_or(0.0),
        best.accuracy.unwrap_or(0.0),
        match best.kind {
            RecordKind::Wifi => "WIFI",
            RecordKind::Ble => "BLE",
        },
    )?;
    Ok(())
}

/// The centre frequency of a sighting's channel, for WiGLE's `Frequency` column, derived from the
/// stored channel.
///
/// The channel is the one the access point announces, or the node's parked channel when it
/// announces none (`wartui_proto::beacon::parse_mgmt`). An access point can announce a channel no
/// pool tunes, so this covers the 2.4 GHz channels (14 the odd one out) and the 5 GHz ladder from
/// 32 to 177. Blank off both ladders, and for every BLE row (see the module docs).
fn frequency_column(channel: i64, kind: RecordKind) -> String {
    if kind == RecordKind::Ble {
        return String::new();
    }
    match channel {
        1..=13 => format!("{}", 2407 + 5 * channel),
        14 => "2484".to_owned(),
        32..=177 => format!("{}", 5000 + 5 * channel),
        _ => String::new(),
    }
}

/// Zero-padded UTC, to the second. Built from a real timestamp, not by reformatting the firmware's
/// `2026-5-1 13:34:37`, which WiGLE rejects.
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
    fn timestamp_zero_pads_fields_when_month_day_or_time_single_digit() {
        // The whole reason this function exists. The node firmware emits
        // `2026-5-1 13:34:37` for this instant, which WiGLE rejects.
        assert_eq!(timestamp(1_777_642_477_000), "2026-05-01 13:34:37");
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn quote_quotes_only_when_needed() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("has,comma"), "\"has,comma\"");
        assert_eq!(quote("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(quote(""), "");
    }

    #[test]
    fn mac_full_writes_uppercase_colon_hex_when_formatting_bssid() {
        // The `MAC` column WiGLE reads. A change to `mac::full` must not change it.
        assert_eq!(
            mac::full(&[0x02, 0x00, 0x5E, 0x10, 0x57, 0x84]).to_string(),
            "02:00:5E:10:57:84"
        );
    }

    #[test]
    fn frequency_column_gives_centre_mhz_when_wifi_channel_on_a_ladder() {
        assert_eq!(frequency_column(1, RecordKind::Wifi), "2412");
        assert_eq!(frequency_column(6, RecordKind::Wifi), "2437");
        assert_eq!(frequency_column(13, RecordKind::Wifi), "2472");
        // Channel 14 is the one 2.4 GHz channel that breaks the 5 MHz ladder.
        assert_eq!(frequency_column(14, RecordKind::Wifi), "2484");
        // An access point can announce these below the pools' 36.
        assert_eq!(frequency_column(32, RecordKind::Wifi), "5160");
        assert_eq!(frequency_column(33, RecordKind::Wifi), "5165");
        assert_eq!(frequency_column(34, RecordKind::Wifi), "5170");
        assert_eq!(frequency_column(35, RecordKind::Wifi), "5175");
        assert_eq!(frequency_column(36, RecordKind::Wifi), "5180");
        assert_eq!(frequency_column(165, RecordKind::Wifi), "5825");
        assert_eq!(frequency_column(177, RecordKind::Wifi), "5885");
    }

    #[test]
    fn frequency_column_is_blank_when_ble_or_channel_unmapped() {
        // A BLE row's frequency column means a "device type" code a passive scan
        // cannot produce, so it is blank whatever the channel field holds.
        assert_eq!(frequency_column(0, RecordKind::Ble), "");
        // A channel on neither band's ladder is not something to guess a frequency for.
        assert_eq!(frequency_column(0, RecordKind::Wifi), "");
        assert_eq!(frequency_column(15, RecordKind::Wifi), "");
        assert_eq!(frequency_column(31, RecordKind::Wifi), "");
        assert_eq!(frequency_column(200, RecordKind::Wifi), "");
    }
}
