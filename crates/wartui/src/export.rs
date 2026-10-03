//! `wartui export` — turn a capture into something WiGLE will take.
//!
//! Separate from the capture because the store is the system of record: an
//! export can be re-run after a decoder fix, run against a capture that ended
//! last week, or run against one that is still going. WAL is what makes that
//! last case safe.
//!
//! Both ends default, so exporting the evening that just ended is `wartui export` and
//! nothing else: the newest capture in the working directory, written out under its own
//! name with `.csv` where the `.db` was. A name chosen that way is never written over —
//! the reader did not pick it, and the file it would replace may be the one already
//! uploaded — so `--out` is how to say another name, or the same one again and mean it.
//!
//! A capture of test data — made with `--sim` or `--lat`/`--lon` — still exports, since
//! testing is what it is for, but with a warning not to submit the CSV anywhere.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Args as ClapArgs;
use wartui_core::export::{DEFAULT_RECAPTURE_SECS, ExportFilter, ExportSummary, wigle_csv};
use wartui_core::store::{Connection, is_simulated, open_readonly};
use wartui_proto::mac;

use crate::capture;

/// Which capture to read and which of its rows to send, shared by `export` and `upload`
/// so both read a capture the same way.
#[derive(ClapArgs, Debug)]
pub struct Selection {
    /// The capture to read. The newest `wartui-<date>.db` in the working
    /// directory by default, which is the one a finished `run` left there.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    /// How long a network's sightings fold into one row, in seconds. One
    /// hour by default, the leaderboard's scan cooldown; `0` writes one row
    /// per network.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_RECAPTURE_SECS)]
    recapture: u64,
}

impl Selection {
    /// The capture to read: `--db`, or the newest in the working directory, named on
    /// standard error after `verb` when it was found rather than given.
    pub(crate) fn capture(&self, verb: &str) -> Result<PathBuf> {
        match &self.db {
            Some(path) => Ok(path.clone()),
            None => {
                let found = capture::newest(Path::new("."))?;
                // Named on the way past, because which capture was picked is otherwise
                // invisible, and the case where that matters is the quiet one: a run that
                // opened its store and then died leaves a file newer than the evening
                // being read, and the result says nothing but `0 rows`.
                // Standard error, because standard output may be the CSV.
                eprintln!("{verb} {}", found.display());
                Ok(found)
            }
        }
    }

    /// The rows `--recapture` asks for.
    pub(crate) fn filter(&self) -> ExportFilter {
        ExportFilter { recapture_secs: self.recapture, after_uploads: false }
    }
}

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,

    /// Where to write the WiGLE CSV. The capture's own name with `.csv` by
    /// default, beside the capture itself; `-` writes to standard output. A
    /// name chosen by default is not written over, so say it here to mean it.
    #[arg(long, short = 'o', value_name = "PATH")]
    out: Option<PathBuf>,
}

pub fn run(args: Args) -> Result<()> {
    let db = args.selection.capture("exporting")?;
    let (out, derived) = match args.out {
        Some(path) => (path, false),
        None => (capture::export_path(&db), true),
    };
    if is_the_capture(&out, &db) {
        bail!(
            "{} is the capture itself, and writing the export there would empty it; \
             name the CSV with --out PATH",
            out.display()
        );
    }
    let conn = open_readonly(&db).with_context(|| format!("opening {}", db.display()))?;
    let warning = test_data_note(&conn, &db)?;
    let filter = args.selection.filter();
    let version = env!("CARGO_PKG_VERSION");

    let summary = if out.as_os_str() == "-" {
        let stdout = std::io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        let summary = wigle_csv(&conn, filter, &mut writer, version)?;
        writer.flush().context("flushing the export")?;
        eprint!("{}", report(&summary, None));
        summary
    } else {
        let summary =
            into_csv(&out, derived, |writer| Ok(wigle_csv(&conn, filter, writer, version)?))?;
        eprint!("{}", report(&summary, Some(&out)));
        summary
    };

    note_unpositioned(&summary);
    if let Some(warning) = warning {
        eprintln!("{warning}");
    }
    Ok(())
}

