//! `wartui upload` — send a capture to WDGWars and WiGLE.
//!
//! Each site is its own module: [`wdgwars`] and [`wigle`] know their endpoints, their answers
//! and their limits. What both share is here.
//!
//! What is uploaded is the capture, not a CSV file. The store is the system of record, and
//! building the CSV in memory from it means no CSV on disk can be stale: an upload after a
//! decoder fix sends what `export` would write now, not what it wrote last week. Uploading a
//! capture that is still being written is safe for the reason exporting one is: WAL gives
//! the read a consistent snapshot.
//!
//! An upload goes to every site with credentials, or to the one `--to` names. A site with half
//! its credentials (a WiGLE name and no token) is refused by name rather than read as unset,
//! since it is a mistake worth seeing, and the other site still goes. Each site gets its own
//! body, because each folds rows over its own recapture window unless `--recapture` sets one
//! for both.
//!
//! Nothing is sent without the operator seeing what will go and saying yes, because an
//! upload cannot be taken back. One question covers every site. Without a terminal to ask on,
//! that is known before any body is built, so it is refused first.
//!
//! The capture records each upload per site, so a repeat sends a site only what came after its
//! last one, and an upload to one site is no cutoff for the other. The cutoff is the last
//! sighting the upload covered, in the order the capture stored them, read in the same snapshot
//! as the CSV, so it is exactly what was considered and a capture still being written loses
//! nothing to it. The record is written once the site has queued the upload, because that is
//! when the upload has happened. The capture is opened for writing before anything is sent, so
//! one this user cannot write is refused with nothing uploaded rather than uploaded and left
//! unrecorded. An upload the site reports failed imported nothing and is no cutoff. `--resend`
//! ignores the record and sends everything, which is what a decoder fix needs: an upload then
//! sends what `export` would write now.
//!
//! Every ready site is sent to, whatever happened to the other. `upload` finishes each before
//! starting the next, so WDGWars' job is followed before WiGLE's send starts, which has no
//! deadline. A script that sees a non-zero exit retries, so the exit is non-zero only when a site
//! was refused before it queued the upload, or WDGWars reports its import failed. A WiGLE upload
//! whose `queued` result cannot be written is no failure: its row is written, and is a cutoff.
//! The retry is safe: a site that queued reads "nothing new".
//!
//! A capture of test data — made with `--sim` or `--lat`/`--lon` — is refused for every site
//! whatever the flags say: invented networks or a fixed position would be published as real,
//! WDGWars and WiGLE ban both, and an upload cannot be taken back. The capture records that it
//! holds test data, so the refusal holds for `upload` and for the fleet view.
//!
//! The steps are separate functions — [`prepare`], [`send`], [`wdgwars::wait`], [`finish`] —
//! and the wait reports through a callback rather than printing, so something other than this
//! command can drive them.

pub(crate) mod wdgwars;
pub(crate) mod wigle;

use std::collections::HashSet;
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::{Args as ClapArgs, ValueEnum};
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Map, Value};
use wartui_core::export::{DEFAULT_RECAPTURE_SECS, ExportFilter, ExportSummary, wigle_csv};
use wartui_core::store::{
    Connection, UploadRecord, UploadSite, is_simulated, last_upload, open_readonly, open_readwrite,
    record_upload, set_upload_result,
};

use crate::config::Config;
use crate::export::{
    Selection, details, note_excluded, note_quality, note_unknown_kind, note_unpositioned,
};
use crate::text::thousands;

/// Every site, in the order an upload goes to them.
pub(crate) const SITES: [UploadSite; 2] = [UploadSite::Wdgwars, UploadSite::Wigle];

/// How long to wait for a response's headers, and then for its body.
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of an unexpected answer to quote back.
const QUOTE_CHARS: usize = 200;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,

    /// Send to this site only. Every site with credentials by default.
    #[arg(long, value_name = "SITE")]
    to: Option<To>,

    /// Upload without asking first.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Send everything the selection covers, ignoring what earlier uploads sent.
    #[arg(long)]
    resend: bool,
}

/// A site `--to` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum To {
    Wdgwars,
    Wigle,
}

impl To {
    fn site(self) -> UploadSite {
        match self {
            Self::Wdgwars => UploadSite::Wdgwars,
            Self::Wigle => UploadSite::Wigle,
        }
    }
}

/// What the bin crate says of a site.
pub(crate) trait Site {
    /// The site's name, as the operator reads it.
    fn label(self) -> &'static str;
    /// What the site calls an upload it queued.
    fn job(self) -> &'static str;
    /// How long a network's sightings fold into one row when `--recapture` is not given.
    fn recapture_secs(self) -> u64;
    /// Refuse a body over the site's limit: `csv_bytes` before compression, `file_bytes` as
    /// sent. One exactly at the limit goes.
    fn check_size(self, csv_bytes: usize, file_bytes: usize) -> Result<()>;
}

impl Site for UploadSite {
    fn label(self) -> &'static str {
        match self {
            Self::Wdgwars => "WDGWars",
            Self::Wigle => "WiGLE",
        }
    }

    fn job(self) -> &'static str {
        match self {
            Self::Wdgwars => "job",
            Self::Wigle => "transaction",
        }
    }

    fn recapture_secs(self) -> u64 {
        match self {
            // The leaderboard's scan cooldown.
            Self::Wdgwars => DEFAULT_RECAPTURE_SECS,
            Self::Wigle => wigle::RECAPTURE_SECS,
        }
    }

    fn check_size(self, csv_bytes: usize, file_bytes: usize) -> Result<()> {
        match self {
            Self::Wdgwars if csv_bytes > wdgwars::MAX_CSV => bail!(
                "the upload's CSV is {} before compression, over WDGWars' limit of {}",
                size(csv_bytes),
                size(wdgwars::MAX_CSV)
            ),
            Self::Wigle if file_bytes > wigle::MAX_FILE => bail!(
                "the upload is {} compressed, over WiGLE's limit of {} (180 MiB)",
                size(file_bytes),
                size(wigle::MAX_FILE)
            ),
            Self::Wdgwars | Self::Wigle => Ok(()),
        }
    }
}

