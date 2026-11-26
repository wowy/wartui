//! What `wartui upload` and the fleet view's `u` say of an upload, from the typed values
//! `wartui_app::upload` hands back. Both front ends word a site, a refusal or a site's answer
//! alike, so the operator reads one account of an upload whichever started it.

use std::path::Path;

use chrono::DateTime;
use wartui_app::export::NodeNames;
use wartui_app::upload::wdgwars::{Count, CountValue, JobState, Last, Unfollowed};
use wartui_app::upload::{Missing, Prepare, Prepared, Refusal, SendError, SiteError, SiteRefusal};
use wartui_core::store::{StoreError, UploadSite, open_readonly};

use crate::export::details;
use crate::text::thousands;

/// What the bin crate says of a site.
pub(crate) trait Site {
    /// The site's name, as the operator reads it.
    fn label(self) -> &'static str;
    /// What the site calls an upload it queued.
    fn job(self) -> &'static str;
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
}

/// The site's label, possessive.
fn possessive(site: UploadSite) -> &'static str {
    match site {
        UploadSite::Wdgwars => "WDGWars'",
        UploadSite::Wigle => "WiGLE's",
    }
}

/// What the summaries of `outcomes` call the nodes of the capture at `db`. Only a ready site's
/// summary names nodes, so with none the capture is not opened: it may not exist. A capture that
/// cannot be opened names every node by its address tail, as one whose slots cannot be read does.
pub(crate) fn names(db: &Path, outcomes: &[(UploadSite, Prepare)]) -> NodeNames {
    if !outcomes.iter().any(|(_, outcome)| matches!(outcome, Prepare::Ready(_))) {
        return NodeNames::by_address();
    }
    match open_readonly(db) {
        Ok(conn) => NodeNames::of(&conn),
        Err(error) => {
            tracing::warn!(%error, "cannot open the capture for its node slots; naming nodes by address");
            NodeNames::by_address()
        }
    }
}

/// What the operator is asked to send: the row count, the summary's details, the CSV's size, its
/// compressed size and the upload this one follows on from, a line each.
pub(crate) fn describe(prepared: &Prepared, names: &NodeNames) -> String {
    let mut text = rows_line(prepared);
    text.push_str(&details(prepared.summary(), names));
    text.push_str(&sizes(prepared));
    text
}

/// [`describe`] without the summary's details, for a confirm that shows two sites.
pub(crate) fn brief(prepared: &Prepared) -> String {
    let mut text = rows_line(prepared);
    text.push_str(&sizes(prepared));
    text
}

fn rows_line(prepared: &Prepared) -> String {
    format!("{} rows to upload\n", thousands(prepared.summary().rows))
}

fn sizes(prepared: &Prepared) -> String {
    let mut text = format!("  {:<11}{}\n", "csv", size(prepared.csv_bytes()));
    text.push_str(&format!("  {:<11}{}\n", "compressed", size(prepared.file_bytes())));
    if let Some(previous) = prepared.previous() {
        text.push_str(&format!(
            "  {:<11}{} {}, {}; sending what came after\n",
            "previously",
            prepared.site().job(),
            previous.job,
            utc_minute(previous.uploaded_at_ms)
        ));
    }
    text
}

/// `bytes` in decimal units, the ones WDGWars' limit is given in.
pub(crate) fn size(bytes: u64) -> String {
    const MB: u64 = 1_000_000;
    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else {
        format!("{} KB", bytes.div_ceil(1_000))
    }
}

/// Unix milliseconds in UTC to the minute, as the summary's span line gives times.
pub(crate) fn utc_minute(ms: i64) -> String {
    let at = DateTime::from_timestamp_millis(ms).unwrap_or_default();
    at.format("%Y-%m-%d %H:%M UTC").to_string()
}

/// Where the settings live, as a refusal names them: `config`, or the default file's name.
fn config_file(config: Option<&Path>) -> String {
    config.map_or_else(|| "wartui.toml".to_owned(), |path| path.display().to_string())
}