/// [`test_data_warning`] when the capture holds test data.
fn test_data_note(conn: &Connection, db: &Path) -> Result<Option<String>> {
    let marked = is_simulated(conn).with_context(|| format!("reading {}", db.display()))?;
    Ok(marked.then(|| test_data_warning(db)))
}

/// What `export` says about a capture of test data, whose CSV must go nowhere.
pub(crate) fn test_data_warning(db: &Path) -> String {
    format!(
        "warning: {} holds test data (made with --sim or --lat/--lon). Do not submit this CSV \
         to WiGLE, WDGWars or any other service.",
        db.display()
    )
}

/// Say how many rows had no position and were left out, when any were.
pub(crate) fn note_unpositioned(summary: &ExportSummary) {
    if summary.unpositioned > 0 {
        eprintln!(
            "{} rows were left out because no sighting in their window had a \
             position. Capture with a GPS receiver attached to include them.",
            thousands(summary.unpositioned)
        );
    }
}

/// What the export wrote and what the capture held, for standard error: standard output
/// may be the CSV. `out` is `None` for standard output.
fn report(summary: &ExportSummary, out: Option<&Path>) -> String {
    let dest = out.map_or_else(|| "standard output".to_owned(), |p| p.display().to_string());
    format!("{} rows written to {dest}\n{}", thousands(summary.rows), details(summary))
}

/// Everything [`report`] says after its first line: what the capture held, by kind,
/// position source, node and span.
pub(crate) fn details(summary: &ExportSummary) -> String {
    use std::fmt::Write as _;

    let mut text = String::new();

    let (wifi, bands) = (&summary.wifi, &summary.wifi_bands);
    if wifi.sightings > 0 {
        let other = if bands.other > 0 {
            format!(", other {}", thousands(bands.other))
        } else {
            String::new()
        };
        let _ = writeln!(
            text,
            "  {:<9}{:>8} networks (2.4 GHz {}, 5 GHz {}{other})  {} sightings  {} rows",
            "Wi-Fi",
            thousands(wifi.networks),
            thousands(bands.ghz2_4),
            thousands(bands.ghz5),
            thousands(wifi.sightings),
            thousands(wifi.rows),
        );
    }
    let ble = &summary.ble;
    if ble.sightings > 0 {
        let _ = writeln!(
            text,
            "  {:<9}{:>8} devices  {} sightings  {} rows",
            "BLE",
            thousands(ble.networks),
            thousands(ble.sightings),
            thousands(ble.rows),
        );
    }

    let sightings = wifi.sightings + ble.sightings;
    if sightings == 0 {
        return text;
    }
    let pos = &summary.positions;
    let share = |n: u64| n as f64 * 100.0 / sightings as f64;
    let _ = writeln!(
        text,
        "  {:<11}gps {:.1}%  static {:.1}%  none {:.1}%",
        "positions",
        share(pos.gps),
        share(pos.fixed),
        share(pos.none),
    );

    let heard_wifi = summary.nodes.iter().filter(|n| n.wifi > 0).count();
    let heard_ble = summary.nodes.iter().filter(|n| n.ble > 0).count();
    let mut heard = Vec::new();
    if heard_wifi > 0 {
        heard.push(format!("{heard_wifi} heard Wi-Fi"));
    }
    if heard_ble > 0 {
        heard.push(format!("{heard_ble} heard Bluetooth"));
    }
    let _ = writeln!(text, "  {:<11}{}", "nodes", heard.join(", "));
    for node in &summary.nodes {
        let mut line = format!("    {}", mac::short(&node.mac));
        if node.wifi > 0 {
            let _ = write!(line, "  wifi {}", thousands(node.wifi));
        }
        if node.ble > 0 {
            let _ = write!(line, "  ble {}", thousands(node.ble));
        }
        let _ = writeln!(text, "{line}");
    }

    if let (Some(first), Some(last)) = (summary.first_rx, summary.last_rx) {
        let _ = writeln!(text, "  {:<11}{}", "span", span(first, last));
    }
    text
}