pub fn run(args: Args) -> Result<()> {
    let (config, config_path) = args.selection.config()?;
    let db = args.selection.capture("reading")?;
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to upload");
    }
    let options = Options {
        to: args.to.map(To::site),
        recapture: args.selection.recapture(),
        resend: args.resend,
        excluded_ssids: args.selection.filter(&config).excluded_ssids,
    };
    let outcomes = prepare(&config, config_path.as_deref(), &db, &options)?;

    let mut failed = Vec::new();
    let mut ready = Vec::new();
    for (at, (site, outcome)) in outcomes.into_iter().enumerate() {
        if at > 0 {
            eprintln!();
        }
        let label = site.label();
        match outcome {
            Prepare::Ready(prepared) => {
                eprint!("{label}: {}", prepared.describe());
                notes(&prepared.summary);
                ready.push(prepared);
            }
            Prepare::NothingNew { previous, summary } => {
                eprintln!(
                    "{label}: everything in {} was uploaded ({} {}, {}); --resend sends it again",
                    db.display(),
                    site.job(),
                    previous.job,
                    utc_minute(previous.uploaded_at_ms)
                );
                notes(&summary);
            }
            Prepare::NoRows(summary) => {
                eprintln!("{label}: {} has no rows to upload", db.display());
                notes(&summary);
            }
            Prepare::Refused(error) => {
                eprintln!("not uploaded to {label}: {error:#}");
                failed.push(site);
            }
        }
    }
    if ready.is_empty() {
        return failures(&failed);
    }
    let labels: Vec<&str> = ready.iter().map(|prepared| prepared.site().label()).collect();
    if !args.yes && !confirm(&labels.join(" and "))? {
        eprintln!("not uploaded");
        return failures(&failed);
    }

    // In `SITES` order, each finished before the next starts, so WDGWars' job is followed
    // without waiting on WiGLE's send, which has no deadline.
    for prepared in &ready {
        let site = prepared.site();
        match send(prepared, &db, wdgwars::BASE, wigle::BASE) {
            Ok(Sent::Wdgwars { client, job }) => {
                eprintln!("uploaded; WDGWars queued it as job {}", job.id);
                if !follow_wdgwars(&client, &job) {
                    failed.push(site);
                }
            }
            Ok(Sent::Wigle { queued, unrecorded }) => {
                eprintln!(
                    "uploaded; WiGLE queued it as transaction {}. Its import carries on; check \
                     https://wigle.net/uploads for the result.",
                    queued.trans_id
                );
                if let Some(warning) = queued.warning {
                    eprintln!("WiGLE warns: {warning}");
                }
                // WiGLE has the file and the row is a cutoff, so this is no failed upload.
                if let Some(error) = unrecorded {
                    eprintln!("{error:#}");
                }
            }
            Err(error) => {
                eprintln!("{} upload failed: {error:#}", site.label());
                failed.push(site);
            }
        }
    }
    failures(&failed)
}

/// Follow WDGWars' `job` on standard error and record how it ended. False when the import
/// failed or its end could not be recorded.
fn follow_wdgwars(client: &wdgwars::Client, job: &Job) -> bool {
    let id = job.id;
    let started = Instant::now();
    let waited = wdgwars::wait(
        id,
        wdgwars::give_up(job.rows),
        || client.poll(id),
        std::thread::sleep,
        || started.elapsed(),
        wdgwars::on_stderr(id),
    );
    let recorded = finish(job, &waited, started.elapsed());
    let imported = match &waited {
        Ok(wdgwars::Waited::Done(result)) => {
            print!("{}", wdgwars::outcome(result));
            true
        }
        Ok(wdgwars::Waited::Unfollowed(reason)) => {
            eprintln!(
                "WDGWars accepted the upload as job {id}. Its import carries on; check {} or \
                 your profile on wdgwars.pl for the result. wartui stopped following it: \
                 {reason}.",
                client.job_url(id)
            );
            true
        }
        Err(error) => {
            eprintln!("{error:#}");
            false
        }
    };
    if let Err(error) = &recorded {
        eprintln!("{error:#}");
    }
    imported && recorded.is_ok()
}

/// An error naming every site in `failed`, or `Ok` when it is empty.
fn failures(failed: &[UploadSite]) -> Result<()> {
    if failed.is_empty() {
        return Ok(());
    }
    let labels: Vec<&str> = failed.iter().map(|site| site.label()).collect();
    bail!("the upload to {} failed", labels.join(" and "))
}

/// What `export` says of the rows a summary left out.
fn notes(summary: &ExportSummary) {
    note_unpositioned(summary);
    note_unknown_kind(summary);
    note_excluded(summary);
    note_quality(summary);
}

/// What an upload covers, beside the capture.
#[derive(Debug, Default)]
pub(crate) struct Options {
    /// The one site to send to; every site with credentials when `None`.
    pub(crate) to: Option<UploadSite>,
    /// The recapture window for every site, in seconds; each site's own when `None`.
    pub(crate) recapture: Option<u64>,
    /// Send everything, ignoring what earlier uploads sent.
    pub(crate) resend: bool,
    /// Wi-Fi SSIDs left out.
    pub(crate) excluded_ssids: HashSet<Vec<u8>>,
}

/// What a site signs in with.
pub(crate) enum Credentials {
    Wdgwars { key: String },
    Wigle { name: String, token: String },
}

