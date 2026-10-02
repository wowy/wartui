//! `wartui upload` — send a capture to the WDGWars leaderboard.
//!
//! It uses the v2 CSV queue: one POST hands over the file and gets a job back, and the
//! import is then polled for. The POST returns once the bytes have landed, so a slow
//! uplink in a car park cannot run into a server-side request timeout while the
//! leaderboard imports, and the queue accepts the CSV gzipped, which is several times
//! smaller over a phone's tether.
//!
//! The site's JSON upload is not used. It would re-express what the WiGLE export already
//! decides — recapture windows, which sighting submits, how identifiers are written — in a
//! second schema that could drift from the first. Its HMAC signature adds nothing over TLS
//! and the key already in the header.
//!
//! What is uploaded is the capture, not a CSV file. The store is the system of record, and
//! building the CSV in memory from it means no CSV on disk can be stale: an upload after a
//! decoder fix sends what `export` would write now, not what it wrote last week. Uploading a
//! capture that is still being written is safe for the reason exporting one is: WAL gives
//! the read a consistent snapshot.
//!
//! Once submit returns a job id, the upload has happened. A script that sees a non-zero exit
//! retries, and a retry sends a duplicate, so nothing after that point exits non-zero except
//! a job the site reports as failed. A poll that errors is retried until the give-up time; a
//! poll the site will not answer (a redirect to its login page, or a 401 or 403), and running
//! out of time, end the wait as "cannot follow" and exit zero: the profile on the site shows
//! the result. Redirects are not followed, because an API that redirects is not answering as
//! the API, and following one reads an HTML page as JSON.
//!
//! The wait gives up after 30 seconds. A person sits at the terminal for it, and nothing
//! needs the result to finish: the job's URL and the profile on the site show it later. Each
//! poll has its own short deadline and no sleep crosses the give-up time, so one hung poll
//! cannot stretch the wait. Sending the body has no deadline: a large file over a slow tether
//! takes as long as it takes.
//!
//! Nothing is sent without the operator seeing what will go and saying yes, because an
//! upload cannot be taken back. Without a terminal to ask on, that is known before the body
//! is built, so it is refused first.
//!
//! The capture records each upload, so a repeat sends only what came after the last one. The
//! cutoff is the last sighting the upload covered, in the order the capture stored them, read
//! in the same snapshot as the CSV, so it is exactly what was considered and a capture still
//! being written loses nothing to it. The record is written once the site has queued the job,
//! because that is when the upload has happened. The capture is opened for writing before
//! anything is sent, so one this user cannot write is refused with nothing uploaded rather
//! than uploaded and left unrecorded. A job the site reports failed imported nothing and is
//! no cutoff. `--resend` ignores the record and sends everything,
//! which is what a decoder fix needs: an upload then sends what `export` would write now.
//!
//! The steps are separate functions — [`prepare`], [`send`], [`wait`], [`finish`] — and the
//! wait reports through a callback rather than printing, so something other than this command
//! can drive them.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use clap::Args as ClapArgs;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Map, Value};
use ureq::unversioned::multipart::{Form, Part};
use wartui_core::export::{ExportFilter, ExportSummary, wigle_csv};
use wartui_core::store::{
    Connection, UploadRecord, last_upload, open_readonly, open_readwrite, record_upload,
    set_upload_result,
};

use crate::config::{self, Config};
use crate::export::{Selection, details, note_unpositioned, thousands};

/// The leaderboard.
pub(crate) const BASE: &str = "https://wdgwars.pl";

/// The largest file the queue accepts. The site says 40 MB; the decimal reading is the
/// smaller, so it is the safe one.
const MAX_BODY: usize = 40_000_000;

/// How long to wait before the first poll, and the ceiling the wait grows to.
const FIRST_POLL: Duration = Duration::from_secs(2);
const MAX_POLL: Duration = Duration::from_secs(10);

/// How long to keep asking before leaving the job to the site.
pub(crate) const GIVE_UP: Duration = Duration::from_secs(30);

/// How long to wait for a response's headers, and then for its body.
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// How long one poll may take, start to finish.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// How much of an unexpected answer to quote back.
const QUOTE_CHARS: usize = 200;

#[derive(ClapArgs, Debug)]
pub struct Args {
    #[command(flatten)]
    selection: Selection,

    /// Read the API key from this file instead of the default `wartui.toml`. Never
    /// written; see `crates/wartui/README.md` § "Config file".
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Upload without asking first.
    #[arg(long, short = 'y')]
    yes: bool,

    /// Send everything the selection covers, ignoring what earlier uploads sent.
    #[arg(long)]
    resend: bool,
}