/// Why the whole upload of `db` is refused. `config` is where the settings live.
pub(crate) fn refusal_text(refusal: &Refusal, db: &Path, config: Option<&Path>) -> String {
    let db = db.display();
    match refusal {
        Refusal::NoCredentials => format!(
            "no upload credentials; paste a WDGWars API key or a WiGLE API name and token in the \
             settings modal (`c`), or set `wdgwars`, or `wigle-name` and `wigle-token`, under \
             `[api-keys]` in {}",
            config_file(config)
        ),
        Refusal::TestData => {
            format!("{db} holds test data (made with --sim or --lat/--lon), so it is not uploaded")
        }
        Refusal::Unopened(error) => format!("opening {db}: {error}"),
        Refusal::Unread(error) => format!("reading {db}: {error}"),
        Refusal::Export(error) => error.to_string(),
        Refusal::Compress(error) => format!("compressing the upload: {error}"),
    }
}

/// Why `site` alone cannot be sent to. `config` is where the settings live.
pub(crate) fn site_refusal_text(
    site: UploadSite,
    refusal: &SiteRefusal,
    config: Option<&Path>,
) -> String {
    let file = config_file(config);
    match refusal {
        SiteRefusal::Missing(Missing::WdgwarsKey) => format!(
            "no WDGWars API key; paste one in the settings modal (`c`) or set `wdgwars` under \
             `[api-keys]` in {file}"
        ),
        SiteRefusal::Missing(Missing::WigleNameAndToken) => format!(
            "no WiGLE API name and token; paste them in the settings modal (`c`) or set \
             `wigle-name` and `wigle-token` under `[api-keys]` in {file}"
        ),
        SiteRefusal::Missing(missing @ (Missing::WigleName | Missing::WigleToken)) => {
            let (what, key) = match missing {
                Missing::WigleName => ("name", "wigle-name"),
                _ => ("token", "wigle-token"),
            };
            format!(
                "WiGLE needs both an API name and an API token; paste the {what} in the settings \
                 modal (`c`) or set `{key}` under `[api-keys]` in {file}"
            )
        }
        SiteRefusal::TooLarge { bytes, limit } => match site {
            UploadSite::Wdgwars => format!(
                "the upload's CSV is {} before compression, over WDGWars' limit of {}",
                size(*bytes),
                size(*limit)
            ),
            UploadSite::Wigle => format!(
                "the upload is {} compressed, over WiGLE's limit of {} (180 MiB)",
                size(*bytes),
                size(*limit)
            ),
        },
    }
}

/// `head`, and `reason` after it when there is one.
fn with_reason(head: String, reason: Option<&String>) -> String {
    match reason {
        Some(reason) => format!("{head}: {reason}"),
        None => head,
    }
}

/// What `site`'s API did instead of what was asked.
pub(crate) fn site_error_text(site: UploadSite, error: &SiteError) -> String {
    let label = site.label();
    match error {
        SiteError::Unsent { url, error } => format!("sending the upload to {url}: {error}"),
        SiteError::Unreached { url, error } => format!("reaching {url}: {error}"),
        SiteError::LoginRequired { location } => format!(
            "{label} redirected to its login page ({location}): the endpoint wants a logged-in \
             session rather than the API key"
        ),
        SiteError::Redirected { location } => {
            format!("{label} redirected to {}", location.as_deref().unwrap_or("nowhere"))
        }
        SiteError::CredentialsRejected { reason } => {
            let head = match site {
                UploadSite::Wdgwars => "WDGWars rejected the API key",
                UploadSite::Wigle => "WiGLE rejected the API name and token",
            };
            with_reason(head.to_owned(), reason.as_ref())
        }
        SiteError::TooLarge => format!("{label} refused the upload as too large"),
        SiteError::TooManyUploads { reason } => {
            with_reason(format!("{label} has taken too many uploads today"), reason.as_ref())
        }
        SiteError::Status { code, said } => {
            with_reason(format!("{label} answered {code}"), said.as_ref())
        }
        SiteError::Unread { error } => format!("reading {} answer: {error}", possessive(site)),
        SiteError::NotText => format!("{} answer is not UTF-8", possessive(site)),
        SiteError::NotJson { quoted } => {
            format!("{label} answered with something other than a JSON object: {quoted}")
        }
        SiteError::Refused { reason } => format!("{label} refused: {reason}"),
        SiteError::Unnamed { quoted } => {
            format!("{label} accepted the upload but named no {}: {quoted}", site.job())
        }
        SiteError::UnknownState { quoted } => {
            format!("{label} sent a job status this build does not know: {quoted}")
        }
    }
}

