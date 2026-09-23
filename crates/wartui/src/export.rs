//! `wartui export` — turn a capture into something WiGLE will take.
//!
//! Separate from the capture because the store is the system of record: an
//! export can be re-run after a decoder fix, run against a session that ended
//! last week, or run against one that is still going. WAL is what makes that
//! last case safe.
//!
//! Both ends default, so exporting the evening that just ended is `wartui export` and
//! nothing else: the newest capture in the working directory, written out under its own
//! name with `.csv` where the `.db` was. A name chosen that way is never written over —
//! the reader did not pick it, and the file it would replace may be the one already
//! uploaded — so `--out` is how to say another name, or the same one again and mean it.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use wartui_core::export::{DEFAULT_RECAPTURE_SECS, ExportFilter, wigle_csv};
use wartui_core::store::open_readonly;

use crate::capture;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The capture to export. The newest `wartui-<date>.db` in the working
    /// directory by default, which is the one a finished `run` left there.
    #[arg(long, value_name = "PATH")]
    db: Option<PathBuf>,

    /// Where to write the WiGLE CSV. The capture's own name with `.csv` by
    /// default, beside the capture itself; `-` writes to standard output. A
    /// name chosen by default is not written over, so say it here to mean it.
    #[arg(long, short = 'o', value_name = "PATH")]
    out: Option<PathBuf>,

    /// Only this session. By default every session in the file is exported,
    /// which is usually right: WiGLE deduplicates its own side, and more
    /// sightings of a network is better data rather than worse.
    #[arg(long, value_name = "ID")]
    session: Option<i64>,

    /// How long a network's sightings fold into one row, in seconds. One
    /// hour by default, the leaderboard's scan cooldown; `0` writes one row
    /// per network.
    #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_RECAPTURE_SECS)]
    recapture: u64,
}

pub fn run(args: Args) -> Result<()> {
    let db = match args.db {
        Some(path) => path,
        None => {
            let found = capture::newest(Path::new("."))?;
            // Named on the way past, because which capture was picked is otherwise
            // invisible, and the case where that matters is the quiet one: a run that
            // opened its store and then died leaves a file newer than the evening
            // being exported, and an export of it says nothing but `0 rows`.
            // Standard error, because standard output may be the CSV.
            eprintln!("exporting {}", found.display());
            found
        }
    };
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
    let filter = ExportFilter { session_id: args.session, recapture_secs: args.recapture };
    let version = env!("CARGO_PKG_VERSION");

    let summary = if out.as_os_str() == "-" {
        let stdout = std::io::stdout();
        let mut writer = BufWriter::new(stdout.lock());
        let summary = wigle_csv(&conn, filter, &mut writer, version)?;
        writer.flush().context("flushing the export")?;
        summary
    } else {
        let summary =
            into_csv(&out, derived, |writer| Ok(wigle_csv(&conn, filter, writer, version)?))?;
        eprintln!("{} rows written to {}", summary.rows, out.display());
        summary
    };

    if summary.unpositioned > 0 {
        eprintln!(
            "{} rows were left out because no sighting in their window had a \
             position. Capture with --lat and --lon to include them.",
            summary.unpositioned
        );
    }
    Ok(())
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

    use super::{create_csv, into_csv, is_the_capture};

    #[test]
    fn export_writer_creates_destination_file_when_derived_path_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30.csv");
        create_csv(&path, true).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn export_mgr_refuses_overwrite_when_auto_generated_file_already_exists() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30.csv");
        std::fs::write(&path, "the export that was uploaded").unwrap();
        let error = create_csv(&path, true).unwrap_err().to_string();
        assert!(error.contains("--out"), "{error}");
        assert!(error.contains("wartui-2026-09-18-14-30.csv"), "{error}");
        // And the file it refused to open is the one still there.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "the export that was uploaded");
    }

    #[test]
    fn export_writer_allows_overwrite_when_destination_path_is_explicitly_specified() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tonight.csv");
        std::fs::write(&path, "an export of the session before").unwrap();
        create_csv(&path, false).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn export_writer_deletes_partial_file_when_auto_generated_export_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30.csv");
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
        let path = dir.path().join("wartui-2026-09-18-14-30.csv");
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
}