pub fn run(args: Args) -> Result<()> {
    let config = config::load(args.config.as_deref())?;
    let config_path = config::path(args.config.as_deref());
    let db = args.selection.capture("reading")?;
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to upload");
    }
    let filter = args.selection.filter();
    let Some(prepared) = prepare(&config, config_path.as_deref(), &db, filter, args.resend)? else {
        return Ok(());
    };

    eprint!("{} rows to upload\n{}", thousands(prepared.summary.rows), details(&prepared.summary));
    eprintln!("  {:<11}{}", "compressed", size(prepared.body.len()));
    if let Some(previous) = &prepared.previous {
        eprintln!(
            "  {:<11}job {}, {}; sending what came after",
            "previously",
            previous.job_id,
            utc_minute(previous.uploaded_at_ms)
        );
    }
    note_unpositioned(&prepared.summary);

    if !args.yes && !confirm()? {
        eprintln!("not uploaded");
        return Ok(());
    }

    let client = Client::new(BASE, prepared.key.clone());
    let sent = send(&client, &prepared, &db)?;
    let job = sent.job;
    eprintln!("uploaded; WDGWars queued it as job {job}");
    let started = Instant::now();
    let waited = wait(
        job,
        GIVE_UP,
        || client.poll(job),
        std::thread::sleep,
        || started.elapsed(),
        on_stderr(job),
    );
    finish(&sent, &waited)?;
    match waited? {
        Waited::Done(result) => print!("{}", outcome(&result)),
        Waited::Unfollowed(reason) => eprintln!(
            "WDGWars accepted the upload as job {job}. Its import carries on; check {} or your \
             profile on wdgwars.pl for the result. wartui stopped following it: {reason}.",
            client.job_url(job)
        ),
    }
    Ok(())
}

/// What is ready to send.
pub(crate) struct Prepared {
    pub(crate) key: String,
    pub(crate) body: Vec<u8>,
    pub(crate) summary: ExportSummary,
    /// The upload this one follows on from. `None` with `--resend`, which follows on from
    /// nothing.
    pub(crate) previous: Option<UploadRecord>,
}

/// Check the key, then build the body from what came after the last upload, or from
/// everything when `resend`. `None` when there is no row to send, which is said on standard
/// error. Contacts nothing, so every refusal here costs no request.
pub(crate) fn prepare(
    config: &Config,
    config_path: Option<&Path>,
    db: &Path,
    mut filter: ExportFilter,
    resend: bool,
) -> Result<Option<Prepared>> {
    let key = config.api_keys.wdgwars.trim();
    if key.is_empty() {
        let file =
            config_path.map_or_else(|| "wartui.toml".to_owned(), |p| p.display().to_string());
        bail!(
            "no WDGWars API key; paste one in the settings modal (`c`) or set `wdgwars` \
             under `[api-keys]` in {file}"
        );
    }
    filter.after_uploads = !resend;
    let (body, summary, previous) = body(db, filter)?;
    if summary.rows == 0 {
        match &previous {
            Some(previous) => eprintln!(
                "everything in {} was uploaded (job {}, {}); --resend sends it again",
                db.display(),
                previous.job_id,
                utc_minute(previous.uploaded_at_ms)
            ),
            None => eprintln!("{} has no rows to upload", db.display()),
        }
        note_unpositioned(&summary);
        return Ok(None);
    }
    if body.len() > MAX_BODY {
        bail!(
            "the upload is {} compressed, over WDGWars' limit of {}",
            size(body.len()),
            size(MAX_BODY)
        );
    }
    Ok(Some(Prepared { key: key.to_owned(), body, summary, previous }))
}

/// The capture's WiGLE CSV, gzipped in memory, what went into it, and the upload it follows
/// on from when it is [`ExportFilter::after_uploads`].
fn body(db: &Path, filter: ExportFilter) -> Result<(Vec<u8>, ExportSummary, Option<UploadRecord>)> {
    let conn = open_readonly(db).with_context(|| format!("opening {}", db.display()))?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    let summary = wigle_csv(&conn, filter, &mut gz, env!("CARGO_PKG_VERSION"))?;
    let body = gz.finish().context("compressing the upload")?;
    let previous = if filter.after_uploads { last_upload(&conn)? } else { None };
    Ok((body, summary, previous))
}

/// An upload the site queued.
pub(crate) struct Sent {
    pub(crate) job: u64,
    /// The capture's record of it.
    record: i64,
    /// The capture, open for writing since before the upload went.
    conn: Connection,
}

/// Open `db` for writing, hand `prepared` to the site, then record that the site queued it.
pub(crate) fn send(client: &Client, prepared: &Prepared, db: &Path) -> Result<Sent> {
    let conn = open_readwrite(db).with_context(|| format!("opening {}", db.display()))?;
    let job = client.submit(&prepared.body, &filename(db))?;
    let through = prepared.summary.last_id.expect("a capture with rows has a last sighting");
    let now = Utc::now().timestamp_millis();
    let record = record_upload(&conn, through, now, job, prepared.summary.rows)
        .with_context(|| format!("recording job {job} in {}", db.display()))?;
    Ok(Sent { job, record, conn })
}