impl Credentials {
    fn site(&self) -> UploadSite {
        match self {
            Self::Wdgwars { .. } => UploadSite::Wdgwars,
            Self::Wigle { .. } => UploadSite::Wigle,
        }
    }
}

/// What is ready to send to one site.
pub(crate) struct Prepared {
    credentials: Credentials,
    pub(crate) body: Vec<u8>,
    /// The CSV's length before compression, the size WDGWars' limit counts.
    pub(crate) csv_bytes: usize,
    pub(crate) summary: ExportSummary,
    /// The upload to this site this one follows on from. `None` with `--resend`, which follows
    /// on from nothing.
    pub(crate) previous: Option<UploadRecord>,
}

impl Prepared {
    /// The site it goes to.
    pub(crate) fn site(&self) -> UploadSite {
        self.credentials.site()
    }

    /// What the operator is asked to send: the row count, the summary's details, the CSV's
    /// size, its compressed size and the upload this one follows on from, a line each.
    pub(crate) fn describe(&self) -> String {
        let mut text = self.rows_line();
        text.push_str(&details(&self.summary));
        text.push_str(&self.sizes());
        text
    }

    /// [`Self::describe`] without the summary's details, for a confirm that shows two sites.
    pub(crate) fn brief(&self) -> String {
        let mut text = self.rows_line();
        text.push_str(&self.sizes());
        text
    }

    fn rows_line(&self) -> String {
        format!("{} rows to upload\n", thousands(self.summary.rows))
    }

    fn sizes(&self) -> String {
        let mut text = format!("  {:<11}{}\n", "csv", size(self.csv_bytes));
        text.push_str(&format!("  {:<11}{}\n", "compressed", size(self.body.len())));
        if let Some(previous) = &self.previous {
            text.push_str(&format!(
                "  {:<11}{} {}, {}; sending what came after\n",
                "previously",
                self.site().job(),
                previous.job,
                utc_minute(previous.uploaded_at_ms)
            ));
        }
        text
    }
}

/// What [`prepare`] found for one site. It prints nothing, so the fleet view can drive it; the
/// caller says why nothing is sent.
pub(crate) enum Prepare {
    Ready(Prepared),
    /// Every row was covered by `previous`.
    NothingNew {
        previous: UploadRecord,
        summary: ExportSummary,
    },
    /// No positioned row, and no upload before this one.
    NoRows(ExportSummary),
    /// The site cannot be sent to, and why: half its credentials, or a body over its limit.
    Refused(anyhow::Error),
}

/// The sites `config` holds credentials for, whole or half, in [`SITES`] order.
pub(crate) fn involved(config: &Config) -> Vec<UploadSite> {
    let keys = &config.api_keys;
    SITES
        .into_iter()
        .filter(|site| match site {
            UploadSite::Wdgwars => !keys.wdgwars.trim().is_empty(),
            UploadSite::Wigle => {
                !keys.wigle_name.trim().is_empty() || !keys.wigle_token.trim().is_empty()
            }
        })
        .collect()
}

/// What `config` holds for `site`, or why it cannot sign in. `file` is where the config lives.
fn credentials(config: &Config, site: UploadSite, file: &str) -> Result<Credentials> {
    let keys = &config.api_keys;
    match site {
        UploadSite::Wdgwars => {
            let key = keys.wdgwars.trim();
            if key.is_empty() {
                bail!(
                    "no WDGWars API key; paste one in the settings modal (`c`) or set `wdgwars` \
                     under `[api-keys]` in {file}"
                );
            }
            Ok(Credentials::Wdgwars { key: key.to_owned() })
        }
        UploadSite::Wigle => {
            let (name, token) = (keys.wigle_name.trim(), keys.wigle_token.trim());
            let missing = match (name.is_empty(), token.is_empty()) {
                (false, false) => {
                    return Ok(Credentials::Wigle {
                        name: name.to_owned(),
                        token: token.to_owned(),
                    });
                }
                (true, true) => bail!(
                    "no WiGLE API name and token; paste them in the settings modal (`c`) or set \
                     `wigle-name` and `wigle-token` under `[api-keys]` in {file}"
                ),
                (false, true) => ("token", "wigle-token"),
                (true, false) => ("name", "wigle-name"),
            };
            bail!(
                "WiGLE needs both an API name and an API token; paste the {} in the settings \
                 modal (`c`) or set `{}` under `[api-keys]` in {file}",
                missing.0,
                missing.1
            )
        }
    }
}

/// Check the credentials, then build each site's body from what came after its last upload, or
/// from everything with `resend`. One outcome per site, in [`SITES`] order. Contacts nothing, so
/// every refusal here costs no request. An error refuses the whole upload: no site has
/// credentials, or the capture holds test data or cannot be read.
pub(crate) fn prepare(
    config: &Config,
    config_path: Option<&Path>,
    db: &Path,
    options: &Options,
) -> Result<Vec<(UploadSite, Prepare)>> {
    let file = config_path.map_or_else(|| "wartui.toml".to_owned(), |p| p.display().to_string());
    let sites = options.to.map_or_else(|| involved(config), |site| vec![site]);
    if sites.is_empty() {
        bail!(
            "no upload credentials; paste a WDGWars API key or a WiGLE API name and token in the \
             settings modal (`c`), or set `wdgwars`, or `wigle-name` and `wigle-token`, under \
             `[api-keys]` in {file}"
        );
    }
    let signed: Vec<(UploadSite, Result<Credentials>)> =
        sites.into_iter().map(|site| (site, credentials(config, site, &file))).collect();
    if signed.iter().all(|(_, credentials)| credentials.is_err()) {
        let refused = signed.into_iter().filter_map(|(site, credentials)| {
            credentials.err().map(|error| (site, Prepare::Refused(error)))
        });
        return Ok(refused.collect());
    }
    let conn = open_readonly(db).with_context(|| format!("opening {}", db.display()))?;
    if is_simulated(&conn).with_context(|| format!("reading {}", db.display()))? {
        return Err(Simulated { db: db.to_owned() }.into());
    }
    signed
        .into_iter()
        .map(|(site, credentials)| {
            let outcome = match credentials {
                Ok(credentials) => prepare_site(&conn, credentials, options)?,
                Err(error) => Prepare::Refused(error),
            };
            Ok((site, outcome))
        })
        .collect()
}

