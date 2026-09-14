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
//! order, holding one window's state at a time and collecting the rows it
//! submits for the sort at the end, so memory grows with the rows written and
//! never with the sightings read; the store stays the system of record and
//! nothing here writes back.
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
//! `RCOIs` and `MfgrId` are blank because nothing in the pipeline yet collects a
//! roaming consortium identifier or a Bluetooth manufacturer ID; the columns are
//! shaped so that capturing them later is a fill-in rather than a re-format. A BLE
//! row's `Frequency` stays blank on purpose: what WiGLE asks for there is a
//! Bluetooth "device type" code, a class-of-device value that only an active inquiry
//! produces, and a node that never transmits while scanning has none to report.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, Row};

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

    let mut stmt = conn.prepare(SELECT_SIGHTINGS)?;
    let mut rows = stmt.query(rusqlite::params![filter.session_id])?;

    let mut submitted: Vec<Submitted> = Vec::new();
    let mut unpositioned = 0u64;
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
            close(&mut window, &mut submitted, &mut unpositioned);
            window = Some(Window { first_seen: candidate.rx_at, best: candidate });
        } else if let Some(w) = window.as_mut()
            && candidate.submits_over(&w.best)
        {
            w.best = candidate;
        }
    }
    close(&mut window, &mut submitted, &mut unpositioned);

    // By when each row's window opened, so a file re-exported after a decoder
    // fix diffs cleanly against the one before it; the network breaks ties.
    submitted.sort_by(|a, b| (a.first_seen, &a.best.bssid).cmp(&(b.first_seen, &b.best.bssid)));
    for row in &submitted {
        write_row(row, out)?;
    }
    Ok(ExportSummary { rows: submitted.len() as u64, unpositioned })
}

/// Retire the window in flight, submitting it if its best sighting is
/// positioned and counting it if not.
fn close(window: &mut Option<Window>, submitted: &mut Vec<Submitted>, unpositioned: &mut u64) {
    let Some(w) = window.take() else { return };
    if w.best.positioned() {
        submitted.push(Submitted { first_seen: w.first_seen, best: w.best });
    } else {
        *unpositioned += 1;
    }
}

/// Every sighting of every network in the filter, in fold order: network, then
/// time. The `id` tiebreaker keeps the order — and therefore which sighting a
/// tied window submits — deterministic.
///
/// There is deliberately no index for this to walk: a scan and a sort read the table in
/// order, and were faster than one random lookup per sighting (the store's v6 migration
/// says more).
const SELECT_SIGHTINGS: &str = r"
SELECT bssid, ssid, security, channel, rssi, lat, lon, alt, accuracy, kind, rx_at
FROM observation o
WHERE ?1 IS NULL OR o.session_id = ?1
ORDER BY o.bssid, o.rx_at, o.id
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
            rx_at: row.get(10)?,
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

/// A window that closed on a positioned sighting, waiting for its turn in the
/// file: rows are ordered by when their windows opened, not by network.
struct Submitted {
    first_seen: i64,
    best: Candidate,
}

fn write_row<W: Write>(row: &Submitted, out: &mut W) -> Result<(), ExportError> {
    let best = &row.best;
    let channel = best.channel;
    let rssi = best.rssi;
    let lat = best.lat.unwrap_or(0.0);
    let lon = best.lon.unwrap_or(0.0);
    let frequency = frequency_column(channel, &best.kind);

    writeln!(
        out,
        "{},{},{},{},{channel},{frequency},{rssi},{lat},{lon},{},{},,,{}",
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
