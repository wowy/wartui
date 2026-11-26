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
//! largest uploads unfollowed. A person waits on it, and nothing needs the result to finish: the
//! job's URL and the profile on the site show it later, so the wait stays short of what a slow
//! import could take. Each poll has its own short deadline and no sleep crosses the give-up time,
//! so one hung poll cannot stretch the wait. A wait that ends with no answer at all names the last
//! poll's error, which tells a lost connection from a slow import.

use std::time::Duration;

use serde_json::{Map, Value};
use ureq::unversioned::multipart::{Form, Part};

use super::Progress;
use super::http::{
    Body, GZIP, SiteError, agent, body, location, object, quote, reason, site_reason,
};

/// The leaderboard.
pub const BASE: &str = "https://wdgwars.pl";

/// The largest CSV the queue accepts, counted before compression. The site counts a gzipped
/// file by its decompressed size and says 40 MB; the decimal reading is the smaller, so it is
/// the safe one.
pub const MAX_CSV: u64 = 40_000_000;

/// How long to wait before the first poll, and the ceiling the wait grows to.
const FIRST_POLL: Duration = Duration::from_secs(2);
const MAX_POLL: Duration = Duration::from_secs(10);

/// How long to keep asking after a job of `rows` before leaving it to the site: 30 seconds, and
/// 2 more per thousand rows, at most two minutes.
#[must_use]
pub fn give_up(rows: u64) -> Duration {
    Duration::from_millis(30_000u64.saturating_add(rows.saturating_mul(2))).min(MAX_GIVE_UP)
}

/// The longest wait [`give_up`] gives, however large the file.
const MAX_GIVE_UP: Duration = Duration::from_secs(120);

/// How long one poll may take, start to finish.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

/// Where a job stands while it has not ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Queued,
    Processing,
}

/// One count a finished job reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Count {
    /// The site's name for it, such as `imported`.
    pub name: String,
    pub value: CountValue,
}

/// A count's value: a whole number, or whatever else the site sent, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CountValue {
    Number(u64),
    Text(String),
}

/// Where an import job stands.
#[derive(Debug, PartialEq)]
pub enum JobStatus {
    Queued,
    Processing,
    /// Imported, with the counts the site sent: `imported`, `captured` and `updated` first, then
    /// the rest in the order the site's JSON sorts into.
    Done(Vec<Count>),
    /// Refused, with the site's reason.
    Failed(String),
    /// Not said to an API key: the site sent it to the login page or refused the key.
    Unreadable(SiteError),
}

/// How a wait for a job ended.
#[derive(Debug, PartialEq)]
pub enum Waited {
    /// Imported, with the counts the site sent.
    Done(Vec<Count>),
    /// Not followed to the end. The upload stands.
    Unfollowed(Unfollowed),
    /// The site reports the import failed, with its reason. It imported nothing.
    Failed(String),
}

/// Why a wait stopped before the job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unfollowed {
    /// The site will not tell an API key about the job: `LoginRequired` or `CredentialsRejected`.
    Unreadable(SiteError),
    /// The give-up time passed, `secs` rounded to the second. `last`: the state the job was last
    /// in, else the last poll's error, else nothing.
    TimedOut { secs: u64, last: Option<Last> },
}

/// What the polls last heard of a job that timed out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Last {
    State(JobState),
    PollFailed(SiteError),
}