/// [`prepare`] for the site `credentials` signs in to.
fn prepare_site(conn: &Connection, credentials: Credentials, options: &Options) -> Result<Prepare> {
    let site = credentials.site();
    let filter = ExportFilter {
        recapture_secs: options.recapture.unwrap_or_else(|| site.recapture_secs()),
        after_uploads: (!options.resend).then_some(site),
        excluded_ssids: options.excluded_ssids.clone(),
    };
    let Body { gz, csv_bytes, summary, previous } = body(conn, &filter)?;
    if summary.rows == 0 {
        return Ok(match previous {
            Some(previous) => Prepare::NothingNew { previous, summary },
            None => Prepare::NoRows(summary),
        });
    }
    if let Err(error) = site.check_size(csv_bytes, gz.len()) {
        return Ok(Prepare::Refused(error));
    }
    Ok(Prepare::Ready(Prepared { credentials, body: gz, csv_bytes, summary, previous }))
}

/// What [`body`] built.
struct Body {
    /// The capture's WiGLE CSV, gzipped.
    gz: Vec<u8>,
    /// The CSV's length before compression, which WDGWars' limit counts.
    csv_bytes: usize,
    /// What went into it.
    summary: ExportSummary,
    /// The upload it follows on from when it is [`ExportFilter::after_uploads`].
    previous: Option<UploadRecord>,
}

/// A writer that counts what it passes on.
struct Counted<W> {
    inner: W,
    bytes: usize,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The capture's WiGLE CSV, gzipped in memory, with its uncompressed length.
fn body(conn: &Connection, filter: &ExportFilter) -> Result<Body> {
    let mut csv = Counted { inner: GzEncoder::new(Vec::new(), Compression::default()), bytes: 0 };
    let summary = wigle_csv(conn, filter, &mut csv, env!("CARGO_PKG_VERSION"))?;
    let gz = csv.inner.finish().context("compressing the upload")?;
    let previous = match filter.after_uploads {
        Some(site) => last_upload(conn, site)?,
        None => None,
    };
    Ok(Body { gz, csv_bytes: csv.bytes, summary, previous })
}

/// The capture holds test data (`--sim` or `--lat`/`--lon`), so it is not uploaded.
#[derive(Debug)]
pub(crate) struct Simulated {
    db: PathBuf,
}

impl std::fmt::Display for Simulated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} holds test data (made with --sim or --lat/--lon), so it is not uploaded",
            self.db.display()
        )
    }
}

impl std::error::Error for Simulated {}

/// An upload a site queued.
pub(crate) enum Sent {
    /// WDGWars queued a job, which [`wdgwars::wait`] follows and [`finish`] records the end of.
    Wdgwars { client: wdgwars::Client, job: Job },
    /// WiGLE queued a transaction, recorded as `queued`. `unrecorded` is why that result could
    /// not be written; the row is written all the same, so it is still a cutoff.
    Wigle { queued: wigle::Queued, unrecorded: Option<anyhow::Error> },
}

/// A WDGWars job, and the capture's record of it.
pub(crate) struct Job {
    pub(crate) id: u64,
    /// The rows the upload carried, which the wait grows with ([`wdgwars::give_up`]).
    pub(crate) rows: u64,
    record: i64,
    /// The capture, open for writing since before the upload went.
    conn: Connection,
}

/// Open `db` for writing, hand `prepared` to its site at `wdgwars_base` or `wigle_base`, then
/// record that the site queued it.
pub(crate) fn send(
    prepared: &Prepared,
    db: &Path,
    wdgwars_base: &str,
    wigle_base: &str,
) -> Result<Sent> {
    let conn = open_readwrite(db).with_context(|| format!("opening {}", db.display()))?;
    let filename = filename(db);
    let through = prepared.summary.last_id.expect("a capture with rows has a last sighting");
    let now = Utc::now().timestamp_millis();
    let rows = prepared.summary.rows;
    match &prepared.credentials {
        Credentials::Wdgwars { key } => {
            let client = wdgwars::Client::new(wdgwars_base, key.clone());
            let id = client.submit(&prepared.body, &filename)?;
            let record =
                record_upload(&conn, UploadSite::Wdgwars, through, now, &id.to_string(), rows)
                    .with_context(|| format!("recording job {id} in {}", db.display()))?;
            Ok(Sent::Wdgwars { client, job: Job { id, rows, record, conn } })
        }
        Credentials::Wigle { name, token } => {
            let client = wigle::Client::new(wigle_base, name, token);
            let queued = client.submit(&prepared.body, &filename)?;
            let id = &queued.trans_id;
            let record = record_upload(&conn, UploadSite::Wigle, through, now, id, rows)
                .with_context(|| format!("recording transaction {id} in {}", db.display()))?;
            let unrecorded = set_upload_result(&conn, record, "queued")
                .with_context(|| format!("recording transaction {id} as queued"))
                .err();
            Ok(Sent::Wigle { queued, unrecorded })
        }
    }
}