/// Record how the wait for `sent` ended.
pub(crate) fn finish(sent: &Sent, waited: &Result<Waited>) -> Result<()> {
    let result = match waited {
        Ok(Waited::Done(_)) => "done",
        Ok(Waited::Unfollowed(_)) => "unfollowed",
        Err(_) => "failed",
    };
    set_upload_result(&sent.conn, sent.record, result)
        .with_context(|| format!("recording how job {} ended", sent.job))?;
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

/// Ask on the terminal. `run` has already refused when standard input is not one.
fn confirm() -> Result<bool> {
    eprint!("Upload to WDGWars? [y/N] ");
    std::io::stderr().flush().context("writing the question")?;
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).context("reading the answer")?;
    Ok(matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

/// How a wait for a job ended, short of the job failing.
#[derive(Debug, PartialEq)]
pub(crate) enum Waited {
    /// Imported, with the counts the site sent.
    Done(Map<String, Value>),
    /// Not followed to the end, and why. The upload stands.
    Unfollowed(String),
}

/// What a wait has to report while it polls.
#[derive(Debug, PartialEq)]
pub(crate) enum Progress {
    /// The job is `queued` or `processing`.
    State(&'static str),
    /// A poll failed, and the wait carries on.
    PollError(String),
}

/// Poll `job` until it ends or `give_up` has passed on `elapsed`, sleeping between polls
/// with `sleep` but never past `give_up`, and telling `say` of every poll that did not end
/// it. An error only when the site reports the job failed.
pub(crate) fn wait(
    job: u64,
    give_up: Duration,
    mut poll: impl FnMut() -> Result<JobStatus>,
    mut sleep: impl FnMut(Duration),
    elapsed: impl Fn() -> Duration,
    mut say: impl FnMut(Progress),
) -> Result<Waited> {
    let mut delay = FIRST_POLL;
    let mut said = None;
    loop {
        sleep(delay.min(give_up.saturating_sub(elapsed())));
        match poll() {
            Ok(JobStatus::Done(result)) => return Ok(Waited::Done(result)),
            Ok(JobStatus::Unreadable(reason)) => return Ok(Waited::Unfollowed(reason)),
            Ok(JobStatus::Failed(message)) => {
                bail!("WDGWars could not import job {job}: {message}")
            }
            Ok(state) => {
                let name = if state == JobStatus::Queued { "queued" } else { "processing" };
                said = Some(name);
                say(Progress::State(name));
            }
            Err(error) => say(Progress::PollError(format!("{error:#}"))),
        }
        if elapsed() >= give_up {
            let state = said.map_or_else(String::new, |name| format!("; it is still {name}"));
            return Ok(Waited::Unfollowed(format!(
                "no answer within {} seconds{state}",
                give_up.as_secs()
            )));
        }
        delay = next_delay(delay);
    }
}

/// A `say` for [`wait`] that prints each change of state and each distinct poll error.
fn on_stderr(job: u64) -> impl FnMut(Progress) {
    let mut said = None;
    let mut last_error = None;
    move |progress| match progress {
        Progress::State(name) => {
            if said != Some(name) {
                eprintln!("job {job}: {name}");
                said = Some(name);
            }
        }
        Progress::PollError(message) => {
            if last_error.as_ref() != Some(&message) {
                eprintln!("job {job}: asking after it failed ({message}); trying again");
                last_error = Some(message);
            }
        }
    }
}

/// The wait after `delay`: half as long again, up to [`MAX_POLL`].
fn next_delay(delay: Duration) -> Duration {
    delay.mul_f64(1.5).min(MAX_POLL)
}

/// A finished job's counts, `imported`, `captured` and `updated` first, then the rest in
/// the order the site's JSON sorts into.
fn outcome(result: &Map<String, Value>) -> String {
    const FIRST: [&str; 3] = ["imported", "captured", "updated"];
    let ordered = FIRST
        .iter()
        .filter_map(|key| result.get_key_value(*key))
        .chain(result.iter().filter(|(key, _)| !FIRST.contains(&key.as_str())));
    let mut text = String::new();
    for (key, value) in ordered {
        let value = match value {
            Value::Number(n) => n.as_u64().map_or_else(|| n.to_string(), thousands),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        text.push_str(&format!("{key:<10} {value}\n"));
    }
    if text.is_empty() {
        text.push_str("done; WDGWars sent no counts\n");
    }
    text
}

/// `bytes` in decimal units, the ones [`MAX_BODY`] is given in.
pub(crate) fn size(bytes: usize) -> String {
    const MB: usize = 1_000_000;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1_000))
    }
}

/// Where an import job stands.
#[derive(Debug, PartialEq)]
pub enum JobStatus {
    Queued,
    Processing,
    /// Imported, with the counts the site sent.
    Done(Map<String, Value>),
    /// Refused, with the site's reason.
    Failed(String),
    /// Not said to an API key, and why: the site sent it to the login page or refused it.
    Unreadable(String),
}

/// The site redirected a request to its login page: it wants a logged-in session, not the
/// API key.
#[derive(Debug)]
struct LoginRequired {
    location: String,
}

impl std::fmt::Display for LoginRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "WDGWars redirected to its login page ({}): the endpoint wants a logged-in \
             session rather than the API key",
            self.location
        )
    }
}

