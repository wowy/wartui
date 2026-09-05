//! `wartui export` — turn a capture into something WiGLE will take.
//!
//! Separate from the capture because the store is the system of record: an
//! export can be re-run after a decoder fix, run against a session that ended
//! last week, or run against one that is still going. WAL is what makes that
//! last case safe.

use std::io::{BufWriter, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Args as ClapArgs;
use wartui_core::export::{ExportFilter, wigle_csv};
use wartui_core::store::open_readonly;

#[derive(ClapArgs, Debug)]
pub struct Args {
    /// The capture to export.
    #[arg(long, value_name = "PATH", default_value = "wartui.db")]
    db: PathBuf,

    /// Where to write the WiGLE CSV. `-` writes to standard output.
    #[arg(long, value_name = "PATH")]
    wigle: PathBuf,

    /// Only this session. By default every session in the file is exported,
    /// which is usually right: WiGLE deduplicates on its own side, and more
    /// sightings of a network is better data rather than worse.
    #[arg(long, value_name = "ID")]
    session: Option<i64>,
}

pub fn run(args: Args) -> Result<()> {
    let conn = open_readonly(&args.db).with_context(|| format!("opening {}", args.db.display()))?;
    let filter = ExportFilter { session_id: args.session };
    let version = env!("CARGO_PKG_VERSION");

    let summary = if args.wigle.as_os_str() == "-" {
        let stdout = std::io::stdout();
        let mut out = BufWriter::new(stdout.lock());
        let summary = wigle_csv(&conn, filter, &mut out, version)?;
        out.flush().context("flushing the export")?;
        summary
    } else {
        let file = std::fs::File::create(&args.wigle)
            .with_context(|| format!("creating {}", args.wigle.display()))?;
        let mut out = BufWriter::new(file);
        let summary = wigle_csv(&conn, filter, &mut out, version)?;
        out.flush().context("flushing the export")?;
        eprintln!("{} networks written to {}", summary.networks, args.wigle.display());
        summary
    };

    if summary.unpositioned > 0 {
        eprintln!(
            "{} networks were left out because no sighting of them had a position. \
             Capture with --lat and --lon to include them.",
            summary.unpositioned
        );
    }
    Ok(())
}
