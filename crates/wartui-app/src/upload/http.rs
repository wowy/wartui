//! What both sites' clients share: the agent, the reading of an answer, and [`SiteError`], what a
//! site's API did other than what was asked.

use std::time::Duration;

use serde_json::{Map, Value};

/// How long to wait for a response's headers, and then for its body.
const RECV_TIMEOUT: Duration = Duration::from_secs(60);

/// How much of an unexpected answer to quote back.
const QUOTE_CHARS: usize = 200;

/// The MIME type of the file an upload carries.
pub(super) const GZIP: &str = "application/gzip";

/// What a site's API did other than queue the upload or answer the poll. Site-neutral: the front
/// end knows which site it asked.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SiteError {
    /// The upload's request failed in transport, with the transport's reason.
    #[error("sending to {url}: {error}")]
    Unsent { url: String, error: String },
    /// A poll's request failed in transport, with the transport's reason.
    #[error("reaching {url}: {error}")]
    Unreached { url: String, error: String },
    /// WDGWars redirected to its login page: the endpoint wants a logged-in session.
    #[error("redirected to the login page at {location}")]
    LoginRequired { location: String },
    /// The site redirected elsewhere.
    #[error("redirected to {location}")]
    Redirected { location: String },
    /// The site refused the credentials (WDGWars' 401 or 403, WiGLE's 401), with its reason.
    #[error("credentials rejected")]
    CredentialsRejected { reason: Option<String> },
    /// WDGWars' 413.
    #[error("too large")]
    TooLarge,
    /// WiGLE's 429, with its reason.
    #[error("too many uploads")]
    TooManyUploads { reason: Option<String> },
    /// Any other status, with what the site said.
    #[error("status {code}")]
    Status { code: u16, said: Option<String> },
    /// A 2xx body that could not be read, with the transport's reason.
    #[error("reading the answer: {error}")]
    Unread { error: String },
    /// A 2xx body that is not UTF-8.
    #[error("the answer is not UTF-8")]
    NotText,
    /// An answer that is not a JSON object, quoted.
    #[error("not a JSON object: {quoted}")]
    NotJson { quoted: String },
    /// The answer says `"ok": false` or `"success": false`, with its reason.
    #[error("refused: {reason}")]
    Refused { reason: String },
    /// The upload was accepted, naming no job or transaction. The answer, quoted.
    #[error("accepted, naming nothing: {quoted}")]
    Unnamed { quoted: String },
    /// A job status this build does not know. The answer, quoted.
    #[error("unknown job status: {quoted}")]
    UnknownState { quoted: String },
}

/// An agent for a site's API. Statuses are mapped by each client rather than by `ureq`, so a
/// refusal is reported in the site's own terms, and redirects reach the client rather than being
/// followed. Sending the body has no deadline: a large file over a slow tether takes as long as it
/// takes.
pub(super) fn agent() -> ureq::Agent {
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
pub(super) fn location(response: &ureq::http::Response<ureq::Body>) -> String {
    response
        .headers()
        .get(ureq::http::header::LOCATION)
        .map_or_else(|| "nowhere".to_owned(), |l| String::from_utf8_lossy(l.as_bytes()).into())
}

/// A response's body, read up to a limit, as the status asked for it: a 2xx body whole, or a
/// refusal's as text to quote, empty when it cannot be read.
pub(super) enum Body {
    Success(Result<String, SiteError>),
    Refusal(String),
}

/// `response`'s body, as [`Body`] says. Not for a redirect, whose body is the page being
/// redirected from, not an answer.
pub(super) fn body(response: &mut ureq::http::Response<ureq::Body>) -> Body {
    let status = response.status();
    let body = response.body_mut().with_config().limit(1024 * 1024).read_to_vec();
    if status.is_success() {
        let text = body
            .map_err(|error| SiteError::Unread { error: error.to_string() })
            .and_then(|bytes| String::from_utf8(bytes).map_err(|_| SiteError::NotText));
        return Body::Success(text);
    }
    Body::Refusal(body.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default())
}

/// The site's reason in a refusal's body: its `error` or `message`, or the quoted text.
pub(super) fn site_reason(text: &str) -> Option<String> {
    let reason = match serde_json::from_str(text) {
        Ok(Value::Object(object)) => reason(&object),
        _ => None,
    };
    reason.or_else(|| Some(quote(text)).filter(|text| !text.is_empty()))
}

/// At most [`QUOTE_CHARS`] of `text`, trimmed.
pub(super) fn quote(text: &str) -> String {
    let text = text.trim();
    match text.char_indices().nth(QUOTE_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// `text` as a JSON object.
pub(super) fn object(text: &str) -> Result<Map<String, Value>, SiteError> {
    match serde_json::from_str(text) {
        Ok(Value::Object(object)) => Ok(object),
        _ => Err(SiteError::NotJson { quoted: quote(text) }),
    }
}

/// The site's explanation, under `error` or `message`.
pub(super) fn reason(object: &Map<String, Value>) -> Option<String> {
    ["error", "message"].iter().find_map(|key| object.get(*key)?.as_str().map(str::to_owned))
}