impl std::error::Error for LoginRequired {}

/// The site answered 401 or 403, with its reason when it gave one.
#[derive(Debug)]
struct KeyRejected {
    reason: Option<String>,
}

impl std::fmt::Display for KeyRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.reason {
            Some(reason) => write!(f, "WDGWars rejected the API key: {reason}"),
            None => write!(f, "WDGWars rejected the API key"),
        }
    }
}

impl std::error::Error for KeyRejected {}

/// The leaderboard's CSV queue.
pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    /// A client for the site at `base`. Statuses are mapped here rather than by `ureq`,
    /// so a refusal is reported in the site's own words, and redirects reach [`answer`]
    /// rather than being followed.
    pub fn new(base: &str, key: String) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(30)))
            .timeout_recv_response(Some(RECV_TIMEOUT))
            .timeout_recv_body(Some(RECV_TIMEOUT))
            .build()
            .into();
        Self { base: base.trim_end_matches('/').to_owned(), key, agent }
    }

    /// Hand over `body` as `filename` and return the job it was queued as.
    pub fn submit(&self, body: &[u8], filename: &str) -> Result<u64> {
        let url = format!("{}/api/v2/upload-csv", self.base);
        let part = Part::bytes(body).file_name(filename).mime_str("application/gzip")?;
        let form = Form::new().part("file", part);
        let response = self
            .agent
            .post(&url)
            .header("X-API-Key", &self.key)
            .send(form)
            .with_context(|| format!("sending the upload to {url}"))?;
        parse_submit(&answer(response)?)
    }

    /// Ask where `job` stands. A redirect to the login page, or a refused key, is
    /// [`JobStatus::Unreadable`]: the job exists, and the site will not tell an API key
    /// about it.
    pub fn poll(&self, job: u64) -> Result<JobStatus> {
        let url = self.job_url(job);
        let response = self
            .agent
            .get(&url)
            .header("X-API-Key", &self.key)
            .config()
            .timeout_global(Some(POLL_TIMEOUT))
            .build()
            .call()
            .with_context(|| format!("reaching {url}"))?;
        match answer(response) {
            Ok(text) => parse_job(&text),
            Err(error) if error.is::<LoginRequired>() || error.is::<KeyRejected>() => {
                Ok(JobStatus::Unreadable(error.to_string()))
            }
            Err(error) => Err(error),
        }
    }

    fn job_url(&self, job: u64) -> String {
        format!("{}/api/v2/upload-job/{job}", self.base)
    }
}

/// A response's body, or why its status means it has none worth reading. The status decides
/// first: a refusal's body is read only to quote, so one that cannot be read or is not UTF-8
/// still gives the status's message. A redirect's body is not read: it is the page being
/// redirected from, not an answer.
fn answer(mut response: ureq::http::Response<ureq::Body>) -> Result<String> {
    let status = response.status();
    if status.is_redirection() {
        let location = response
            .headers()
            .get(ureq::http::header::LOCATION)
            .map_or_else(|| "nowhere".to_owned(), |l| String::from_utf8_lossy(l.as_bytes()).into());
        if location.contains("/login") {
            return Err(LoginRequired { location }.into());
        }
        bail!("WDGWars redirected to {location}");
    }
    let body = response.body_mut().with_config().limit(1024 * 1024).read_to_vec();
    if status.is_success() {
        let bytes = body.context("reading WDGWars' answer")?;
        return String::from_utf8(bytes).context("WDGWars' answer is not UTF-8");
    }
    let text = body.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
    match status.as_u16() {
        401 | 403 => Err(KeyRejected { reason: site_reason(&text) }.into()),
        413 => bail!("WDGWars refused the upload as too large"),
        code => bail!("WDGWars answered {code}: {}", quote(&text)),
    }
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

/// The job a submit was queued as.
fn parse_submit(text: &str) -> Result<u64> {
    let object = object(text)?;
    refused(&object, text)?;
    object
        .get("job_id")
        .and_then(Value::as_u64)
        .with_context(|| format!("WDGWars accepted the upload but named no job: {}", quote(text)))
}

/// Where a job stands, from a poll's answer.
fn parse_job(text: &str) -> Result<JobStatus> {
    let object = object(text)?;
    refused(&object, text)?;
    match object.get("status").and_then(Value::as_str) {
        Some("queued") => Ok(JobStatus::Queued),
        Some("processing") => Ok(JobStatus::Processing),
        Some("done") => {
            let result = match object.get("result") {
                Some(Value::Object(result)) => result.clone(),
                _ => Map::new(),
            };
            Ok(JobStatus::Done(result))
        }
        Some("failed") => {
            let result = object.get("result").and_then(Value::as_object);
            let message =
                reason(&object).or_else(|| result.and_then(reason)).unwrap_or_else(|| quote(text));
            Ok(JobStatus::Failed(message))
        }
        _ => bail!("WDGWars sent a job status this build does not know: {}", quote(text)),
    }
}

/// `text` as a JSON object.
fn object(text: &str) -> Result<Map<String, Value>> {
    match serde_json::from_str(text) {
        Ok(Value::Object(object)) => Ok(object),
        _ => bail!("WDGWars answered with something other than a JSON object: {}", quote(text)),
    }
}

/// Refuse an answer that says `"ok": false`, in its own words.
fn refused(object: &Map<String, Value>, text: &str) -> Result<()> {
    if object.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!("WDGWars refused: {}", reason(object).unwrap_or_else(|| quote(text)));
    }
    Ok(())
}