/// Why the upload of `db` to `site` did not end recorded.
pub(crate) fn send_error_text(site: UploadSite, error: &SendError, db: &Path) -> String {
    let db = db.display();
    match error {
        SendError::NoConsent => "nothing sent: the confirmation was for another upload".to_owned(),
        SendError::Capture(error) => format!("opening {db}: {error}"),
        SendError::Site(error) => site_error_text(site, error),
        SendError::Unrecorded { job, error } => {
            format!("recording {} {job} in {db}: {error}", site.job())
        }
    }
}

/// What a WDGWars job in `state` is, as the operator reads it.
pub(crate) fn state_name(state: JobState) -> &'static str {
    match state {
        JobState::Queued => "queued",
        JobState::Processing => "processing",
    }
}

/// Why wartui stopped following a WDGWars job.
pub(crate) fn unfollowed_text(unfollowed: &Unfollowed) -> String {
    match unfollowed {
        Unfollowed::Unreadable(error) => site_error_text(UploadSite::Wdgwars, error),
        Unfollowed::TimedOut { secs, last } => {
            let why = match last {
                Some(Last::State(state)) => format!("; it is still {}", state_name(*state)),
                Some(Last::PollFailed(error)) => format!(
                    "; the last poll failed: {}",
                    site_error_text(UploadSite::Wdgwars, error)
                ),
                None => String::new(),
            };
            format!("no answer within {secs} seconds{why}")
        }
    }
}

/// WDGWars reports `job`'s import failed with `message`.
pub(crate) fn import_failed_text(job: u64, message: &str) -> String {
    format!("WDGWars could not import job {job}: {message}")
}

/// WiGLE queued `trans_id`, and its `queued` result could not be written.
pub(crate) fn queued_unrecorded_text(trans_id: &str, error: &StoreError) -> String {
    format!("recording transaction {trans_id} as queued: {error}")
}

/// How WDGWars' `job` ended could not be written.
pub(crate) fn end_unrecorded_text(job: u64, error: &StoreError) -> String {
    format!("recording how job {job} ended: {error}")
}

/// A count's value as the operator reads it: a number with thousands separators, or the site's
/// text.
pub(crate) fn count_value(value: &CountValue) -> String {
    match value {
        CountValue::Number(n) => thousands(*n),
        CountValue::Text(text) => text.clone(),
    }
}

