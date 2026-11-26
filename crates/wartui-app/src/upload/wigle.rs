//! Uploading to WiGLE.
//!
//! It uses the v3 upload: one multipart POST to `/api/v3/upload` hands over the file, and the
//! answer names the transaction it was queued as. The file is the same gzipped CSV WDGWars
//! takes, under the same name.
//!
//! WiGLE authenticates an upload with HTTP Basic: the account's API Name is the username and
//! its API Token the password. `ureq` has no helper for it, so the header is built here.
//!
//! Nothing is polled. The upload's answer is the last word wartui reads: the API names no
//! states for a transaction's import, so there is nothing to tell the operator but where to
//! look. The upload is recorded `queued` as soon as WiGLE names the transaction.
//!
//! WiGLE places a network from every observation it is sent, rather than scoring one per
//! cooldown as WDGWars does, so its rows fold over a far narrower window: [`RECAPTURE_SECS`].
//! WiGLE's size limit is on the file sent, so it counts the gzipped body, not the CSV inside it.
//!
//! Statuses are mapped here and redirects are not followed, as for WDGWars, so a refusal is
//! reported in WiGLE's own terms and an HTML page is never read as the API's answer.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::Value;
use ureq::unversioned::multipart::{Form, Part};

use super::http::{
    Body, GZIP, SiteError, agent, body, location, object, quote, reason, site_reason,
};

/// WiGLE's API.
pub const BASE: &str = "https://api.wigle.net";

/// The largest file WiGLE takes, counted on the file as sent: the gzipped body.
pub const MAX_FILE: u64 = 180 * 1024 * 1024;

/// How long a network's sightings fold into one row in an upload to WiGLE, in seconds: one row
/// per network per ~70 m at 50 km/h.
pub const RECAPTURE_SECS: u64 = 5;

/// An upload WiGLE queued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Queued {
    /// WiGLE's name for it.
    pub trans_id: String,
    /// What WiGLE warned of, when it warned.
    pub warning: Option<String>,
}

/// WiGLE's upload endpoint.
pub struct Client {
    base: String,
    /// `Basic` and the name and token, encoded.
    authorization: String,
    agent: ureq::Agent,
}

impl Client {
    /// A client for WiGLE at `base`, signing in as `name` with `token`.
    #[must_use]
    pub fn new(base: &str, name: &str, token: &str) -> Self {
        let authorization = format!("Basic {}", STANDARD.encode(format!("{name}:{token}")));
        Self { base: base.trim_end_matches('/').to_owned(), authorization, agent: agent() }
    }

    /// Hand over `body` as `filename` and return the transaction it was queued as.
    ///
    /// # Errors
    /// [`SiteError`] when WiGLE did not queue it.
    pub fn submit(&self, body: &[u8], filename: &str) -> Result<Queued, SiteError> {
        let url = format!("{}/api/v3/upload", self.base);
        let part = Part::bytes(body).file_name(filename).mime_str(GZIP).expect("a valid MIME type");
        let form = Form::new().part("file", part);
        let response = self
            .agent
            .post(&url)
            .header("Authorization", &self.authorization)
            .send(form)
            .map_err(|error| SiteError::Unsent { url, error: error.to_string() })?;
        answer(response)
    }
}

/// The transaction a response names, or why it names none. A refusal's body is read only to
/// quote, so one that cannot be read still gives the status's error. A redirect's body is not
/// read.
fn answer(mut response: ureq::http::Response<ureq::Body>) -> Result<Queued, SiteError> {
    let status = response.status();
    if status.is_redirection() {
        return Err(SiteError::Redirected { location: location(&response) });
    }
    let text = match body(&mut response) {
        Body::Success(text) => return parse_upload(&text?),
        Body::Refusal(text) => text,
    };
    let reason = site_reason(&text);
    Err(match status.as_u16() {
        401 => SiteError::CredentialsRejected { reason },
        429 => SiteError::TooManyUploads { reason },
        code => SiteError::Status { code, said: reason },
    })
}

/// The transaction an accepted upload's answer names.
fn parse_upload(text: &str) -> Result<Queued, SiteError> {
    let object = object(text)?;
    if object.get("success").and_then(Value::as_bool) != Some(true) {
        return Err(SiteError::Refused { reason: reason(&object).unwrap_or_else(|| quote(text)) });
    }
    let trans_id = object
        .get("results")
        .and_then(|results| results.get("transids"))
        .and_then(|ids| ids.get(0))
        .and_then(|id| id.get("transId"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| SiteError::Unnamed { quoted: quote(text) })?;
    let warning = object
        .get("warning")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|warning| !warning.is_empty())
        .map(str::to_owned);
    Ok(Queued { trans_id: trans_id.to_owned(), warning })
}