/// Record how the wait for WDGWars' `job` ended, `elapsed` after it began, and log it. The
/// store keeps only the result, so the log is where an unfollowed job's reason survives.
pub(crate) fn finish(job: &Job, waited: &Result<wdgwars::Waited>, elapsed: Duration) -> Result<()> {
    let (id, rows, elapsed_s) = (job.id, job.rows, elapsed.as_secs());
    let result = match waited {
        Ok(wdgwars::Waited::Done(_)) => {
            tracing::info!(
                site = "wdgwars",
                job = id,
                rows,
                elapsed_s,
                result = "done",
                "WDGWars upload"
            );
            "done"
        }
        Ok(wdgwars::Waited::Unfollowed(reason)) => {
            let result = "unfollowed";
            tracing::info!(
                site = "wdgwars",
                job = id,
                rows,
                elapsed_s,
                result,
                reason,
                "WDGWars upload"
            );
            result
        }
        Err(error) => {
            let reason = error
                .downcast_ref::<wdgwars::JobFailed>()
                .map_or_else(|| format!("{error:#}"), |failed| failed.message.clone());
            let result = "failed";
            tracing::warn!(
                site = "wdgwars",
                job = id,
                rows,
                elapsed_s,
                result,
                reason,
                "WDGWars upload"
            );
            result
        }
    };
    set_upload_result(&job.conn, job.record, result)
        .with_context(|| format!("recording how job {} ended", job.id))?;
    Ok(())
}

/// Unix milliseconds in UTC to the minute, as the summary's span line gives times.
fn utc_minute(ms: i64) -> String {
    let at = DateTime::from_timestamp_millis(ms).unwrap_or_default();
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// The name the file goes up under: the capture's, as a gzipped CSV.
fn filename(db: &Path) -> String {
    let stem = db.file_stem().map_or_else(|| "wartui".into(), |s| s.to_string_lossy());
    format!("{stem}.csv.gz")
}

/// Ask on the terminal whether to upload to `sites`. `run` has already refused when standard
/// input is not one.
fn confirm(sites: &str) -> Result<bool> {
    eprint!("Upload to {sites}? [y/N] ");
    std::io::stderr().flush().context("writing the question")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// `bytes` in decimal units, the ones [`wdgwars::MAX_CSV`] is given in.
pub(crate) fn size(bytes: usize) -> String {
    const MB: usize = 1_000_000;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1_000))
    }
}

/// An agent for a site's API. Statuses are mapped by each client rather than by `ureq`, so a
/// refusal is reported in the site's own words, and redirects reach the client rather than
/// being followed. Sending the body has no deadline: a large file over a slow tether takes as
/// long as it takes.
fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(RECV_TIMEOUT))
        .timeout_recv_body(Some(RECV_TIMEOUT))
        .build()
        .into()
}

/// Where a redirect points.
fn location(response: &ureq::http::Response<ureq::Body>) -> String {
    response
        .headers()
        .get(ureq::http::header::LOCATION)
        .map_or_else(|| "nowhere".to_owned(), |l| String::from_utf8_lossy(l.as_bytes()).into())
}

/// The site's reason in a refusal's body: its `error` or `message`, or the quoted text.
fn site_reason(text: &str) -> Option<String> {
    let reason = match serde_json::from_str(text) {
        Ok(Value::Object(object)) => reason(&object),
        _ => None,
    };
    reason.or_else(|| Some(quote(text)).filter(|text| !text.is_empty()))
}

/// At most [`QUOTE_CHARS`] of `text`, trimmed.
fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// `text` as a JSON object, from `site`.
fn object(site: UploadSite, text: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str(text) {
        Ok(Value::Object(object)) => Ok(object),
        _ => bail!(
            "{} answered with something other than a JSON object: {}",
            site.label(),
            quote(text)
        ),
    }
}