/// A finished job's counts, a line each.
pub(crate) fn outcome(counts: &[Count]) -> String {
    let mut text = String::new();
    for count in counts {
        text.push_str(&format!("{:<10} {}\n", count.name, count_value(&count.value)));
    }
    if text.is_empty() {
        text.push_str("done; WDGWars sent no counts\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use wartui_app::upload::testing::{EPOCH_MS, prepared};
    use wartui_app::upload::wdgwars::{Count, CountValue, JobState, Last, Unfollowed};
    use wartui_app::upload::{Missing, Refusal, SendError, SiteError, SiteRefusal, wdgwars, wigle};
    use wartui_core::export::ExportSummary;
    use wartui_core::store::{StoreError, UploadRecord, UploadSite};

    use super::{
        brief, describe, end_unrecorded_text, import_failed_text, outcome, queued_unrecorded_text,
        refusal_text, send_error_text, site_error_text, site_refusal_text, size, unfollowed_text,
    };
    use wartui_app::export::NodeNames;

    const W: UploadSite = UploadSite::Wdgwars;
    const G: UploadSite = UploadSite::Wigle;

    /// A prepared upload to `site` of two rows, following on from `previous`.
    fn ready(site: UploadSite, previous: Option<UploadRecord>) -> wartui_app::upload::Prepared {
        let summary = ExportSummary { rows: 2, ..Default::default() };
        prepared(site, vec![0; 412_345], 3_343_178, summary, previous)
    }

    fn tails() -> NodeNames {
        NodeNames::new(wartui_proto::bus::NodeBoard::NoCarrier, &std::collections::BTreeMap::new())
    }

    fn rejected(reason: &str) -> SiteError {
        SiteError::CredentialsRejected { reason: Some(reason.to_owned()) }
    }

    #[test]
    fn upload_describe_shows_csv_and_compressed_sizes_when_prepared() {
        // The limit counts the CSV, so the operator sees that size beside the one sent.
        let text = describe(&ready(W, None), &tails());
        assert!(text.contains("  csv        3.3 MB\n"), "{text}");
        assert!(text.contains("  compressed 413 KB\n"), "{text}");
    }

    #[test]
    fn upload_brief_drops_details_when_prepared() {
        let previous =
            UploadRecord { job: "20261122-00017".to_owned(), uploaded_at_ms: EPOCH_MS, rows: 1 };
        assert_eq!(
            brief(&ready(G, Some(previous))),
            "2 rows to upload\n  csv        3.3 MB\n  compressed 413 KB\n  previously \
             transaction 20261122-00017, 2026-05-01 13:34 UTC; sending what came after\n"
        );
    }

    #[test]
    fn upload_size_formats_decimal_units_when_bytes_large() {
        assert_eq!(size(40_000_000), "40.0 MB");
        assert_eq!(size(1_500_000), "1.5 MB");
        assert_eq!(size(12_345), "13 KB");
        assert_eq!(size(1_000), "1 KB");
    }

    #[test]
    fn upload_refusal_text_names_settings_and_file_when_no_credentials() {
        let file = Path::new("/home/op/.config/wartui/wartui.toml");
        let text = refusal_text(&Refusal::NoCredentials, Path::new("x.db"), Some(file));
        assert_eq!(
            text,
            "no upload credentials; paste a WDGWars API key or a WiGLE API name and token in the \
             settings modal (`c`), or set `wdgwars`, or `wigle-name` and `wigle-token`, under \
             `[api-keys]` in /home/op/.config/wartui/wartui.toml"
        );
        let text = refusal_text(&Refusal::NoCredentials, Path::new("x.db"), None);
        assert!(text.ends_with("under `[api-keys]` in wartui.toml"), "{text}");
    }

    #[test]
    fn upload_refusal_text_names_capture_when_test_data() {
        let text = refusal_text(&Refusal::TestData, Path::new("/tmp/sim.db"), None);
        assert_eq!(
            text,
            "/tmp/sim.db holds test data (made with --sim or --lat/--lon), so it is not uploaded"
        );
    }

    #[test]
    fn upload_refusal_text_names_capture_when_it_cannot_be_read() {
        let db = Path::new("/tmp/a.db");
        let text = refusal_text(&Refusal::Unopened(StoreError::NotACapture), db, None);
        assert_eq!(
            text,
            "opening /tmp/a.db: this file is not a wartui capture: it has no capture table"
        );
        let text = refusal_text(&Refusal::Unread(StoreError::NotACapture), db, None);
        assert!(text.starts_with("reading /tmp/a.db: this file is not"), "{text}");
        let io = std::io::Error::other("disk full");
        let text = refusal_text(&Refusal::Compress(io), db, None);
        assert_eq!(text, "compressing the upload: disk full");
        let export = wartui_core::export::ExportError::Io(std::io::Error::other("disk full"));
        let text = refusal_text(&Refusal::Export(export), db, None);
        assert_eq!(text, "could not write the export: disk full");
    }

    #[test]
    fn upload_site_refusal_text_names_missing_credentials_when_site_has_none() {
        let file = Some(Path::new("/etc/wartui.toml"));
        let text = |missing| site_refusal_text(W, &SiteRefusal::Missing(missing), file);
        assert_eq!(
            text(Missing::WdgwarsKey),
            "no WDGWars API key; paste one in the settings modal (`c`) or set `wdgwars` under \
             `[api-keys]` in /etc/wartui.toml"
        );
        assert_eq!(
            text(Missing::WigleNameAndToken),
            "no WiGLE API name and token; paste them in the settings modal (`c`) or set \
             `wigle-name` and `wigle-token` under `[api-keys]` in /etc/wartui.toml"
        );
        assert_eq!(
            text(Missing::WigleToken),
            "WiGLE needs both an API name and an API token; paste the token in the settings \
             modal (`c`) or set `wigle-token` under `[api-keys]` in /etc/wartui.toml"
        );
        assert_eq!(
            text(Missing::WigleName),
            "WiGLE needs both an API name and an API token; paste the name in the settings \
             modal (`c`) or set `wigle-name` under `[api-keys]` in /etc/wartui.toml"
        );
    }

    #[test]
    fn upload_site_refusal_text_names_limit_when_upload_too_large() {
        let over = SiteRefusal::TooLarge { bytes: wdgwars::MAX_CSV + 1, limit: wdgwars::MAX_CSV };
        assert_eq!(
            site_refusal_text(W, &over, None),
            "the upload's CSV is 40.0 MB before compression, over WDGWars' limit of 40.0 MB"
        );
        let over = SiteRefusal::TooLarge { bytes: wigle::MAX_FILE + 1, limit: wigle::MAX_FILE };
        assert_eq!(
            site_refusal_text(G, &over, None),
            "the upload is 188.7 MB compressed, over WiGLE's limit of 188.7 MB (180 MiB)"
        );
    }

    #[test]
    fn upload_site_error_text_names_url_when_transport_fails() {
        let url = "http://127.0.0.1:9/api/v3/upload".to_owned();
        let unsent = SiteError::Unsent { url: url.clone(), error: "refused".to_owned() };
        assert_eq!(
            site_error_text(G, &unsent),
            "sending the upload to http://127.0.0.1:9/api/v3/upload: refused"
        );
        let unreached = SiteError::Unreached { url, error: "timed out".to_owned() };
        assert_eq!(
            site_error_text(W, &unreached),
            "reaching http://127.0.0.1:9/api/v3/upload: timed out"
        );
    }

    #[test]
    fn upload_site_error_text_names_location_when_site_redirects() {
        let login = SiteError::LoginRequired { location: "/login/".to_owned() };
        assert_eq!(
            site_error_text(W, &login),
            "WDGWars redirected to its login page (/login/): the endpoint wants a logged-in \
             session rather than the API key"
        );
        let elsewhere = SiteError::Redirected { location: Some("/elsewhere".to_owned()) };
        assert_eq!(site_error_text(W, &elsewhere), "WDGWars redirected to /elsewhere");
        assert_eq!(site_error_text(G, &elsewhere), "WiGLE redirected to /elsewhere");
        let nowhere = SiteError::Redirected { location: None };
        assert_eq!(site_error_text(W, &nowhere), "WDGWars redirected to nowhere");
    }

    #[test]
    fn upload_site_error_text_quotes_reason_when_credentials_rejected() {
        assert_eq!(
            site_error_text(W, &rejected("key revoked")),
            "WDGWars rejected the API key: key revoked"
        );
        let bare = SiteError::CredentialsRejected { reason: None };
        assert_eq!(site_error_text(W, &bare), "WDGWars rejected the API key");
        assert_eq!(site_error_text(G, &bare), "WiGLE rejected the API name and token");
        assert_eq!(
            site_error_text(G, &rejected("bad token")),
            "WiGLE rejected the API name and token: bad token"
        );
    }

    #[test]
    fn upload_site_error_text_words_refusal_when_site_refuses_by_status() {
        assert_eq!(
            site_error_text(W, &SiteError::TooLarge),
            "WDGWars refused the upload as too large"
        );
        let limit = SiteError::TooManyUploads { reason: Some("Too many uploads today".to_owned()) };
        assert_eq!(
            site_error_text(G, &limit),
            "WiGLE has taken too many uploads today: Too many uploads today"
        );
        let status = SiteError::Status { code: 500, said: Some("it broke".to_owned()) };
        assert_eq!(site_error_text(W, &status), "WDGWars answered 500: it broke");
        assert_eq!(site_error_text(G, &status), "WiGLE answered 500: it broke");
        let bare = SiteError::Status { code: 400, said: None };
        assert_eq!(site_error_text(G, &bare), "WiGLE answered 400");
        // WDGWars' error status with an empty body.
        let empty = SiteError::Status { code: 502, said: None };
        assert_eq!(site_error_text(W, &empty), "WDGWars answered 502");
    }

    #[test]
    fn upload_site_error_text_quotes_answer_when_it_cannot_be_read() {
        let unread = SiteError::Unread { error: "connection reset".to_owned() };
        assert_eq!(site_error_text(W, &unread), "reading WDGWars' answer: connection reset");
        assert_eq!(site_error_text(G, &unread), "reading WiGLE's answer: connection reset");
        assert_eq!(site_error_text(W, &SiteError::NotText), "WDGWars' answer is not UTF-8");
        assert_eq!(site_error_text(G, &SiteError::NotText), "WiGLE's answer is not UTF-8");
        let not_json = SiteError::NotJson { quoted: "<html>".to_owned() };
        assert_eq!(
            site_error_text(W, &not_json),
            "WDGWars answered with something other than a JSON object: <html>"
        );
    }

    #[test]
    fn upload_site_error_text_quotes_answer_when_it_says_too_little() {
        let refused = SiteError::Refused { reason: "file too small".to_owned() };
        assert_eq!(site_error_text(G, &refused), "WiGLE refused: file too small");
        let unnamed = SiteError::Unnamed { quoted: "{}".to_owned() };
        assert_eq!(
            site_error_text(G, &unnamed),
            "WiGLE accepted the upload but named no transaction: {}"
        );
        assert_eq!(
            site_error_text(W, &unnamed),
            "WDGWars accepted the upload but named no job: {}"
        );
        let unknown = SiteError::UnknownState { quoted: "{}".to_owned() };
        assert_eq!(
            site_error_text(W, &unknown),
            "WDGWars sent a job status this build does not know: {}"
        );
    }

    #[test]
    fn upload_send_error_text_names_capture_when_it_cannot_be_written() {
        let db = Path::new("/tmp/a.db");
        let capture = SendError::Capture(StoreError::ReadOnly(db.to_owned()));
        assert_eq!(
            send_error_text(W, &capture, db),
            "opening /tmp/a.db: /tmp/a.db cannot be written"
        );
        let unrecorded =
            SendError::Unrecorded { job: "42".to_owned(), error: StoreError::NotACapture };
        assert!(
            send_error_text(W, &unrecorded, db).starts_with("recording job 42 in /tmp/a.db: "),
            "{}",
            send_error_text(W, &unrecorded, db)
        );
        let site = SendError::Site(rejected("bad key"));
        assert_eq!(send_error_text(W, &site, db), "WDGWars rejected the API key: bad key");
        assert!(!send_error_text(W, &SendError::NoConsent, db).is_empty());
    }

    #[test]
    fn upload_unfollowed_text_names_last_state_when_wait_timed_out() {
        let still = |state| Unfollowed::TimedOut { secs: 30, last: Some(Last::State(state)) };
        assert_eq!(
            unfollowed_text(&still(JobState::Processing)),
            "no answer within 30 seconds; it is still processing"
        );
        assert_eq!(
            unfollowed_text(&still(JobState::Queued)),
            "no answer within 30 seconds; it is still queued"
        );
        let error =
            SiteError::Unreached { url: "http://x".to_owned(), error: "timed out".to_owned() };
        let failed = Unfollowed::TimedOut { secs: 60, last: Some(Last::PollFailed(error)) };
        assert_eq!(
            unfollowed_text(&failed),
            "no answer within 60 seconds; the last poll failed: reaching http://x: timed out"
        );
        let silent = Unfollowed::TimedOut { secs: 49, last: None };
        assert_eq!(unfollowed_text(&silent), "no answer within 49 seconds");
    }

    #[test]
    fn upload_unfollowed_text_words_site_error_when_poll_unreadable() {
        let unreadable = Unfollowed::Unreadable(rejected("no scope"));
        assert_eq!(unfollowed_text(&unreadable), "WDGWars rejected the API key: no scope");
    }

    #[test]
    fn upload_outcome_lists_counts_when_job_finished() {
        let count = |name: &str, value| Count { name: name.to_owned(), value };
        let counts = [
            count("imported", CountValue::Number(1200)),
            count("captured", CountValue::Number(17)),
            count("updated", CountValue::Number(3)),
            count("note", CountValue::Text("ok".to_owned())),
        ];
        assert_eq!(
            outcome(&counts),
            "imported   1,200\ncaptured   17\nupdated    3\nnote       ok\n"
        );
        assert_eq!(outcome(&[]), "done; WDGWars sent no counts\n");
    }

    #[test]
    fn upload_text_names_job_when_import_failed_or_unrecorded() {
        assert_eq!(import_failed_text(42, "bad"), "WDGWars could not import job 42: bad");
        let error = StoreError::NotACapture;
        assert!(
            queued_unrecorded_text("20261122-00017", &error)
                .starts_with("recording transaction 20261122-00017 as queued: ")
        );
        assert!(end_unrecorded_text(8, &error).starts_with("recording how job 8 ended: "));
    }
}
