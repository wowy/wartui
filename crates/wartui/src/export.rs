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
        let file = create_csv(&out, derived)?;
        let mut writer = BufWriter::new(file);
        let summary = wigle_csv(&conn, filter, &mut writer, version)?;
        writer.flush().context("flushing the export")?;
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

    use super::create_csv;

    #[test]
    fn a_name_we_chose_is_created_when_nothing_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wartui-2026-09-18-14-30.csv");
        create_csv(&path, true).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }

    #[test]
    fn a_name_we_chose_is_never_written_over() {
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
    fn a_name_the_reader_typed_is_written_over() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tonight.csv");
        std::fs::write(&path, "an export of the session before").unwrap();
        create_csv(&path, false).unwrap().write_all(b"rows").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "rows");
    }
}