/// The site's explanation, under `error` or `message`.
fn reason(object: &Map<String, Value>) -> Option<String> {
    ["error", "message"].iter().find_map(|key| object.get(*key)?.as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use clap::Parser;
    use flate2::read::GzDecoder;
    use wartui_core::export::{ExportFilter, ExportSummary, wigle_csv};
    use wartui_core::store::{
        UploadSite, last_upload, open_readonly, open_readwrite, record_upload, set_upload_result,
    };

    use super::{
        Credentials, Job, Options, Prepare, Prepared, Sent, Site, To, body, finish, prepare, send,
        size, wdgwars, wigle,
    };
    use crate::config::Config;
    use crate::testing::{EPOCH_MS, capture, capture_simulated, serve_once, sighting};

    fn keyed() -> Config {
        let mut config = Config::default();
        config.api_keys.wdgwars = "secret".to_owned();
        config
    }

    /// [`keyed`], with a WiGLE name and token too.
    fn both() -> Config {
        let mut config = keyed();
        config.api_keys.wigle_name = "AIDexamplename".to_owned();
        config.api_keys.wigle_token = "exampletoken789".to_owned();
        config
    }

    /// `prepare` with `options`, as a list of outcomes.
    fn outcomes_of(config: &Config, db: &Path, options: &Options) -> Vec<(UploadSite, Prepare)> {
        prepare(config, None, db, options).unwrap()
    }

    /// `prepare` with the default options, for the one site `config` names.
    fn prepare_one(config: &Config, db: &Path, resend: bool) -> Prepare {
        let options = Options { resend, ..Options::default() };
        let mut outcomes = outcomes_of(config, db, &options);
        assert_eq!(outcomes.len(), 1);
        outcomes.remove(0).1
    }

    /// `site`'s outcome in `outcomes`.
    fn outcome(outcomes: Vec<(UploadSite, Prepare)>, site: UploadSite) -> Prepare {
        outcomes.into_iter().find(|(s, _)| *s == site).expect("an outcome for the site").1
    }

    #[test]
    fn upload_body_gunzips_to_export_csv_when_capture_has_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            &dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        let conn = open_readonly(&db).unwrap();
        let built = body(&conn, &ExportFilter::default()).unwrap();
        assert_eq!(built.summary.rows, 2);

        let mut csv = Vec::new();
        GzDecoder::new(built.gz.as_slice()).read_to_end(&mut csv).unwrap();
        let mut expected = Vec::new();
        wigle_csv(&conn, &ExportFilter::default(), &mut expected, env!("CARGO_PKG_VERSION"))
            .unwrap();
        assert_eq!(csv, expected);
    }

    #[test]
    fn upload_body_counts_csv_bytes_before_compression_when_built() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            &dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        let built = body(&open_readonly(&db).unwrap(), &ExportFilter::default()).unwrap();
        let mut csv = Vec::new();
        GzDecoder::new(built.gz.as_slice()).read_to_end(&mut csv).unwrap();
        assert_eq!(built.csv_bytes, csv.len());
    }

    /// A prepared WDGWars upload of two rows, following on from `previous`.
    fn ready(previous: Option<wartui_core::store::UploadRecord>) -> Prepared {
        Prepared {
            credentials: Credentials::Wdgwars { key: "secret".to_owned() },
            body: vec![0; 412_345],
            csv_bytes: 3_343_178,
            summary: ExportSummary { rows: 2, ..Default::default() },
            previous,
        }
    }

    #[test]
    fn upload_describe_shows_csv_and_compressed_sizes_when_prepared() {
        // The limit counts the CSV, so the operator sees that size beside the one sent.
        let text = ready(None).describe();
        assert!(text.contains("  csv        3.3 MB\n"), "{text}");
        assert!(text.contains("  compressed 413 KB\n"), "{text}");
    }

    #[test]
    fn upload_brief_drops_details_when_prepared() {
        let previous = wartui_core::store::UploadRecord {
            job: "20261122-00017".to_owned(),
            uploaded_at_ms: EPOCH_MS,
            rows: 1,
        };
        let mut prepared = ready(Some(previous));
        prepared.credentials = Credentials::Wigle {
            name: "AIDexamplename".to_owned(),
            token: "exampletoken789".to_owned(),
        };
        assert_eq!(
            prepared.brief(),
            "2 rows to upload\n  csv        3.3 MB\n  compressed 413 KB\n  previously \
             transaction 20261122-00017, 2026-05-01 13:34 UTC; sending what came after\n"
        );
    }

    #[test]
    fn upload_check_size_refuses_when_csv_over_wdgwars_limit() {
        // WDGWars counts a gzipped file by its decompressed size, so a CSV over the limit is
        // refused however well it compresses.
        let error = UploadSite::Wdgwars.check_size(wdgwars::MAX_CSV + 1, 10).unwrap_err();
        assert!(error.to_string().contains("before compression"), "{error}");
        assert!(UploadSite::Wdgwars.check_size(wdgwars::MAX_CSV, wdgwars::MAX_CSV * 2).is_ok());
    }

    #[test]
    fn upload_check_size_counts_gzipped_file_when_site_is_wigle() {
        // WiGLE's limit is on the file sent, so a CSV that compresses under it goes.
        assert!(UploadSite::Wigle.check_size(wigle::MAX_FILE * 3, wigle::MAX_FILE).is_ok());
        let error = UploadSite::Wigle.check_size(10, wigle::MAX_FILE + 1).unwrap_err();
        assert_eq!(
            error.to_string(),
            "the upload is 188.7 MB compressed, over WiGLE's limit of 188.7 MB (180 MiB)"
        );
    }

    #[test]
    fn upload_refuses_without_contacting_server_when_no_credentials() {
        // `prepare` takes no client, so a refusal here cannot have sent anything; the
        // capture does not even exist.
        let error = prepare(
            &Config::default(),
            Some(Path::new("/home/op/.config/wartui/wartui.toml")),
            Path::new("/nonexistent/wartui.db"),
            &Options::default(),
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.starts_with("no upload credentials; "), "{error}");
        assert!(error.contains("settings modal (`c`)"), "{error}");
        assert!(error.contains("/home/op/.config/wartui/wartui.toml"), "{error}");
    }

    #[test]
    fn upload_refuses_named_site_when_it_has_no_credentials() {
        let options = Options { to: Some(UploadSite::Wdgwars), ..Options::default() };
        let outcomes =
            outcomes_of(&Config::default(), Path::new("/nonexistent/wartui.db"), &options);
        let [(UploadSite::Wdgwars, Prepare::Refused(error))] = outcomes.as_slice() else {
            panic!("not refused")
        };
        assert!(error.to_string().starts_with("no WDGWars API key; "), "{error}");
    }

    #[test]
    fn upload_refuses_every_site_when_capture_simulated() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))];
        let db = capture_simulated(&dir, records, true);
        for resend in [false, true] {
            let options = Options { resend, ..Options::default() };
            let error = prepare(&both(), None, &db, &options).err().expect("refused").to_string();
            assert_eq!(
                error,
                format!(
                    "{} holds test data (made with --sim or --lat/--lon), so it is not uploaded",
                    db.display()
                )
            );
        }
    }

    #[test]
    fn upload_sends_nothing_when_capture_has_no_positioned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, None)]);
        assert!(matches!(prepare_one(&keyed(), &db, false), Prepare::NoRows(_)));
    }

    #[test]
    fn upload_prepare_reports_no_rows_when_every_ssid_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            &dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        // `sighting` stores every network as `example`.
        let excluded_ssids = std::collections::HashSet::from([b"example".to_vec()]);
        let options = Options { excluded_ssids, ..Options::default() };
        let outcomes = outcomes_of(&keyed(), &db, &options);
        let Prepare::NoRows(summary) = outcome(outcomes, UploadSite::Wdgwars) else {
            panic!("rows to upload")
        };
        assert_eq!(summary.excluded_ssid, 2);
    }

    #[test]
    fn upload_prepare_covers_every_configured_site_when_none_named() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let outcomes = outcomes_of(&both(), &db, &Options::default());
        let sites: Vec<UploadSite> = outcomes.iter().map(|(site, _)| *site).collect();
        assert_eq!(sites, [UploadSite::Wdgwars, UploadSite::Wigle]);
        assert!(outcomes.iter().all(|(_, outcome)| matches!(outcome, Prepare::Ready(_))));
    }

    #[test]
    fn upload_prepare_covers_only_named_site_when_to_given() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..Options::default() };
        let outcomes = outcomes_of(&both(), &db, &options);
        let [(UploadSite::Wigle, Prepare::Ready(prepared))] = outcomes.as_slice() else {
            panic!("not WiGLE alone")
        };
        assert_eq!(prepared.site(), UploadSite::Wigle);
    }

    #[test]
    fn upload_prepare_refuses_wigle_when_token_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let mut config = both();
        config.api_keys.wigle_token = "  ".to_owned();
        let outcomes = outcomes_of(&config, &db, &Options::default());
        let [(UploadSite::Wdgwars, Prepare::Ready(_)), (UploadSite::Wigle, Prepare::Refused(e))] =
            outcomes.as_slice()
        else {
            panic!("WDGWars not ready or WiGLE not refused")
        };
        let error = e.to_string();
        assert!(
            error.starts_with("WiGLE needs both an API name and an API token; paste the token"),
            "{error}"
        );
        assert!(error.contains("`wigle-token`"), "{error}");
    }

    #[test]
    fn upload_prepare_folds_each_site_over_its_window_when_no_recapture_given() {
        // One network seen twice, 10 s apart: past WiGLE's 5 s, inside WDGWars' hour.
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            &dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS + 10_000, Some(37.1)),
            ],
        );
        let mut outcomes = outcomes_of(&both(), &db, &Options::default());
        let Prepare::Ready(wigle) = outcomes.pop().unwrap().1 else { panic!("WiGLE not ready") };
        let Prepare::Ready(wdgwars) = outcomes.pop().unwrap().1 else {
            panic!("WDGWars not ready")
        };
        assert_eq!((wdgwars.summary.rows, wigle.summary.rows), (1, 2));
    }

    #[test]
    fn upload_prepare_folds_every_site_over_given_window_when_recapture_given() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            &dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS + 10_000, Some(37.1)),
            ],
        );
        for (recapture, rows) in [(3_600, 1), (1, 2)] {
            let options = Options { recapture: Some(recapture), ..Options::default() };
            for (site, outcome) in outcomes_of(&both(), &db, &options) {
                let Prepare::Ready(prepared) = outcome else { panic!("{site:?} not ready") };
                assert_eq!(prepared.summary.rows, rows, "{site:?} at {recapture} s");
            }
        }
    }

    /// Three networks heard a minute apart, positioned, with a WDGWars upload recorded through
    /// the second.
    fn uploaded_through_second(dir: &tempfile::TempDir) -> PathBuf {
        let db = capture(
            dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 60_000, Some(37.1)),
                sighting([0x10, 0, 0, 0, 0, 3], EPOCH_MS + 120_000, Some(37.2)),
            ],
        );
        let conn = open_readwrite(&db).unwrap();
        let id = record_upload(&conn, UploadSite::Wdgwars, 2, EPOCH_MS + 90_000, "7", 2).unwrap();
        set_upload_result(&conn, id, "done").unwrap();
        db
    }

    /// The MACs in a gzipped CSV's rows.
    fn macs(gz: &[u8]) -> Vec<String> {
        let mut csv = String::new();
        GzDecoder::new(gz).read_to_string(&mut csv).unwrap();
        csv.lines().skip(2).map(|line| line.split(',').next().unwrap().to_owned()).collect()
    }

    #[test]
    fn upload_prepare_sends_wigle_every_row_when_only_wdgwars_has_uploaded() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let outcomes = outcomes_of(&both(), &db, &Options::default());
        let Prepare::Ready(wigle) = outcome(outcomes, UploadSite::Wigle) else {
            panic!("WiGLE not ready")
        };
        assert_eq!(wigle.summary.rows, 3);
        assert_eq!(wigle.previous, None);
    }

    #[test]
    fn upload_records_job_and_cutoff_when_submit_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare_one(&keyed(), &db, false).ready();
        assert_eq!(prepared.previous, None);

        let (base, server) = serve_once("202 Accepted", r#"{"ok":true,"job_id":42}"#);
        let sent = send(&prepared, &db, &base, "http://127.0.0.1:9").unwrap();
        server.join().unwrap();
        let Sent::Wdgwars { job, .. } = sent else { panic!("not a WDGWars job") };
        assert_eq!(job.id, 42);

        // Everything went, so a repeat has nothing to send, and the record names the job.
        assert!(matches!(prepare_one(&keyed(), &db, false), Prepare::NothingNew { .. }));
        let previous = last_upload(&open_readonly(&db).unwrap(), UploadSite::Wdgwars)
            .unwrap()
            .expect("a record");
        assert_eq!((previous.job.as_str(), previous.rows), ("42", 1));
    }

    #[test]
    fn upload_records_wigle_transaction_as_queued_when_wigle_accepts() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..Options::default() };
        let prepared = outcome(outcomes_of(&both(), &db, &options), UploadSite::Wigle).ready();

        let (base, server) = serve_once(
            "200 OK",
            r#"{"success":true,"results":{"transids":[{"transId":"20261122-00017"}]}}"#,
        );
        let sent = send(&prepared, &db, "http://127.0.0.1:9", &base).unwrap();
        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.contains("filename=\"wartui.csv.gz\""), "{request}");
        let Sent::Wigle { queued, unrecorded: None } = sent else {
            panic!("not a recorded WiGLE transaction")
        };
        assert_eq!(queued.trans_id, "20261122-00017");

        let conn = open_readonly(&db).unwrap();
        let (site, job, result): (String, String, Option<String>) = conn
            .query_row("SELECT site, job, result FROM upload", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!((site.as_str(), job.as_str()), ("wigle", "20261122-00017"));
        assert_eq!(result.as_deref(), Some("queued"));
        // WiGLE has it all now, and WDGWars still has nothing.
        let outcomes = outcomes_of(&both(), &db, &Options::default());
        let Prepare::NothingNew { previous, .. } = outcome(outcomes, UploadSite::Wigle) else {
            panic!("WiGLE has something new")
        };
        assert_eq!(previous.job, "20261122-00017");
        let outcomes = outcomes_of(&both(), &db, &Options::default());
        assert!(matches!(outcome(outcomes, UploadSite::Wdgwars), Prepare::Ready(_)));
    }

    /// Make every later write of an upload's result fail on `db`, as a full disk would.
    fn refuse_result_writes(db: &Path) {
        open_readwrite(db)
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER refuse_result BEFORE UPDATE ON upload \
                 BEGIN SELECT RAISE(ABORT, 'disk full'); END;",
            )
            .unwrap();
    }

    #[test]
    fn upload_returns_wigle_success_with_error_when_queued_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..Options::default() };
        let prepared = outcome(outcomes_of(&both(), &db, &options), UploadSite::Wigle).ready();
        refuse_result_writes(&db);

        let (base, server) = serve_once(
            "200 OK",
            r#"{"success":true,"results":{"transids":[{"transId":"20261122-00017"}]}}"#,
        );
        let sent = send(&prepared, &db, "http://127.0.0.1:9", &base).unwrap();
        server.join().unwrap();
        let Sent::Wigle { queued, unrecorded: Some(error) } = sent else {
            panic!("not a WiGLE transaction with an unrecorded result")
        };
        assert_eq!(queued.trans_id, "20261122-00017");
        let error = format!("{error:#}");
        assert!(error.starts_with("recording transaction 20261122-00017 as queued"), "{error}");

        // WiGLE has the file, and the row is still its cutoff.
        let conn = open_readonly(&db).unwrap();
        let (site, job, result): (String, String, Option<String>) = conn
            .query_row("SELECT site, job, result FROM upload", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap();
        assert_eq!((site.as_str(), job.as_str(), result), ("wigle", "20261122-00017", None));
    }

    #[test]
    fn upload_sends_nothing_when_capture_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare_one(&keyed(), &db, false).ready();
        let mut permissions = std::fs::metadata(&db).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&db, permissions).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&db).is_ok() {
            // Running as root, which file permissions do not stop.
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let error = send(&prepared, &db, &base, &base).err().expect("refused");
        assert!(format!("{error:#}").contains("cannot be written"), "{error:#}");
        listener.set_nonblocking(true).unwrap();
        let accepted = listener.accept();
        assert!(accepted.is_err(), "the upload was sent anyway");
    }

    #[test]
    fn upload_sends_only_new_sightings_when_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let prepared = prepare_one(&keyed(), &db, false).ready();
        assert_eq!(macs(&prepared.body), ["10:00:00:00:00:03"]);
        assert_eq!(prepared.previous.map(|p| p.job).as_deref(), Some("7"));
    }

    #[test]
    fn upload_sends_everything_when_resend() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let prepared = prepare_one(&keyed(), &db, true).ready();
        assert_eq!(prepared.summary.rows, 3);
        assert_eq!(prepared.previous, None);
    }

    #[test]
    fn upload_finish_records_failed_when_job_failed() {
        // A failed job is no cutoff, so the next upload sends what it carried again.
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let conn = open_readwrite(&db).unwrap();
        let record =
            record_upload(&conn, UploadSite::Wdgwars, 3, EPOCH_MS + 150_000, "8", 1).unwrap();
        let job = Job { id: 8, rows: 1, record, conn };
        finish(&job, &Err(anyhow::anyhow!("failed")), Duration::from_secs(3)).unwrap();

        let previous = last_upload(&open_readonly(&db).unwrap(), UploadSite::Wdgwars)
            .unwrap()
            .expect("a record");
        assert_eq!(previous.job, "7");
    }

    #[test]
    fn upload_size_formats_decimal_units_when_bytes_large() {
        assert_eq!(size(40_000_000), "40.0 MB");
        assert_eq!(size(1_500_000), "1.5 MB");
        assert_eq!(size(12_345), "13 KB");
        assert_eq!(size(1_000), "1 KB");
    }

    #[test]
    fn upload_args_take_to_when_site_named() {
        for (name, to) in [("wdgwars", To::Wdgwars), ("wigle", To::Wigle)] {
            let cli = crate::Cli::try_parse_from(["wartui", "upload", "--to", name]).unwrap();
            let Some(crate::Command::Upload(args)) = cli.command else { panic!("not upload") };
            assert_eq!(args.to, Some(to));
            assert_eq!(args.selection.recapture(), None);
        }
        let cli = crate::Cli::try_parse_from(["wartui", "upload"]).unwrap();
        let Some(crate::Command::Upload(args)) = cli.command else { panic!("not upload") };
        assert_eq!(args.to, None);
    }

    #[test]
    fn upload_args_refuse_to_when_site_unknown() {
        assert!(crate::Cli::try_parse_from(["wartui", "upload", "--to", "openwifi"]).is_err());
    }
}
