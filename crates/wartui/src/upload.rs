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

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Args as ClapArgs;
use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::{Map, Value};
use ureq::unversioned::multipart::{Form, Part};
use wartui_core::export::{ExportFilter, ExportSummary, wigle_csv};
use wartui_core::store::open_readonly;

use crate::config::{self, Config};
use crate::export::{Selection, details, note_unpositioned, thousands};

/// The leaderboard.
const BASE: &str = "https://wdgwars.pl";

/// The largest file the queue accepts. The site says 40 MB; the decimal reading is the
/// smaller, so it is the safe one.
const MAX_BODY: usize = 40_000_000;

/// How long to wait before the first poll, and the ceiling the wait grows to.
const FIRST_POLL: Duration = Duration::from_secs(2);
const MAX_POLL: Duration = Duration::from_secs(10);

/// How long to keep asking before leaving the job to the site.
const GIVE_UP: Duration = Duration::from_secs(30);

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
}

pub fn run(args: Args) -> Result<()> {
    let config = config::load(args.config.as_deref())?;
    let config_path = config::path(args.config.as_deref());
    let db = args.selection.capture("reading")?;
    if !args.yes && !std::io::stdin().is_terminal() {
        bail!("standard input is not a terminal, so nothing can answer; pass --yes to upload");
    }
    let Some(prepared) = prepare(&config, config_path.as_deref(), &db, args.selection.filter())?
    else {
        return Ok(());
    };

    eprint!("{} rows to upload\n{}", thousands(prepared.summary.rows), details(&prepared.summary));
    eprintln!("  {:<11}{}", "compressed", size(prepared.body.len()));
    note_unpositioned(&prepared.summary);

    if !args.yes && !confirm()? {
        eprintln!("not uploaded");
        return Ok(());
    }

    let client = Client::new(BASE, prepared.key);
    let job = client.submit(&prepared.body, &filename(&db))?;
    eprintln!("uploaded; WDGWars queued it as job {job}");
    let started = Instant::now();
    let waited = wait(job, GIVE_UP, || client.poll(job), std::thread::sleep, || started.elapsed())?;
    match waited {
        Waited::Done(result) => print!("{}", outcome(&result)),
        Waited::Unfollowed(reason) => eprintln!(
            "WDGWars accepted the upload as job {job}. Its import carries on; check {} or your \
             profile on wdgwars.pl for the result. wartui stopped following it: {reason}.",
            client.job_url(job)
        ),
    }
    Ok(())
}

/// What `run` has ready to send.
struct Prepared {
    key: String,
    body: Vec<u8>,
    summary: ExportSummary,
}

/// Check the key, then build the body. `None` when the capture has no row to send, which
/// is said on standard error. Contacts nothing, so every refusal here costs no request.
fn prepare(
    config: &Config,
    config_path: Option<&Path>,
    db: &Path,
    filter: ExportFilter,
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
    let (body, summary) = body(db, filter)?;
    if summary.rows == 0 {
        eprintln!("{} has no rows to upload", db.display());
        note_unpositioned(&summary);
        return Ok(None);
    }
    if body.len() > MAX_BODY {
        bail!(
            "the upload is {} compressed, over WDGWars' {}; send one session at a time \
             with --session ID",
            size(body.len()),
            size(MAX_BODY)
        );
    }
    Ok(Some(Prepared { key: key.to_owned(), body, summary }))
}

/// The capture's WiGLE CSV, gzipped in memory, and what went into it.
fn body(db: &Path, filter: ExportFilter) -> Result<(Vec<u8>, ExportSummary)> {
    let conn = open_readonly(db).with_context(|| format!("opening {}", db.display()))?;
    let mut gz = GzEncoder::new(Vec::new(), Compression::default());
    let summary = wigle_csv(&conn, filter, &mut gz, env!("CARGO_PKG_VERSION"))?;
    let body = gz.finish().context("compressing the upload")?;
    Ok((body, summary))
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
enum Waited {
    /// Imported, with the counts the site sent.
    Done(Map<String, Value>),
    /// Not followed to the end, and why. The upload stands.
    Unfollowed(String),
}

/// Poll `job` until it ends or `give_up` has passed on `elapsed`, sleeping between polls
/// with `sleep` but never past `give_up`, and saying each change of state and each distinct poll error. An error only
/// when the site reports the job failed.
fn wait(
    job: u64,
    give_up: Duration,
    mut poll: impl FnMut() -> Result<JobStatus>,
    mut sleep: impl FnMut(Duration),
    elapsed: impl Fn() -> Duration,
) -> Result<Waited> {
    let mut delay = FIRST_POLL;
    let mut said = None;
    let mut last_error = None;
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
                if said != Some(name) {
                    eprintln!("job {job}: {name}");
                    said = Some(name);
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                if last_error.as_ref() != Some(&message) {
                    eprintln!("job {job}: asking after it failed ({message}); trying again");
                    last_error = Some(message);
                }
            }
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
fn size(bytes: usize) -> String {
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
        413 => bail!("WDGWars refused the upload as too large; send one session with --session ID"),
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
    use wartui_core::store::{SessionInfo, Store, StoreConfig, open_readonly};
    use wartui_proto::air::RecordKind;
    use wartui_proto::plan::ChannelPool;

    use super::{
        Client, GIVE_UP, JobStatus, Waited, body, next_delay, outcome, parse_job, parse_submit,
        prepare, size, wait,
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
        let session = SessionInfo { pool: ChannelPool::Us, notes: None };
        let store = Store::open(&config, &session, EPOCH_MS).expect("opening the store");
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
        let (gz, summary) = body(&db, ExportFilter::default()).unwrap();
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
        let prepared = prepare(&keyed(), None, &db, ExportFilter::default()).unwrap();
        assert!(prepared.is_none());
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
        let job = client.submit(b"gzipped bytes", "wartui-2026-09-30-19-02.csv.gz").unwrap();
        assert_eq!(job, 42);

        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /api/v2/upload-csv "), "{request}");
        assert!(request.to_ascii_lowercase().contains("x-api-key: secret\r\n"), "{request}");
        assert!(request.contains("name=\"file\""), "{request}");
        assert!(request.contains("filename=\"wartui-2026-09-30-19-02.csv.gz\""), "{request}");
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