#[cfg(test)]
mod tests {
    use super::{Client, Queued};
    use crate::upload::SiteError;
    use crate::upload::testing::{serve_once, serve_once_with};

    /// An answer accepting the upload as `20261122-00017`, warning `warning`.
    fn accepted(warning: &str) -> String {
        format!(
            r#"{{"success":true,"warning":{warning},"results":{{"timeTaken":"1 s","filesize":655,
                "filename":"wartui.csv.gz","transids":[{{"file":"wartui.csv.gz","size":655,
                "transId":"20261122-00017"}}]}},"observer":"example"}}"#
        )
    }

    fn client(base: &str) -> Client {
        Client::new(base, "AIDexamplename", "exampletoken789")
    }

    #[test]
    fn wigle_client_sends_basic_auth_and_multipart_file_when_submitting() {
        let (base, server) = serve_once("200 OK", &accepted("null"));
        client(&base).submit(b"gzipped bytes", "wartui-2026-09-30-19-02-11.csv.gz").unwrap();

        let request = String::from_utf8_lossy(&server.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /api/v3/upload "), "{request}");
        // The base64 of `AIDexamplename:exampletoken789`. Header names are case-insensitive.
        let auth = request.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization").then(|| value.trim())
        });
        assert_eq!(auth, Some("Basic QUlEZXhhbXBsZW5hbWU6ZXhhbXBsZXRva2VuNzg5"), "{request}");
        assert!(request.contains("name=\"file\""), "{request}");
        assert!(request.contains("filename=\"wartui-2026-09-30-19-02-11.csv.gz\""), "{request}");
        assert!(request.contains("gzipped bytes"), "{request}");
        assert!(!request.to_ascii_lowercase().contains("x-api-key"), "{request}");
    }

    #[test]
    fn wigle_client_returns_transaction_when_upload_accepted() {
        let (base, server) = serve_once("200 OK", &accepted(r#""  ""#));
        let queued = client(&base).submit(b"bytes", "x.csv.gz").unwrap();
        // An empty warning is no warning.
        assert_eq!(queued, Queued { trans_id: "20261122-00017".to_owned(), warning: None });
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_returns_warning_when_upload_accepted_with_one() {
        let (base, server) = serve_once("200 OK", &accepted(r#""duplicate file""#));
        let queued = client(&base).submit(b"bytes", "x.csv.gz").unwrap();
        assert_eq!(queued.warning.as_deref(), Some("duplicate file"));
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_no_transaction_when_accepted_answer_names_none() {
        let answer = r#"{"success":true,"results":{"transids":[]}}"#;
        let (base, server) = serve_once("200 OK", answer);
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Unnamed { quoted: answer.to_owned() });
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_refusal_when_answer_says_no_success() {
        let (base, server) =
            serve_once("200 OK", r#"{"success":false,"message":"file too small"}"#);
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Refused { reason: "file too small".to_owned() });
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_not_text_when_accepted_answer_is_not_utf8() {
        let (base, server) = serve_once_with("200 OK", "", b"\xff\xfe");
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::NotText);
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_credentials_rejected_when_server_returns_401() {
        let (base, server) = serve_once("401 Unauthorized", "");
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::CredentialsRejected { reason: None });
        server.join().unwrap();

        let (base, server) =
            serve_once("401 Unauthorized", r#"{"success":false,"message":"bad token"}"#);
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::CredentialsRejected { reason: Some("bad token".to_owned()) });
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_daily_limit_when_server_returns_429() {
        let (base, server) = serve_once(
            "429 Too Many Requests",
            r#"{"success":false,"message":"Too many uploads today"}"#,
        );
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(
            error,
            SiteError::TooManyUploads { reason: Some("Too many uploads today".to_owned()) }
        );
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_reports_redirect_when_submit_redirects() {
        let (base, server) = serve_once_with("302 Found", "Location: /elsewhere\r\n", b"");
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Redirected { location: Some("/elsewhere".to_owned()) });
        // `serve_once` panics, failing the join, if the redirect was followed.
        server.join().unwrap();
    }

    #[test]
    fn wigle_client_quotes_answer_when_server_returns_unexpected_status() {
        let (base, server) =
            serve_once("400 Bad Request", r#"{"success":false,"message":"filename required"}"#);
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(
            error,
            SiteError::Status { code: 400, said: Some("filename required".to_owned()) }
        );
        server.join().unwrap();

        let (base, server) = serve_once("500 Internal Server Error", "it broke");
        let error = client(&base).submit(b"bytes", "x.csv.gz").unwrap_err();
        assert_eq!(error, SiteError::Status { code: 500, said: Some("it broke".to_owned()) });
        server.join().unwrap();
    }
}