/// The site's explanation, under `error` or `message`.
fn reason(object: &Map<String, Value>) -> Option<String> {
    ["error", "message"].iter().find_map(|key| object.get(*key)?.as_str().map(str::to_owned))
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::Path;
    use std::time::Duration;

    use flate2::read::GzDecoder;
    use wartui_core::export::{ExportFilter, wigle_csv};
    use wartui_core::position::{Fix, PositionSource};
    use wartui_core::record::{Observation, Record};
    use wartui_core::store::{
        CaptureInfo, Store, StoreConfig, last_upload, open_readonly, open_readwrite, record_upload,
        set_upload_result,
    };
    use wartui_proto::air::RecordKind;
    use wartui_proto::plan::ChannelPool;

    use super::{
        Client, GIVE_UP, JobStatus, Sent, Waited, body, finish, next_delay, outcome, parse_job,
        parse_submit, prepare, send, size, wait,
    };
    use crate::config::Config;

    const EPOCH_MS: i64 = 1_777_642_477_000;

    fn sighting(bssid: [u8; 6], at_ms: i64, lat: Option<f64>) -> Record {
        Record::Observation(Observation {
            node_mac: [0x02, 0x00, 0x5E, 0x10, 0x57, 0x84],
            rx_at_ms: at_ms,
            link_rssi: Some(-41),
            bssid,
            ssid: b"example".to_vec(),
            security: "[WPA2_PSK]".to_owned(),
            channel: 6,
            rssi: -60,
            kind: RecordKind::Wifi,
            rcoi: None,
            mfgr_id: None,
            fix: Fix {
                lat,
                lon: lat.map(|_| -122.0),
                alt: None,
                accuracy: None,
                source: PositionSource::Static,
                at_ms: None,
            },
            raw_body: Vec::new(),
        })
    }

    /// A capture at `dir/wartui.db` holding `records`.
    fn capture(dir: &tempfile::TempDir, records: Vec<Record>) -> std::path::PathBuf {
        let path = dir.path().join("wartui.db");
        let mut config = StoreConfig::new(&path);
        config.batch_interval = Duration::from_millis(10);
        let info = CaptureInfo { pool: ChannelPool::Us, notes: None };
        let store = Store::create(&config, &info, EPOCH_MS).expect("creating the store");
        assert_eq!(store.submit(records), 0, "nothing should have been dropped");
        store.close();
        path
    }

    fn keyed() -> Config {
        let mut config = Config::default();
        config.api_keys.wdgwars = "secret".to_owned();
        config
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
        let (gz, summary, _) = body(&db, ExportFilter::default()).unwrap();
        assert_eq!(summary.rows, 2);

        let mut csv = Vec::new();
        GzDecoder::new(gz.as_slice()).read_to_end(&mut csv).unwrap();
        let mut expected = Vec::new();
        let conn = open_readonly(&db).unwrap();
        wigle_csv(&conn, ExportFilter::default(), &mut expected, env!("CARGO_PKG_VERSION"))
            .unwrap();
        assert_eq!(csv, expected);
    }

    #[test]
    fn upload_refuses_without_contacting_server_when_key_empty() {
        // `prepare` takes no client, so a refusal here cannot have sent anything; the
        // capture does not even exist.
        let error = prepare(
            &Config::default(),
            Some(Path::new("/home/op/.config/wartui/wartui.toml")),
            Path::new("/nonexistent/wartui.db"),
            ExportFilter::default(),
            false,
        )
        .err()
        .unwrap()
        .to_string();
        assert!(error.contains("no WDGWars API key"), "{error}");
        assert!(error.contains("/home/op/.config/wartui/wartui.toml"), "{error}");
    }

    #[test]
    fn upload_sends_nothing_when_capture_has_no_positioned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, None)]);
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default(), false).unwrap();
        assert!(prepared.is_none());
    }

    /// Three networks heard a minute apart, positioned, with an upload recorded through
    /// the second.
    fn uploaded_through_second(dir: &tempfile::TempDir) -> std::path::PathBuf {
        let db = capture(
            dir,
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 60_000, Some(37.1)),
                sighting([0x10, 0, 0, 0, 0, 3], EPOCH_MS + 120_000, Some(37.2)),
            ],
        );
        let conn = open_readwrite(&db).unwrap();
        let id = record_upload(&conn, 2, EPOCH_MS + 90_000, 7, 2).unwrap();
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
    fn upload_records_job_and_cutoff_when_submit_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default(), false)
            .unwrap()
            .expect("a row to send");
        assert_eq!(prepared.previous, None);

        let (base, server) = serve_once("202 Accepted", r#"{"ok":true,"job_id":42}"#);
        let sent = send(&Client::new(&base, prepared.key.clone()), &prepared, &db).unwrap();
        server.join().unwrap();
        assert_eq!(sent.job, 42);

        // Everything went, so a repeat has nothing to send, and the record names the job.
        assert!(prepare(&keyed(), None, &db, ExportFilter::default(), false).unwrap().is_none());
        let previous = last_upload(&open_readonly(&db).unwrap()).unwrap().expect("a record");
        assert_eq!((previous.job_id, previous.rows), (42, 1));
    }

    #[test]
    fn upload_sends_nothing_when_capture_not_writable() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(&dir, vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default(), false)
            .unwrap()
            .expect("a row to send");
        let mut permissions = std::fs::metadata(&db).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&db, permissions).unwrap();
        if std::fs::OpenOptions::new().write(true).open(&db).is_ok() {
            // Running as root, which file permissions do not stop.
            return;
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let client = Client::new(&base, prepared.key.clone());
        let error = send(&client, &prepared, &db).err().expect("refused");
        assert!(format!("{error:#}").contains("cannot be written"), "{error:#}");
        listener.set_nonblocking(true).unwrap();
        let accepted = listener.accept();
        assert!(accepted.is_err(), "the upload was sent anyway");
    }

    #[test]
    fn upload_sends_only_new_sightings_when_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default(), false)
            .unwrap()
            .expect("a row to send");
        assert_eq!(macs(&prepared.body), ["10:00:00:00:00:03"]);
        assert_eq!(prepared.previous.map(|p| p.job_id), Some(7));
    }

    #[test]
    fn upload_sends_everything_when_resend() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default(), true)
            .unwrap()
            .expect("rows to send");
        assert_eq!(prepared.summary.rows, 3);
        assert_eq!(prepared.previous, None);
    }

    #[test]
    fn upload_finish_records_failed_when_job_failed() {
        // A failed job is no cutoff, so the next upload sends what it carried again.
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(&dir);
        let conn = open_readwrite(&db).unwrap();
        let record = record_upload(&conn, 3, EPOCH_MS + 150_000, 8, 1).unwrap();
        finish(&Sent { job: 8, record, conn }, &Err(anyhow::anyhow!("failed"))).unwrap();

        let previous = last_upload(&open_readonly(&db).unwrap()).unwrap().expect("a record");
        assert_eq!(previous.job_id, 7);
    }

    #[test]
    fn upload_parser_reads_job_id_when_submit_returns_accepted() {
        let id = parse_submit(r#"{"ok":true,"job_id":42,"poll_url":"/api/v2/upload-job/42"}"#);
        assert_eq!(id.unwrap(), 42);
    }

    #[test]
    fn upload_parser_returns_queued_when_job_waiting() {
        let status = parse_job(r#"{"ok":true,"job_id":42,"status":"queued"}"#).unwrap();
        assert_eq!(status, JobStatus::Queued);
    }

    #[test]
    fn upload_parser_returns_processing_when_job_running() {
        let status = parse_job(r#"{"ok":true,"job_id":42,"status":"processing"}"#).unwrap();
        assert_eq!(status, JobStatus::Processing);
    }

    #[test]
    fn upload_parser_returns_done_with_result_when_job_finished() {
        let status = parse_job(
            r#"{"ok":true,"job_id":42,"status":"done",
                "result":{"updated":3,"imported":1200,"captured":17,"skipped":4}}"#,
        )
        .unwrap();
        let JobStatus::Done(result) = status else { panic!("{status:?}") };
        assert_eq!(result.get("imported").and_then(|v| v.as_u64()), Some(1200));
        assert_eq!(
            outcome(&result),
            "imported   1,200\ncaptured   17\nupdated    3\nskipped    4\n"
        );
    }

    #[test]
    fn upload_parser_returns_failed_with_message_when_job_failed() {
        let status = parse_job(r#"{"ok":true,"job_id":42,"status":"failed","error":"bad header"}"#);
        assert_eq!(status.unwrap(), JobStatus::Failed("bad header".to_owned()));
        let status = parse_job(r#"{"ok":true,"job_id":42,"status":"failed","message":"no rows"}"#);
        assert_eq!(status.unwrap(), JobStatus::Failed("no rows".to_owned()));
        // With neither, the operator gets the raw answer rather than nothing.
        let raw = r#"{"ok":true,"job_id":42,"status":"failed"}"#;
        assert_eq!(parse_job(raw).unwrap(), JobStatus::Failed(raw.to_owned()));
    }

    #[test]
    fn upload_backoff_grows_to_cap_when_polling() {
        assert_eq!(next_delay(Duration::from_secs(2)), Duration::from_secs(3));
        assert_eq!(next_delay(Duration::from_secs(8)), Duration::from_secs(10));
        assert_eq!(next_delay(Duration::from_secs(10)), Duration::from_secs(10));
    }

    /// A server on a free local port that answers one request with `status` and `body`,
    /// and hands back the request it read.
    fn serve_once(status: &str, body: &str) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        serve_once_with(status, "", body.as_bytes())
    }

    /// [`serve_once`], with `headers` (each ending `\r\n`) added to the response.
    fn serve_once_with(
        status: &str,
        headers: &str,
        body: &[u8],
    ) -> (String, std::thread::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buf = [0u8; 8192];
            let header_end = loop {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0, "the client hung up mid-request");
                request.extend_from_slice(&buf[..n]);
                if let Some(at) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    break at + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
            let length = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .map_or(0, |n| n.trim().parse::<usize>().unwrap());
            while request.len() < header_end + length {
                let n = stream.read(&mut buf).unwrap();
                assert!(n > 0, "the client hung up mid-body");
                request.extend_from_slice(&buf[..n]);
            }
            stream.write_all(&response).unwrap();
            // A followed redirect would come back for a second request; none may.
            listener.set_nonblocking(true).unwrap();
            std::thread::sleep(Duration::from_millis(200));
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let n = stream.read(&mut buf).unwrap_or(0);
                panic!("a second request arrived: {}", String::from_utf8_lossy(&buf[..n]));
            }
            request
        });
        (base, handle)
    }

    #[test]
    fn upload_client_sends_api_key_and_multipart_file_when_submitting() {
        let (base, server) = serve_once(
            "202 Accepted",
            r#"{"ok":true,"job_id":42,"poll_url":"/api/v2/upload-job/42"}"#,
        );
        let client = Client::new(&base, "secret".to_owned());
        let job = client.submit(b"gzipped bytes", "wartui-2026-09-30-19-02-11.csv.gz").unwrap();
        assert_eq!(job, 42);

        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /api/v2/upload-csv "), "{request}");
        assert!(request.to_ascii_lowercase().contains("x-api-key: secret\r\n"), "{request}");
        assert!(request.contains("name=\"file\""), "{request}");
        assert!(request.contains("filename=\"wartui-2026-09-30-19-02-11.csv.gz\""), "{request}");
        assert!(request.contains("gzipped bytes"), "{request}");
    }

    #[test]
    fn upload_client_reports_key_rejected_when_server_returns_401() {
        let (base, server) = serve_once("401 Unauthorized", r#"{"ok":false,"error":"bad key"}"#);
        let client = Client::new(&base, "wrong".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err().to_string();
        assert!(error.contains("rejected the API key"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn upload_client_quotes_answer_when_server_returns_unexpected_status() {
        let (base, server) = serve_once("500 Internal Server Error", "it broke");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.poll(42).unwrap_err().to_string();
        assert!(error.contains("WDGWars answered 500: it broke"), "{error}");
        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("GET /api/v2/upload-job/42 "), "{request}");
    }

    #[test]
    fn upload_client_reports_login_required_when_poll_redirects_to_login() {
        let location = "/login/?next=%2Fendpoint%2Fv2%2Fupload-job%2F42";
        let (base, server) =
            serve_once_with("302 Found", &format!("Location: {location}\r\n"), b"");
        let client = Client::new(&base, "secret".to_owned());
        let status = client.poll(42).unwrap();
        let JobStatus::Unreadable(reason) = status else { panic!("{status:?}") };
        assert!(reason.contains(location), "{reason}");
        // `serve_once` panics, failing the join, if the redirect was followed.
        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("GET /api/v2/upload-job/42 "), "{request}");
    }

    #[test]
    fn upload_client_reports_redirect_when_submit_redirects() {
        let (base, server) = serve_once_with("302 Found", "Location: /elsewhere\r\n", b"");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err().to_string();
        assert!(error.contains("WDGWars redirected to /elsewhere"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_login_required_when_submit_redirects_to_login() {
        // Unlike a poll, a submit sent to the login page uploaded nothing.
        let (base, server) = serve_once_with("302 Found", "Location: /login/\r\n", b"");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err().to_string();
        assert!(error.contains("wants a logged-in session"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn upload_client_includes_site_reason_when_submit_returns_401() {
        let (base, server) =
            serve_once("401 Unauthorized", r#"{"ok":false,"error":"key revoked"}"#);
        let client = Client::new(&base, "wrong".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err().to_string();
        assert_eq!(error, "WDGWars rejected the API key: key revoked");
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_unreadable_when_poll_returns_403() {
        // The upload already happened, so a poll the key may not make is "cannot follow",
        // not a failure.
        let (base, server) = serve_once("403 Forbidden", r#"{"ok":false,"message":"no scope"}"#);
        let client = Client::new(&base, "secret".to_owned());
        let status = client.poll(42).unwrap();
        assert_eq!(
            status,
            JobStatus::Unreadable("WDGWars rejected the API key: no scope".to_owned())
        );
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_status_when_error_body_is_not_utf8() {
        let (base, server) = serve_once_with("413 Payload Too Large", "", b"\xff\xfe too big");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err().to_string();
        assert!(error.contains("refused the upload as too large"), "{error}");
        server.join().unwrap();
    }

    #[test]
    fn upload_size_formats_decimal_units_when_bytes_large() {
        assert_eq!(size(40_000_000), "40.0 MB");
        assert_eq!(size(1_500_000), "1.5 MB");
        assert_eq!(size(12_345), "13 KB");
        assert_eq!(size(1_000), "1 KB");
    }

    /// `wait` on a fake clock that advances only by what it sleeps, answering each poll
    /// from `answers` in turn.
    fn wait_on(
        give_up: Duration,
        answers: Vec<anyhow::Result<JobStatus>>,
    ) -> anyhow::Result<Waited> {
        let clock = std::cell::Cell::new(Duration::ZERO);
        let mut answers = answers.into_iter();
        wait(
            42,
            give_up,
            || answers.next().unwrap_or(Ok(JobStatus::Processing)),
            |delay| clock.set(clock.get() + delay),
            || clock.get(),
            |_| {},
        )
    }

    #[test]
    fn upload_wait_returns_done_when_transient_error_precedes_done() {
        let waited = wait_on(
            Duration::from_secs(900),
            vec![
                Ok(JobStatus::Queued),
                Err(anyhow::anyhow!("WDGWars answered 502: bad gateway")),
                Err(anyhow::anyhow!("WDGWars answered 502: bad gateway")),
                Ok(JobStatus::Done(serde_json::Map::new())),
            ],
        );
        assert_eq!(waited.unwrap(), Waited::Done(serde_json::Map::new()));
    }

    #[test]
    fn upload_wait_returns_unfollowed_when_give_up_reached() {
        let waited = wait_on(GIVE_UP, vec![Ok(JobStatus::Queued)]).unwrap();
        let Waited::Unfollowed(reason) = waited else { panic!("{waited:?}") };
        assert_eq!(reason, "no answer within 30 seconds; it is still processing");
    }

    #[test]
    fn upload_wait_sleeps_no_longer_than_give_up_when_job_never_ends() {
        for secs in [1, 3, 29, 30, 31, 47] {
            let give_up = Duration::from_secs(secs);
            let slept = std::cell::Cell::new(Duration::ZERO);
            let waited = wait(
                42,
                give_up,
                || Ok(JobStatus::Queued),
                |delay| slept.set(slept.get() + delay),
                || slept.get(),
                |_| {},
            );
            assert!(matches!(waited, Ok(Waited::Unfollowed(_))), "{waited:?}");
            assert!(slept.get() <= give_up, "{secs}s: slept {:?}", slept.get());
        }
    }

    #[test]
    fn upload_wait_returns_unfollowed_when_only_errors_until_give_up() {
        let errors = (0..100).map(|_| Err(anyhow::anyhow!("timed out"))).collect();
        let waited = wait_on(Duration::from_secs(60), errors).unwrap();
        assert!(matches!(waited, Waited::Unfollowed(_)), "{waited:?}");
    }

    #[test]
    fn upload_wait_fails_when_job_failed() {
        let error = wait_on(Duration::from_secs(60), vec![Ok(JobStatus::Failed("bad".to_owned()))])
            .unwrap_err()
            .to_string();
        assert_eq!(error, "WDGWars could not import job 42: bad");
    }
}
