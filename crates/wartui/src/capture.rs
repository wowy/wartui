//! What a capture is called, and how `export` finds the last one.
//!
//! A capture is a wardriving run, and the file is named for the minute the run started, so
//! a directory of them reads as a log rather than as one file every run appends to. Two
//! runs begun inside the same minute do share a name, and the second adds its session to
//! the first one's file — what [`wartui_core::store::Store::open`] does for any path
//! given twice, and what `--db` is for. The date is ISO order — year, month, day — whatever order the reader's locale
//! would put them in: a name that begins with the year and continues in the operator's
//! own order sorts wrong for half the world and reads as an impossible date for the
//! other half. In ISO order the names sort chronologically as text, which is the whole
//! of [`newest`], and that is what lets `export` reach for the capture `run` just wrote
//! without the two commands sharing a literal.
//!
//! The clock is local, because the question asked of a filename is which evening it is
//! rather than which UTC instant. Nothing inside the capture is: the store keeps unix
//! millis and the WiGLE export is UTC. Nothing reads the stamp back out of a name except
//! [`newest`], comparing names written on the one host — and the repeated hour of a
//! daylight-saving fall-back is the one place those names are out of order, so two runs
//! an hour apart across it sort the wrong way round. An offset in the name would settle
//! it and would cost every other name the width.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Local, NaiveDateTime};

/// What every capture's name starts with.
const PREFIX: &str = "wartui-";

/// And ends with.
const SUFFIX: &str = ".db";

/// The stamp in between, zero-padded throughout — which is what makes the names sort.
const STAMP: &str = "%Y-%m-%d-%H-%M";

/// The name a capture started at `started` is given.
///
/// Takes the time rather than reading a clock, for the same reason the engine does:
/// what it writes is then a fact a test can state.
pub fn dated_path(started: DateTime<Local>) -> PathBuf {
    PathBuf::from(format!("{PREFIX}{}{SUFFIX}", started.format(STAMP)))
}

/// The CSV a capture at `db` exports to: its own name, with `.csv` where the `.db` was.
///
/// Beside the capture rather than in the working directory, so a capture copied off the
/// card exports next to itself rather than wherever the export was run from. A `--db`
/// named without an extension gains one; a `--db` that already ends in `.csv` comes back
/// unchanged, and `export` refuses that rather than writing an export over a capture.
pub fn export_path(db: &Path) -> PathBuf {
    let mut csv = db.to_path_buf();
    csv.set_extension("csv");
    csv
}

/// The most recent capture [`dated_path`] could have written into `dir`.
///
/// The names sort chronologically, so the answer is the greatest of them. By name and
/// not by modification time: a capture copied off the card keeps the evening it records
/// in its name and loses it from its timestamps.
pub fn newest(dir: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))?;
    let latest = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_str().is_some_and(is_capture_name))
        // Only after the name matches, because this is a `stat` each: a directory
        // someone archived a capture into carries the name without being one, and
        // handing it to the reader costs them SQLite's account of the failure
        // rather than this module's.
        .filter(|entry| entry.path().is_file())
        .map(|entry| entry.file_name())
        .max();
    match latest {
        Some(name) => Ok(dir.join(name)),
        None => {
            bail!("no {PREFIX}<date>{SUFFIX} in the working directory; name the capture with --db")
        }
    }
}

/// Whether `name` is one [`dated_path`] could have produced.
///
/// The stamp is parsed and written back out rather than only parsed: chrono reads an
/// unpadded `2026-9-8` for `%Y-%m-%d` quite happily, and a name spelled that way sorts
/// above every padded one. Only the canonical spelling is a capture.
fn is_capture_name(name: &str) -> bool {
    let Some(stamp) = name.strip_prefix(PREFIX).and_then(|rest| rest.strip_suffix(SUFFIX)) else {
        return false;
    };
    NaiveDateTime::parse_from_str(stamp, STAMP)
        .is_ok_and(|parsed| parsed.format(STAMP).to_string() == stamp)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use chrono::{DateTime, Local, NaiveDate};

    use super::{dated_path, export_path, newest};

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> DateTime<Local> {
        NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|day| day.and_hms_opt(hour, minute, 0))
            .expect("a real date")
            .and_local_timezone(Local)
            .single()
            .expect("an unambiguous local time")
    }

    #[test]
    fn capture_mgr_formats_filename_from_timestamp_when_session_starts() {
        assert_eq!(dated_path(at(2026, 9, 18, 14, 30)), Path::new("wartui-2026-09-18-14-30.db"));
    }

    #[test]
    fn capture_mgr_sorts_records_in_chronological_order_when_listing_sessions() {
        let written: Vec<_> = [
            at(2025, 12, 31, 23, 59),
            at(2026, 1, 1, 0, 0),
            at(2026, 9, 8, 9, 5),
            at(2026, 9, 18, 14, 30),
        ]
        .into_iter()
        .map(dated_path)
        .collect();
        let mut sorted = written.clone();
        sorted.sort();
        assert_eq!(written, sorted);
    }

    #[test]
    fn capture_mgr_locates_most_recent_valid_capture_file_when_querying_directory() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "wartui-2026-09-18-14-30.db",
            "wartui-2026-09-08-09-05.db",
            "wartui-2025-12-31-23-59.db",
            // Named by hand rather than for a minute: neither is the answer.
            "tonight.db",
            "wartui-notes.db",
            // Unpadded, so it would sort above every real one. Not a name `run` writes.
            "wartui-2026-9-8-9-5.db",
        ] {
            std::fs::File::create(dir.path().join(name)).unwrap();
        }
        // Carries the newest name of all and is not a capture.
        std::fs::create_dir(dir.path().join("wartui-2026-12-25-00-00.db")).unwrap();
        assert_eq!(newest(dir.path()).unwrap(), dir.path().join("wartui-2026-09-18-14-30.db"));
    }

    #[test]
    fn capture_mgr_finds_matching_filename_when_exporting_session() {
        let dir = tempfile::tempdir().unwrap();
        let written = dir.path().join(dated_path(at(2026, 9, 18, 14, 30)));
        std::fs::File::create(&written).unwrap();
        assert_eq!(newest(dir.path()).unwrap(), written);
    }

    #[test]
    fn capture_mgr_reports_error_referencing_db_flag_when_directory_has_no_captures() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::File::create(dir.path().join("out.csv")).unwrap();
        let error = newest(dir.path()).unwrap_err().to_string();
        assert!(error.contains("--db"), "{error}");
    }

    #[test]
    fn capture_mgr_derives_adjacent_csv_path_when_given_db_path() {
        let db = dated_path(at(2026, 9, 18, 14, 30));
        assert_eq!(export_path(&db), Path::new("wartui-2026-09-18-14-30.csv"));
    }

    #[test]
    fn capture_mgr_preserves_parent_directory_when_deriving_csv_path() {
        assert_eq!(export_path(Path::new("/cards/tonight.db")), Path::new("/cards/tonight.csv"));
    }

    #[test]
    fn capture_mgr_exports_to_specified_csv_when_destination_path_is_given() {
        // Which `export` refuses rather than acting on; see its `is_the_capture`.
        assert_eq!(export_path(Path::new("tonight.csv")), Path::new("tonight.csv"));
    }

    #[test]
    fn capture_mgr_appends_csv_extension_when_input_path_lacks_extension() {
        assert_eq!(export_path(Path::new("tonight")), Path::new("tonight.csv"));
    }
}
