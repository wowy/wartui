//! Uploading to the WDGWars leaderboard.
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
//! Once submit returns a job id, the upload has happened. A poll that errors is retried until
//! the give-up time; a poll the site will not answer (a redirect to its login page, or a 401 or
//! 403), and running out of time, end the wait as "cannot follow", which is no failure: the
//! profile on the site shows the result. Only a job the site reports as failed is one.
//! Redirects are not followed, because an API that redirects is not answering as the API, and
//! following one reads an HTML page as JSON.
//!
//! The wait gives up after 30 seconds and 2 more per thousand rows, at most two minutes
//! ([`give_up`]): the site takes longer to import a larger file, and a fixed wait leaves the
//! largest uploads unfollowed. A person sits at the terminal for it, and nothing needs the result
//! to finish: the job's URL and the profile on the site show it later, so the wait stays short of
//! what a slow import could take. Each poll has its own short deadline and no sleep crosses the
//! give-up time, so one hung poll cannot stretch the wait. A wait that ends with no answer at all
//! names the last poll's error, which tells a lost connection from a slow import.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value};
use ureq::unversioned::multipart::{Form, Part};
use wartui_core::store::UploadSite;

use super::{agent, location, object, quote, reason, site_reason};
use crate::text::thousands;

/// The leaderboard.
pub(crate) const BASE: &str = "https://wdgwars.pl";

/// The largest CSV the queue accepts, counted before compression. The site counts a gzipped
/// file by its decompressed size and says 40 MB; the decimal reading is the smaller, so it is
/// the safe one.
pub(crate) const MAX_CSV: usize = 40_000_000;

/// How long to wait before the first poll, and the ceiling the wait grows to.
const FIRST_POLL: Duration = Duration::from_secs(2);
const MAX_POLL: Duration = Duration::from_secs(10);

/// How long to keep asking after a job of `rows` before leaving it to the site: 30 seconds, and
/// 2 more per thousand rows, at most [`MAX_GIVE_UP`].
pub(crate) fn give_up(rows: u64) -> Duration {
    Duration::from_millis(30_000u64.saturating_add(rows.saturating_mul(2))).min(MAX_GIVE_UP)
}

/// The longest wait [`give_up`] gives, however large the file.
const MAX_GIVE_UP: Duration = Duration::from_secs(120);

/// How long one poll may take, start to finish.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// The site reports `job` failed, with its reason.
#[derive(Debug)]
pub(crate) struct JobFailed {
    pub(crate) job: u64,
    pub(crate) message: String,
}

impl std::fmt::Display for JobFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WDGWars could not import job {}: {}", self.job, self.message)
    }
}

impl std::error::Error for JobFailed {}

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
    let mut last_error = None;
    loop {
        sleep(delay.min(give_up.saturating_sub(elapsed())));
        match poll() {
            Ok(JobStatus::Done(result)) => return Ok(Waited::Done(result)),
            Ok(JobStatus::Unreadable(reason)) => return Ok(Waited::Unfollowed(reason)),
            Ok(JobStatus::Failed(message)) => return Err(JobFailed { job, message }.into()),
            Ok(state) => {
                let name = if state == JobStatus::Queued { "queued" } else { "processing" };
                said = Some(name);
                say(Progress::State(name));
            }
            Err(error) => {
                let message = format!("{error:#}");
                tracing::warn!(job, error = %message, "WDGWars poll failed");
                say(Progress::PollError(message.clone()));
                last_error = Some(message);
            }
        }
        if elapsed() >= give_up {
            let why = match (said, last_error) {
                (Some(name), _) => format!("; it is still {name}"),
                (None, Some(error)) => format!("; the last poll failed: {error}"),
                (None, None) => String::new(),
            };
            // Rounded, since the wait is in milliseconds.
            let secs = (give_up.as_millis() + 500) / 1000;
            return Ok(Waited::Unfollowed(format!("no answer within {secs} seconds{why}")));
        }
        delay = next_delay(delay);
    }
}