/// Poll `job` until it ends or `give_up` has passed on `elapsed`, sleeping between polls with
/// `sleep` but never past `give_up`, and telling `say` of every poll that did not end it.
pub fn wait(
    job: u64,
    give_up: Duration,
    mut poll: impl FnMut() -> Result<JobStatus, SiteError>,
    mut sleep: impl FnMut(Duration),
    elapsed: impl Fn() -> Duration,
    mut say: impl FnMut(Progress),
) -> Waited {
    let mut delay = FIRST_POLL;
    let mut said = None;
    let mut last_error: Option<SiteError> = None;
    loop {
        sleep(delay.min(give_up.saturating_sub(elapsed())));
        match poll() {
            Ok(JobStatus::Done(counts)) => return Waited::Done(counts),
            Ok(JobStatus::Unreadable(error)) => {
                return Waited::Unfollowed(Unfollowed::Unreadable(error));
            }
            Ok(JobStatus::Failed(message)) => return Waited::Failed(message),
            Ok(JobStatus::Queued) => {
                said = Some(JobState::Queued);
                say(Progress::State { job, state: JobState::Queued });
            }
            Ok(JobStatus::Processing) => {
                said = Some(JobState::Processing);
                say(Progress::State { job, state: JobState::Processing });
            }
            Err(error) => {
                // Once per distinct error.
                if last_error.as_ref() != Some(&error) {
                    tracing::warn!(job, error = ?error, "WDGWars poll failed");
                }
                say(Progress::PollFailed { job, error: error.clone() });
                last_error = Some(error);
            }
        }
        if elapsed() >= give_up {
            let last = said.map(Last::State).or(last_error.map(Last::PollFailed));
            // Rounded, since the wait is in milliseconds.
            let secs = u64::try_from((give_up.as_millis() + 500) / 1000).unwrap_or(u64::MAX);
            return Waited::Unfollowed(Unfollowed::TimedOut { secs, last });
        }
        delay = next_delay(delay);
    }
}

/// The wait after `delay`: half as long again, up to [`MAX_POLL`].
fn next_delay(delay: Duration) -> Duration {
    delay.mul_f64(1.5).min(MAX_POLL)
}

/// A finished job's counts, `imported`, `captured` and `updated` first, then the rest in the
/// order the site's JSON sorts into.
fn counts(result: &Map<String, Value>) -> Vec<Count> {
    const FIRST: [&str; 3] = ["imported", "captured", "updated"];
    let ordered = FIRST
        .iter()
        .filter_map(|key| result.get_key_value(*key))
        .chain(result.iter().filter(|(key, _)| !FIRST.contains(&key.as_str())));
    ordered
        .map(|(key, value)| {
            let value = match value {
                Value::Number(n) => {
                    n.as_u64().map_or_else(|| CountValue::Text(n.to_string()), CountValue::Number)
                }
                Value::String(s) => CountValue::Text(s.clone()),
                other => CountValue::Text(other.to_string()),
            };
            Count { name: key.clone(), value }
        })
        .collect()
}

/// The leaderboard's CSV queue.
pub struct Client {
    base: String,
    key: String,
    agent: ureq::Agent,
}

impl Client {
    /// A client for the site at `base`.
    #[must_use]
    pub fn new(base: &str, key: String) -> Self {
        Self { base: base.trim_end_matches('/').to_owned(), key, agent: agent() }
    }

    /// Hand over `body` as `filename` and return the job it was queued as.
    ///
    /// # Errors
    /// [`SiteError`] when the site did not queue it.
    pub fn submit(&self, body: &[u8], filename: &str) -> Result<u64, SiteError> {
        let url = format!("{}/api/v2/upload-csv", self.base);
        let part = Part::bytes(body).file_name(filename).mime_str(GZIP).expect("a valid MIME type");
        let form = Form::new().part("file", part);
        let response = self
            .agent
            .post(&url)
            .header("X-API-Key", &self.key)
            .send(form)
            .map_err(|error| SiteError::Unsent { url, error: error.to_string() })?;
        parse_submit(&answer(response)?)
    }

    /// Ask where `job` stands. A redirect to the login page, or a refused key, is
    /// [`JobStatus::Unreadable`]: the job exists, and the site will not tell an API key
    /// about it.
    ///
    /// # Errors
    /// [`SiteError`] when the poll had no answer.
    pub fn poll(&self, job: u64) -> Result<JobStatus, SiteError> {
        let url = self.job_url(job);
        let response = self
            .agent
            .get(&url)
            .header("X-API-Key", &self.key)
            .config()
            .timeout_global(Some(POLL_TIMEOUT))
            .build()
            .call()
            .map_err(|error| SiteError::Unreached { url: url.clone(), error: error.to_string() })?;
        match answer(response) {
            Ok(text) => parse_job(&text),
            Err(
                error @ (SiteError::LoginRequired { .. } | SiteError::CredentialsRejected { .. }),
            ) => Ok(JobStatus::Unreadable(error)),
            Err(error) => Err(error),
        }
    }

