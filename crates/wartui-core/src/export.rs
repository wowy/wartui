//! WiGLE CSV export.
//!
//! WiGLE's v1.4 file is one row per network, so the interesting work is
//! choosing which of a network's many sightings to submit. The store keeps
//! every one — several nodes may see the same access point, repeatedly, from
//! different places — and the row that goes out is the one with the strongest
//! signal, because that is the sighting whose position is closest to the
//! transmitter. `FirstSeen` still comes from the earliest sighting of the
//! network, which is a different row and the reason this is a window query
//! rather than a `GROUP BY`.
//!
//! Two details are here because WiGLE rejects files without them: the timestamp
//! must be zero-padded (`2026-05-01 13:34:37`, where the node firmware emits
//! `2026-5-1 13:34:37`), and the SSID must be RFC-4180 quoted, since an SSID
//! may contain a comma or a quote and is not required to be text at all.

use std::io::Write;

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, Row};

/// The pre-header WiGLE reads for provenance, then the column header.
const COLUMNS: &str = "MAC,SSID,AuthMode,FirstSeen,Channel,RSSI,\
CurrentLatitude,CurrentLongitude,AltitudeMeters,AccuracyMeters,Type";

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
#[derive(Debug, Clone, Copy, Default)]
pub struct ExportFilter {
    /// Only this session. `None` exports every session in the database, which
    /// is usually what is wanted: WiGLE deduplicates on its own side and more
    /// sightings of a network is strictly better data.
    pub session_id: Option<i64>,
}

/// What an export did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Networks written.
    pub networks: u64,
    /// Networks left out because no sighting of them had a position.
    ///
    /// Not an error and not silent: WiGLE cannot use a row without
    /// coordinates, but the operator should know how much of the capture is
    /// waiting on a GPS or a `--lat`/`--lon`.
    pub unpositioned: u64,
}

/// Write a WiGLE v1.4 CSV.
///
/// # Errors
/// [`ExportError`] if the query or the write fails.
pub fn wigle_csv<W: Write>(
    conn: &Connection,
    filter: ExportFilter,
    out: &mut W,
    app_version: &str,
) -> Result<ExportSummary, ExportError> {
    let mut summary = ExportSummary { networks: 0, unpositioned: unpositioned(conn, filter)? };

    writeln!(
        out,
        "WigleWifi-1.4,appRelease={app_version},model=wartui,release={app_version},\
         device=wartui,display=,board=ESP32-C5,brand=wartui"
    )?;
    writeln!(out, "{COLUMNS}")?;

    let mut stmt = conn.prepare(SELECT_NETWORKS)?;
    let mut rows = stmt.query(rusqlite::params![filter.session_id])?;
    while let Some(row) = rows.next()? {
        write_row(row, out)?;
        summary.networks += 1;
    }
    Ok(summary)
}

/// One row per BSSID: the strongest positioned sighting, with `FirstSeen` taken
/// from the earliest sighting of that BSSID — positioned or not.
///
/// Both halves of that need the window to run over the *unfiltered* set. If the
/// position filter were applied first, `FirstSeen` would report the earliest
/// sighting that happened to have coordinates, so an unpositioned session at
/// 20:00 followed by a positioned one at 23:00 would submit 23:00 and hide
/// three hours of evidence that the network was already there.
///
/// Ordering positioned rows ahead of unpositioned ones in `ROW_NUMBER` is what
/// lets the outer filter be safe: without it the strongest sighting could be an
/// unpositioned one, win rn = 1, and then be filtered out — losing a network
/// that had a perfectly good weaker sighting to submit.
///
/// The `?1 IS NULL OR session_id = ?1` shape lets one statement serve both the
/// filtered and unfiltered cases without building SQL by hand.
const SELECT_NETWORKS: &str = r"
SELECT bssid, ssid, security, first_seen, channel, rssi, lat, lon, alt, accuracy, kind
FROM (
  SELECT
    o.bssid, o.ssid, o.security, o.channel, o.rssi, o.lat, o.lon, o.alt, o.accuracy, o.kind,
    MIN(o.rx_at) OVER (PARTITION BY o.bssid) AS first_seen,
    ROW_NUMBER() OVER (
      PARTITION BY o.bssid
      ORDER BY (o.lat IS NOT NULL AND o.lon IS NOT NULL) DESC, o.rssi DESC, o.rx_at ASC
    ) AS rn
  FROM observation o
  WHERE ?1 IS NULL OR o.session_id = ?1
)
WHERE rn = 1 AND lat IS NOT NULL AND lon IS NOT NULL
ORDER BY first_seen
";

/// Networks with no positioned sighting at all.
fn unpositioned(conn: &Connection, filter: ExportFilter) -> Result<u64, rusqlite::Error> {
    conn.query_row(
        r"SELECT COUNT(*) FROM (
            SELECT bssid FROM observation
            WHERE (?1 IS NULL OR session_id = ?1)
            GROUP BY bssid
            HAVING SUM(lat IS NOT NULL AND lon IS NOT NULL) = 0
          )",
        rusqlite::params![filter.session_id],
        |row| row.get(0),
    )
}

fn write_row<W: Write>(row: &Row<'_>, out: &mut W) -> Result<(), ExportError> {
    let bssid: Vec<u8> = row.get(0)?;
    let ssid: Option<Vec<u8>> = row.get(1)?;
    let security: String = row.get(2)?;
    let first_seen: i64 = row.get(3)?;
    let channel: i64 = row.get(4)?;
    let rssi: i64 = row.get(5)?;
    let lat: f64 = row.get(6)?;
    let lon: f64 = row.get(7)?;
    let alt: Option<f64> = row.get(8)?;
    let accuracy: Option<f64> = row.get(9)?;
    let kind: String = row.get(10)?;

    writeln!(
        out,
        "{},{},{},{},{channel},{rssi},{lat},{lon},{},{},{}",
        mac(&bssid),
        quote(&String::from_utf8_lossy(ssid.as_deref().unwrap_or_default())),
        quote(&security),
        timestamp(first_seen),
        alt.unwrap_or(0.0),
        accuracy.unwrap_or(0.0),
        if kind == "ble" { "BLE" } else { "WIFI" },
    )?;
    Ok(())
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
        assert_eq!(mac(&[0x38, 0x44, 0xbe, 0x1f, 0x57, 0x84]), "38:44:BE:1F:57:84");
    }
}