/// `n` with a comma between each group of three digits.
pub(crate) fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// The capture's first and last sighting in UTC, and how long lay between them. The
/// end carries its date only when it is not the start's. The length is measured between
/// the minutes shown, so it agrees with them.
fn span(first_ms: i64, last_ms: i64) -> String {
    let at = |ms| DateTime::<Utc>::from_timestamp_millis(ms).unwrap_or_default();
    let (first, last) = (at(first_ms), at(last_ms));
    let end = if first.date_naive() == last.date_naive() {
        last.format("%H:%M").to_string()
    } else {
        last.format("%Y-%m-%d %H:%M").to_string()
    };
    let minutes = (last_ms.div_euclid(60_000) - first_ms.div_euclid(60_000)).max(0);
    let length = if minutes < 60 {
        format!("{minutes}m")
    } else {
        format!("{}h {}m", minutes / 60, minutes % 60)
    };
    format!("{} – {end} UTC ({length})", first.format("%Y-%m-%d %H:%M"))
}

/// Whether `out` names the capture itself.
///
/// Worth a syscall because the cost of missing it is the evening: the CSV is opened for
/// writing before a row is read, so `--db tonight.db -o tonight.db` — one keystroke from
/// a real command now that `-o` follows `--db` — truncates the store while the export
/// holds it open, and the capture is the one thing here that cannot be taken again.
///
/// Canonicalised, so `./tonight.db` and `tonight.db` are one answer rather than two, and
/// so a link to the capture is recognised as the capture. A path that is not there yet
/// cannot be it, which is the ordinary case and is what the failed canonicalise means.
fn is_the_capture(out: &Path, db: &Path) -> bool {
    match (out.canonicalize(), db.canonicalize()) {
        (Ok(out), Ok(db)) => out == db,
        _ => false,
    }
}

/// Write the CSV at `path` through `write`, taking a name we chose back if it fails.
///
/// The file has to exist before a row can go into it, so a failure part way through —
/// a query that gives up, a full disk — would otherwise leave an empty export under the
/// derived name, and the next run would refuse it. The retry would be blocked by what
/// the fault left behind, which is the one moment the guard must not be in the way.
/// A path the reader typed is left where it failed: they named the file, and what is in
/// it is theirs to look at.
fn into_csv<T>(
    path: &Path,
    derived: bool,
    write: impl FnOnce(&mut BufWriter<std::fs::File>) -> Result<T>,
) -> Result<T> {
    let file = create_csv(path, derived)?;
    let mut writer = BufWriter::new(file);
    let written = write(&mut writer).and_then(|value| {
        writer.flush().context("flushing the export")?;
        Ok(value)
    });
    if written.is_err() && derived {
        let _ = std::fs::remove_file(path);
    }
    written
}