    /// Where `job`'s status is.
    #[must_use]
    pub fn job_url(&self, job: u64) -> String {
        format!("{}/api/v2/upload-job/{job}", self.base)
    }
}

/// A response's body, or why its status means it has none worth reading. The status decides
/// first: a refusal's body is read only to quote, so one that cannot be read or is not UTF-8
/// still gives the status's error. A redirect's body is not read: it is the page being
/// redirected from, not an answer.
fn answer(mut response: ureq::http::Response<ureq::Body>) -> Result<String, SiteError> {
    let status = response.status();
    if status.is_redirection() {
        let location = location(&response);
        if let Some(login) = location.as_ref().filter(|at| at.contains("/login")) {
            return Err(SiteError::LoginRequired { location: login.clone() });
        }
        return Err(SiteError::Redirected { location });
    }
    let text = match body(&mut response) {
        Body::Success(text) => return text,
        Body::Refusal(text) => text,
    };
    Err(match status.as_u16() {
        401 | 403 => SiteError::CredentialsRejected { reason: site_reason(&text) },
        413 => SiteError::TooLarge,
        // The body as it came, when there is one.
        code => {
            SiteError::Status { code, said: Some(quote(&text)).filter(|said| !said.is_empty()) }
        }
    })
}

/// The job a submit was queued as.
fn parse_submit(text: &str) -> Result<u64, SiteError> {
    let object = object(text)?;
    refused(&object, text)?;
    object
        .get("job_id")
        .and_then(Value::as_u64)
        .ok_or_else(|| SiteError::Unnamed { quoted: quote(text) })
}

/// Where a job stands, from a poll's answer.
fn parse_job(text: &str) -> Result<JobStatus, SiteError> {
    let object = object(text)?;
    refused(&object, text)?;
    match object.get("status").and_then(Value::as_str) {
        Some("queued") => Ok(JobStatus::Queued),
        Some("processing") => Ok(JobStatus::Processing),
        Some("done") => {
            let result = match object.get("result") {
                Some(Value::Object(result)) => counts(result),
                _ => Vec::new(),
            };
            Ok(JobStatus::Done(result))
        }
        Some("failed") => {
            let result = object.get("result").and_then(Value::as_object);
            let message =
                reason(&object).or_else(|| result.and_then(reason)).unwrap_or_else(|| quote(text));
            Ok(JobStatus::Failed(message))
        }
        _ => Err(SiteError::UnknownState { quoted: quote(text) }),
    }
}