/// A `say` for [`wait`] that prints each change of state and each distinct poll error.
pub(crate) fn on_stderr(job: u64) -> impl FnMut(Progress) {
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

/// A finished job's counts as `(key, value)`, `imported`, `captured` and `updated` first,
/// then the rest in the order the site's JSON sorts into.
pub(crate) fn counts(result: &Map<String, Value>) -> Vec<(String, String)> {
    const FIRST: [&str; 3] = ["imported", "captured", "updated"];
    let ordered = FIRST
        .iter()
        .filter_map(|key| result.get_key_value(*key))
        .chain(result.iter().filter(|(key, _)| !FIRST.contains(&key.as_str())));
    ordered
        .map(|(key, value)| {
            let value = match value {
                Value::Number(n) => n.as_u64().map_or_else(|| n.to_string(), thousands),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (key.clone(), value)
        })
        .collect()
}

/// A finished job's [`counts`], a line each.
pub(crate) fn outcome(result: &Map<String, Value>) -> String {
    let mut text = String::new();
    for (key, value) in counts(result) {
        text.push_str(&format!("{key:<10} {value}\n"));
    }
    if text.is_empty() {
        text.push_str("done; WDGWars sent no counts\n");
    }
    text
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
    /// A client for the site at `base`.
    pub fn new(base: &str, key: String) -> Self {
        Self { base: base.trim_end_matches('/').to_owned(), key, agent: agent() }
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

    pub(crate) fn job_url(&self, job: u64) -> String {
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
        let location = location(&response);
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

/// The job a submit was queued as.
fn parse_submit(text: &str) -> Result<u64> {
    let object = object(UploadSite::Wdgwars, text)?;
    refused(&object, text)?;
    object
        .get("job_id")
        .and_then(Value::as_u64)
        .with_context(|| format!("WDGWars accepted the upload but named no job: {}", quote(text)))
}

/// Where a job stands, from a poll's answer.
fn parse_job(text: &str) -> Result<JobStatus> {
    let object = object(UploadSite::Wdgwars, text)?;
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

/// Refuse an answer that says `"ok": false`, in its own words.
fn refused(object: &Map<String, Value>, text: &str) -> Result<()> {
    if object.get("ok").and_then(Value::as_bool) == Some(false) {
        bail!("WDGWars refused: {}", reason(object).unwrap_or_else(|| quote(text)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Client, JobStatus, Waited, give_up, next_delay, outcome, parse_job, parse_submit, wait,
    };
    use crate::testing::{serve_once, serve_once_with};

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
        let waited = wait_on(give_up(0), vec![Ok(JobStatus::Queued)]).unwrap();
        let Waited::Unfollowed(reason) = waited else { panic!("{waited:?}") };
        assert_eq!(reason, "no answer within 30 seconds; it is still processing");
    }

    #[test]
    fn upload_give_up_grows_with_rows_when_file_is_larger() {
        assert_eq!(give_up(0), Duration::from_secs(30));
        assert_eq!(give_up(4_819), Duration::from_millis(39_638));
        assert_eq!(give_up(19_969), Duration::from_millis(69_938));
        assert_eq!(give_up(100_000), Duration::from_secs(120), "capped at two minutes");
        assert_eq!(give_up(u64::MAX), Duration::from_secs(120));
    }

    #[test]
    fn upload_wait_names_last_poll_error_when_every_poll_fails() {
        let errors = (0..100).map(|n| Err(anyhow::anyhow!("timed out {n}"))).collect();
        let waited = wait_on(give_up(9_615), errors).unwrap();
        let Waited::Unfollowed(reason) = waited else { panic!("{waited:?}") };
        // 49.2 s, rounded; and the error the wait ended on, not the first.
        assert!(
            reason.starts_with("no answer within 49 seconds; the last poll failed: timed out ")
        );
        assert!(!reason.ends_with("timed out 0"), "{reason}");
    }

    #[test]
    fn upload_wait_names_state_when_errors_follow_a_state() {
        let answers = vec![
            Ok(JobStatus::Queued),
            Err(anyhow::anyhow!("timed out")),
            Err(anyhow::anyhow!("timed out")),
            Err(anyhow::anyhow!("timed out")),
            Err(anyhow::anyhow!("timed out")),
            Err(anyhow::anyhow!("timed out")),
            Err(anyhow::anyhow!("timed out")),
        ];
        let waited = wait_on(give_up(0), answers).unwrap();
        let Waited::Unfollowed(reason) = waited else { panic!("{waited:?}") };
        assert_eq!(reason, "no answer within 30 seconds; it is still queued");
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