/// Open the CSV, refusing to write over a name this command chose rather than the reader.
///
/// `create_new` rather than asking whether the path is there and then creating it, so
/// nothing can appear in the gap between the two questions. A path the reader typed is
/// truncated as any other tool would: they named the file they meant.
fn create_csv(path: &Path, derived: bool) -> Result<std::fs::File> {
    if !derived {
        return std::fs::File::create(path).with_context(|| format!("creating {}", path.display()));
    }
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(file) => Ok(file),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => bail!(
            "{} already exists, and export will not write over a name it chose itself; \
             name another with --out PATH, or --out {} to replace that one",
            path.display(),
            path.display()
        ),
        Err(e) => Err(e).with_context(|| format!("creating {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use std::path::Path;

    use anyhow::bail;

    use wartui_core::export::{Bands, ExportSummary, KindStats, NodeStats, Positions};

    use super::{
        Args, Selection, create_csv, into_csv, is_the_capture, report, test_data_note,
        test_data_warning,
    };
    use crate::testing::{EPOCH_MS, capture, capture_simulated, sighting};

    /// A capture of two nodes, one on each kind, with sightings over an evening.
    fn evening() -> ExportSummary {
        ExportSummary {
            rows: 47_952,
            unpositioned: 0,
            wifi: KindStats { networks: 38_211, sightings: 3_900_112, rows: 41_002 },
            ble: KindStats { networks: 6_402, sightings: 112_233, rows: 6_950 },
            wifi_bands: Bands { ghz2_4: 30_100, ghz5: 9_804, other: 0 },
            positions: Positions { gps: 3_896_000, fixed: 0, none: 116_345 },
            nodes: vec![
                NodeStats { mac: [0x02, 0, 0x5E, 0x10, 0x1C, 0x5A], wifi: 0, ble: 112_233 },
                NodeStats { mac: [0x02, 0, 0x5E, 0x10, 0x57, 0x84], wifi: 3_900_112, ble: 0 },
            ],
            // 2026-09-30 19:02 to 23:48 UTC.
            first_rx: Some(1_790_794_920_000),
            last_rx: Some(1_790_812_080_000),
            last_id: Some(4_012_345),
        }
    }

    #[test]
    fn export_report_lists_nodes_by_last_two_octets_when_nodes_present() {
        let text = report(&evening(), Some(Path::new("wartui-2026-09-30-19-02-11.csv")));
        assert!(text.contains("\n    1C:5A  ble 112,233\n"), "{text}");
        assert!(text.contains("\n    57:84  wifi 3,900,112\n"), "{text}");
        assert!(text.contains("1 heard Wi-Fi, 1 heard Bluetooth"), "{text}");
        assert!(text.contains("2026-09-30 19:02 – 23:48 UTC (4h 46m)"), "{text}");
    }

    #[test]
    fn export_report_omits_ble_line_when_no_ble_sightings() {
        let mut summary = evening();
        summary.ble = KindStats::default();
        summary.nodes.retain(|n| n.wifi > 0);
        let text = report(&summary, None);
        assert!(text.starts_with("47,952 rows written to standard output\n"), "{text}");
        assert!(!text.contains("BLE"), "{text}");
        assert!(!text.contains("Bluetooth"), "{text}");
    }

    #[test]
    fn export_report_omits_wifi_line_when_no_wifi_sightings() {
        let mut summary = evening();
        summary.wifi = KindStats::default();
        summary.wifi_bands = Bands::default();
        summary.nodes.retain(|n| n.ble > 0);
        let text = report(&summary, None);
        assert!(!text.contains("Wi-Fi"), "{text}");
        assert!(text.contains("  BLE "), "{text}");
    }

    #[test]
    fn export_report_measures_span_between_shown_minutes_when_times_have_seconds() {
        let mut summary = evening();
        // 2026-09-30 19:02:59 to 20:02:01 UTC: 59 minutes apart, but an hour between
        // the minutes shown.
        summary.first_rx = Some(1_790_794_979_000);
        summary.last_rx = Some(1_790_798_521_000);
        let text = report(&summary, None);
        assert!(text.contains("2026-09-30 19:02 – 20:02 UTC (1h 0m)"), "{text}");
    }

    #[test]
    fn export_report_formats_thousands_when_counts_are_large() {
        let text = report(&evening(), Some(Path::new("out.csv")));
        assert!(text.starts_with("47,952 rows written to out.csv\n"), "{text}");
        assert!(
            text.contains(
                "  Wi-Fi      38,211 networks (2.4 GHz 30,100, 5 GHz 9,804)  \
                 3,900,112 sightings  41,002 rows\n"
            ),
            "{text}"
        );
        assert!(
            text.contains("  BLE         6,402 devices  112,233 sightings  6,950 rows"),
            "{text}"
        );
        assert_eq!(super::thousands(0), "0");
        assert_eq!(super::thousands(999), "999");
        assert_eq!(super::thousands(1_000), "1,000");
    }

    #[test]
    fn export_writer_creates_destination_file_when_derived_path_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30-07.csv");
        create_csv(&path, true).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn export_mgr_refuses_overwrite_when_auto_generated_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30-07.csv");
        std::fs::write(&path, "the export that was uploaded").unwrap();
        let error = create_csv(&path, true).unwrap_err().to_string();
        assert!(error.contains("--out"), "{error}");
        assert!(error.contains("wartui-2026-09-18-14-30-07.csv"), "{error}");
        // And the file it refused to open is the one still there.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the export that was uploaded");
    }

    #[test]
    fn export_writer_allows_overwrite_when_destination_path_is_explicitly_specified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tonight.csv");
        std::fs::write(&path, "an export of the capture before").unwrap();
        create_csv(&path, false).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn export_writer_deletes_partial_file_when_auto_generated_export_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30-07.csv");
        let failed: Result<(), _> = into_csv(&path, true, |_| bail!("the query went wrong"));
        assert!(failed.is_err());
        // Nothing left under the name, so the retry that fixes the fault is not
        // refused by what the fault left behind.
        assert!(!path.exists(), "an empty export was left at {}", path.display());
    }

    #[test]
    fn export_writer_preserves_partial_file_when_explicitly_named_export_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tonight.csv");
        let failed: Result<(), _> = into_csv(&path, false, |_| bail!("the query went wrong"));
        assert!(failed.is_err());
        // They named the file, so what happened to it is theirs to look at.
        assert!(path.exists());
    }

    #[test]
    fn export_writer_persists_full_payload_when_export_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30-07.csv");
        let rows = into_csv(&path, true, |writer| {
            writer.write_all(b"rows")?;
            Ok(973)
        })
        .unwrap();
        assert_eq!(rows, 973);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn export_writer_identifies_matching_source_db_when_comparing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("tonight.db");
        std::fs::write(&db, "a capture").unwrap();
        assert!(is_the_capture(&dir.path().join(".").join("tonight.db"), &db));
        assert!(!is_the_capture(&dir.path().join("tonight.csv"), &db));
        // Standard output is not a path at all, and asking the filesystem says so.
        assert!(!is_the_capture(Path::new("-"), &db));
    }

    #[test]
    fn export_writes_rows_when_capture_holds_test_data() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))];
        let db = capture_simulated(&dir, records, true);
        let out = dir.path().join("tonight.csv");
        let args =
            Args { selection: Selection { db: Some(db), recapture: 0 }, out: Some(out.clone()) };
        super::run(args).unwrap();
        let csv = std::fs::read_to_string(&out).unwrap();
        assert!(csv.contains("10:00:00:00:00:01"), "{csv}");
    }

    #[test]
    fn export_warns_when_capture_holds_test_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture_simulated(&dir, Vec::new(), true);
        let conn = wartui_core::store::open_readonly(&db).unwrap();
        assert_eq!(test_data_note(&conn, &db).unwrap(), Some(test_data_warning(&db)));
    }

    #[test]
    fn export_stays_quiet_when_capture_holds_no_test_data() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, Vec::new());
        let conn = wartui_core::store::open_readonly(&db).unwrap();
        assert_eq!(test_data_note(&conn, &db).unwrap(), None);
    }

    #[test]
    fn export_names_capture_and_services_when_warning_of_test_data() {
        assert_eq!(
            test_data_warning(Path::new("tonight.db")),
            "warning: tonight.db holds test data (made with --sim or --lat/--lon). Do not \
             submit this CSV to WiGLE, WDGWars or any other service."
        );
    }
}