/// Refuse an answer that says `"ok": false`, with its reason.
fn refused(object: &Map<String, Value>, text: &str) -> Result<(), SiteError> {
    if object.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(SiteError::Refused { reason: reason(object).unwrap_or_else(|| quote(text)) });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        Client, Count, CountValue, JobState, JobStatus, Last, Unfollowed, Waited, give_up,
        next_delay, parse_job, parse_submit, wait,
    };
    use crate::upload::testing::{serve_once, serve_once_with};
    use crate::upload::{Progress, SiteError};

    fn count(name: &str, value: u64) -> Count {
        Count { name: name.to_owned(), value: CountValue::Number(value) }
    }

    /// A poll's transport failure, saying `error`.
    fn unreached(error: &str) -> SiteError {
        SiteError::Unreached { url: "http://127.0.0.1:9".to_owned(), error: error.to_owned() }
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
                "result":{"updated":3,"imported":1200,"captured":17,"skipped":4,
                "note":"ok","ratio":0.5,"flag":true}}"#,
        )
        .unwrap();
        let text = |name: &str, value: &str| Count {
            name: name.to_owned(),
            value: CountValue::Text(value.to_owned()),
        };
        assert_eq!(
            status,
            JobStatus::Done(vec![
                count("imported", 1200),
                count("captured", 17),
                count("updated", 3),
                text("flag", "true"),
                text("note", "ok"),
                text("ratio", "0.5"),
                count("skipped", 4),
            ])
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
    fn upload_parser_reports_unknown_state_when_status_is_new() {
        let raw = r#"{"ok":true,"job_id":42,"status":"paused"}"#;
        assert_eq!(parse_job(raw), Err(SiteError::UnknownState { quoted: raw.to_owned() }));
    }

    #[test]
    fn upload_parser_reports_refusal_when_answer_says_not_ok() {
        let answer = parse_submit(r#"{"ok":false,"error":"quota"}"#);
        assert_eq!(answer, Err(SiteError::Refused { reason: "quota".to_owned() }));
        let answer = parse_submit(r#"{"ok":true}"#);
        assert_eq!(answer, Err(SiteError::Unnamed { quoted: r#"{"ok":true}"#.to_owned() }));
        let answer = parse_submit("<html>");
        assert_eq!(answer, Err(SiteError::NotJson { quoted: "<html>".to_owned() }));
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
        assert!(request.contains("application/gzip"), "{request}");
        assert!(request.contains("gzipped bytes"), "{request}");
    }

    #[test]
    fn upload_client_reports_key_rejected_when_server_returns_401() {
        let (base, server) = serve_once("401 Unauthorized", r#"{"ok":false,"error":"bad key"}"#);
        let client = Client::new(&base, "wrong".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::CredentialsRejected { reason: Some("bad key".to_owned()) });
        server.join().unwrap();
    }

    #[test]
    fn upload_client_quotes_answer_when_server_returns_unexpected_status() {
        let (base, server) = serve_once("500 Internal Server Error", "it broke");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.poll(42).unwrap_err();
        assert_eq!(error, SiteError::Status { code: 500, said: Some("it broke".to_owned()) });
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
        assert_eq!(
            status,
            JobStatus::Unreadable(SiteError::LoginRequired { location: location.to_owned() })
        );
        // `serve_once` panics, failing the join, if the redirect was followed.
        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("GET /api/v2/upload-job/42 "), "{request}");
    }

    #[test]
    fn upload_client_reports_redirect_when_submit_redirects() {
        let (base, server) = serve_once_with("302 Found", "Location: /elsewhere\r\n", b"");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Redirected { location: Some("/elsewhere".to_owned()) });
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_redirect_to_nowhere_when_location_missing() {
        let (base, server) = serve_once_with("302 Found", "", b"");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Redirected { location: None });
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_bare_status_when_error_body_is_empty() {
        let (base, server) = serve_once("502 Bad Gateway", "  ");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.poll(42).unwrap_err();
        assert_eq!(error, SiteError::Status { code: 502, said: None });
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_login_required_when_submit_redirects_to_login() {
        // Unlike a poll, a submit sent to the login page uploaded nothing.
        let (base, server) = serve_once_with("302 Found", "Location: /login/\r\n", b"");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::LoginRequired { location: "/login/".to_owned() });
        server.join().unwrap();
    }

    #[test]
    fn upload_client_includes_site_reason_when_submit_returns_401() {
        let (base, server) =
            serve_once("401 Unauthorized", r#"{"ok":false,"error":"key revoked"}"#);
        let client = Client::new(&base, "wrong".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(
            error,
            SiteError::CredentialsRejected { reason: Some("key revoked".to_owned()) }
        );
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
            JobStatus::Unreadable(SiteError::CredentialsRejected {
                reason: Some("no scope".to_owned())
            })
        );
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_status_when_error_body_is_not_utf8() {
        let (base, server) = serve_once_with("413 Payload Too Large", "", b"\xff\xfe too big");
        let client = Client::new(&base, "secret".to_owned());
        let error = client.submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::TooLarge);
        server.join().unwrap();
    }

    #[test]
    fn upload_client_reports_unsent_when_nothing_listens() {
        // Bound then dropped, so the port refuses the connection.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let error = Client::new(&base, "secret".to_owned()).submit(b"bytes", "x.csv.gz");
        let Err(SiteError::Unsent { url, .. }) = error else { panic!("{error:?}") };
        assert_eq!(url, format!("{base}/api/v2/upload-csv"));
    }

    /// `wait` on a fake clock that advances only by what it sleeps, answering each poll
    /// from `answers` in turn, with what it said.
    fn wait_on(
        give_up: Duration,
        answers: Vec<Result<JobStatus, SiteError>>,
    ) -> (Waited, Vec<Progress>) {
        let clock = std::cell::Cell::new(Duration::ZERO);
        let mut answers = answers.into_iter();
        let mut said = Vec::new();
        let waited = wait(
            42,
            give_up,
            || answers.next().unwrap_or(Ok(JobStatus::Processing)),
            |delay| clock.set(clock.get() + delay),
            || clock.get(),
            |progress| said.push(progress),
        );
        (waited, said)
    }

    #[test]
    fn upload_wait_returns_done_when_transient_error_precedes_done() {
        let bad_gateway = SiteError::Status { code: 502, said: Some("bad gateway".to_owned()) };
        let (waited, said) = wait_on(
            Duration::from_secs(900),
            vec![
                Ok(JobStatus::Queued),
                Err(bad_gateway.clone()),
                Err(bad_gateway.clone()),
                Ok(JobStatus::Done(Vec::new())),
            ],
        );
        assert_eq!(waited, Waited::Done(Vec::new()));
        // Every poll that did not end the wait, repeats included.
        let failed = Progress::PollFailed { job: 42, error: bad_gateway };
        assert_eq!(
            said,
            [Progress::State { job: 42, state: JobState::Queued }, failed.clone(), failed]
        );
    }

    #[test]
    fn upload_wait_returns_unfollowed_when_give_up_reached() {
        let (waited, _) = wait_on(give_up(0), vec![Ok(JobStatus::Queued)]);
        assert_eq!(
            waited,
            Waited::Unfollowed(Unfollowed::TimedOut {
                secs: 30,
                last: Some(Last::State(JobState::Processing))
            })
        );
    }

    #[test]
    fn upload_wait_returns_unfollowed_when_poll_unreadable() {
        let error = SiteError::CredentialsRejected { reason: None };
        let (waited, _) =
            wait_on(Duration::from_secs(60), vec![Ok(JobStatus::Unreadable(error.clone()))]);
        assert_eq!(waited, Waited::Unfollowed(Unfollowed::Unreadable(error)));
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
        let errors = (0..100).map(|n| Err(unreached(&format!("timed out {n}")))).collect();
        let (waited, _) = wait_on(give_up(9_615), errors);
        // 49.2 s, rounded; and the error the wait ended on, not the first.
        let Waited::Unfollowed(Unfollowed::TimedOut { secs: 49, last: Some(Last::PollFailed(e)) }) =
            waited
        else {
            panic!("{waited:?}")
        };
        let SiteError::Unreached { error, .. } = e else { panic!("{e:?}") };
        assert!(error.starts_with("timed out "), "{error}");
        assert_ne!(error, "timed out 0");
    }

    #[test]
    fn upload_wait_names_state_when_errors_follow_a_state() {
        let mut answers = vec![Ok(JobStatus::Queued)];
        answers.extend((0..6).map(|_| Err(unreached("timed out"))));
        let (waited, _) = wait_on(give_up(0), answers);
        assert_eq!(
            waited,
            Waited::Unfollowed(Unfollowed::TimedOut {
                secs: 30,
                last: Some(Last::State(JobState::Queued))
            })
        );
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
            assert!(matches!(waited, Waited::Unfollowed(_)), "{waited:?}");
            assert!(slept.get() <= give_up, "{secs}s: slept {:?}", slept.get());
        }
    }

    #[test]
    fn upload_wait_returns_unfollowed_when_only_errors_until_give_up() {
        let errors = (0..100).map(|_| Err(unreached("timed out"))).collect();
        let (waited, _) = wait_on(Duration::from_secs(60), errors);
        // Repeats logged once still name the error.
        assert_eq!(
            waited,
            Waited::Unfollowed(Unfollowed::TimedOut {
                secs: 60,
                last: Some(Last::PollFailed(unreached("timed out")))
            })
        );
    }

    #[test]
    fn upload_wait_fails_when_job_failed() {
        let (waited, _) =
            wait_on(Duration::from_secs(60), vec![Ok(JobStatus::Failed("bad".to_owned()))]);
        assert_eq!(waited, Waited::Failed("bad".to_owned()));
    }
}
