//! Sending a capture to WDGWars and WiGLE.
//!
//! Each site is its own module: [`wdgwars`] and [`wigle`] know their endpoints, their answers
//! and their limits. What both share is here.
//!
//! **What is uploaded is the capture, not a CSV file.** The store is the system of record, and
//! building the CSV in memory from it means no CSV on disk can be stale: an upload after a
//! decoder fix sends what `export` would write now, not what it wrote last week. Uploading a
//! capture that is still being written is safe for the reason exporting one is: WAL gives the
//! read a consistent snapshot.
//!
//! **Each site gets its own body.** An upload goes to every site with credentials, or to the one
//! [`Options::to`] names. A site with half its credentials (a WiGLE name and no token) is refused
//! by name rather than read as unset, since it is a mistake worth seeing, and the other site
//! still goes. Each site folds rows over its own recapture window ([`recapture_secs`]) unless
//! [`Options::recapture`] sets one for both, and each has its own size limit.
//!
//! **Nothing is sent without the operator's yes.** An upload cannot be taken back, so [`send`]
//! takes a [`Consent`], which only a front end builds, once it has shown what will go and been
//! told yes.
//!
//! **The capture records each upload per site**, so a repeat sends a site only what came after
//! its last one, and an upload to one site is no cutoff for the other. The cutoff is the last
//! sighting the upload covered, in the order the capture stored them, read in the same snapshot
//! as the CSV, so it is exactly what was considered and a capture still being written loses
//! nothing to it. The record is written once the site has queued the upload, because that is
//! when the upload has happened. The capture is opened for writing before anything is sent, so
//! one this user cannot write is refused with nothing uploaded rather than uploaded and left
//! unrecorded. An upload the site reports failed imported nothing and is no cutoff.
//! [`Options::resend`] ignores the record and sends everything, which is what a decoder fix
//! needs: an upload then sends what `export` would write now.
//!
//! **Every ready site is sent to, whatever happened to the other.** A front end finishes each
//! before starting the next, or runs them side by side; WDGWars' job is followed to its end
//! either way. [`Outcome::failed`] says whether a retry is called for: a site refused before it
//! queued the upload, or WDGWars reports its import failed. A WiGLE upload whose `queued` result
//! cannot be written is none: its row is written, and is a cutoff. The retry is safe: a site
//! that queued reads "nothing new".
//!
//! **A capture of test data is refused for every site**, whatever the options say: invented
//! networks or a fixed position would be published as real, WDGWars and WiGLE ban both, and an
//! upload cannot be taken back. The capture records that it holds test data.
//!
//! **Every function blocks.** Preparing reads the whole capture, sending takes as long as the
//! uplink, and following a WDGWars job takes up to two minutes. The front end picks the thread.
//! The steps are separate functions — [`prepare`], [`send`], [`wdgwars::wait`], [`finish`] — and
//! [`upload`] runs the last three for a front end, reporting each step through a callback.

mod http;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod wdgwars;
pub mod wigle;

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use flate2::Compression;
use flate2::write::GzEncoder;
use wartui_core::export::{
    DEFAULT_RECAPTURE_SECS, ExportError, ExportFilter, ExportSummary, wigle_csv,
};
use wartui_core::store::{
    Connection, StoreError, UploadRecord, UploadSite, is_simulated, last_upload, open_readonly,
    open_readwrite, record_upload, set_upload_result,
};

pub use http::SiteError;

use crate::settings::ApiKeys;

/// Every site, in the order an upload goes to them.
pub const SITES: [UploadSite; 2] = [UploadSite::Wdgwars, UploadSite::Wigle];

/// How long a network's sightings fold into one row for `site` when no window is given.
#[must_use]
pub fn recapture_secs(site: UploadSite) -> u64 {
    match site {
        // The leaderboard's scan cooldown.
        UploadSite::Wdgwars => DEFAULT_RECAPTURE_SECS,
        UploadSite::Wigle => wigle::RECAPTURE_SECS,
    }
}

/// The sites `keys` holds credentials for, whole or half, in [`SITES`] order.
#[must_use]
pub fn involved(keys: &ApiKeys) -> Vec<UploadSite> {
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

/// What an upload covers, beside the capture.
#[derive(Debug, Clone)]
pub struct Options {
    /// The one site to send to; every site with credentials when `None`.
    pub to: Option<UploadSite>,
    /// The recapture window for every site, in seconds; each site's own when `None`.
    pub recapture: Option<u64>,
    /// Send everything, ignoring what earlier uploads sent.
    pub resend: bool,
    /// Wi-Fi SSIDs left out.
    pub excluded_ssids: HashSet<Vec<u8>>,
    /// The front end's package version, which the CSV's header names as `export` writes it.
    pub app_version: String,
}

impl Options {
    /// Every site with credentials, each over its own window, sending what is new, leaving out
    /// no SSID.
    #[must_use]
    pub fn new(app_version: impl Into<String>) -> Self {
        Self {
            to: None,
            recapture: None,
            resend: false,
            excluded_ssids: HashSet::new(),
            app_version: app_version.into(),
        }
    }
}

/// Where each site is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub wdgwars: String,
    pub wigle: String,
}

impl Endpoints {
    /// The sites themselves.
    #[must_use]
    pub fn live() -> Self {
        Self { wdgwars: wdgwars::BASE.to_owned(), wigle: wigle::BASE.to_owned() }
    }
}

/// What a site signs in with.
enum Credentials {
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

/// What is ready to send to one site. Holds the credentials, so `Debug` is written by hand and
/// prints neither them nor the body.
pub struct Prepared {
    credentials: Credentials,
    /// The capture's WiGLE CSV, gzipped.
    pub body: Vec<u8>,
    /// The CSV's length before compression, the size WDGWars' limit counts.
    pub csv_bytes: u64,
    pub summary: ExportSummary,
    /// The upload to this site this one follows on from. `None` with [`Options::resend`], which
    /// follows on from nothing.
    pub previous: Option<UploadRecord>,
}

impl Prepared {
    /// The site it goes to.
    #[must_use]
    pub fn site(&self) -> UploadSite {
        self.credentials.site()
    }

    /// The size of the file as sent.
    #[must_use]
    pub fn file_bytes(&self) -> u64 {
        self.body.len() as u64
    }
}

impl std::fmt::Debug for Prepared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Prepared")
            .field("site", &self.site())
            .field("file_bytes", &self.file_bytes())
            .field("csv_bytes", &self.csv_bytes)
            .field("summary", &self.summary)
            .field("previous", &self.previous)
            .finish_non_exhaustive()
    }
}

/// What [`prepare`] found for one site.
#[derive(Debug)]
pub enum Prepare {
    Ready(Prepared),
    /// Every row was covered by `previous`.
    NothingNew {
        previous: UploadRecord,
        summary: ExportSummary,
    },
    /// No positioned row, and no upload before this one.
    NoRows(ExportSummary),
    /// The site cannot be sent to.
    Refused(SiteRefusal),
}

/// Why one site cannot be sent to. The other still goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SiteRefusal {
    /// Its credentials are missing, whole or half.
    #[error("credentials missing: {0:?}")]
    Missing(Missing),
    /// `bytes` over `limit`: the CSV before compression for WDGWars, the file as sent for WiGLE.
    #[error("{bytes} bytes, over the limit of {limit}")]
    TooLarge { bytes: u64, limit: u64 },
}

/// Which credentials a site lacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Missing {
    WdgwarsKey,
    WigleNameAndToken,
    WigleName,
    WigleToken,
}

/// Why the whole upload is refused before anything is built or sent.
#[derive(Debug, thiserror::Error)]
pub enum Refusal {
    /// No site has credentials.
    #[error("no upload credentials")]
    NoCredentials,
    /// The capture holds test data.
    #[error("the capture holds test data")]
    TestData,
    /// The capture cannot be opened.
    #[error("opening the capture")]
    Unopened(#[source] StoreError),
    /// The capture cannot be read.
    #[error("reading the capture")]
    Unread(#[source] StoreError),
    /// The CSV cannot be built.
    #[error("exporting the capture")]
    Export(#[source] ExportError),
    /// The gzip could not be finished.
    #[error("compressing the upload")]
    Compress(#[source] std::io::Error),
}

/// What `keys` holds for `site`, or what it lacks.
fn credentials(keys: &ApiKeys, site: UploadSite) -> Result<Credentials, Missing> {
    match site {
        UploadSite::Wdgwars => {
            let key = keys.wdgwars.trim();
            if key.is_empty() {
                return Err(Missing::WdgwarsKey);
            }
            Ok(Credentials::Wdgwars { key: key.to_owned() })
        }
        UploadSite::Wigle => {
            let (name, token) = (keys.wigle_name.trim(), keys.wigle_token.trim());
            match (name.is_empty(), token.is_empty()) {
                (false, false) => {
                    Ok(Credentials::Wigle { name: name.to_owned(), token: token.to_owned() })
                }
                (true, true) => Err(Missing::WigleNameAndToken),
                (false, true) => Err(Missing::WigleToken),
                (true, false) => Err(Missing::WigleName),
            }
        }
    }
}

/// Check the credentials, then build each site's body from what came after its last upload, or
/// from everything with [`Options::resend`]. One outcome per site, in [`SITES`] order. Contacts
/// nothing, so every refusal here costs no request. When every site named lacks credentials,
/// each is refused without opening the capture.
///
/// # Errors
/// [`Refusal`] refuses the whole upload: no site has credentials, or the capture holds test data
/// or cannot be read.
pub fn prepare(
    keys: &ApiKeys,
    db: &Path,
    options: &Options,
) -> Result<Vec<(UploadSite, Prepare)>, Refusal> {
    let sites = options.to.map_or_else(|| involved(keys), |site| vec![site]);
    if sites.is_empty() {
        return Err(Refusal::NoCredentials);
    }
    let signed: Vec<(UploadSite, Result<Credentials, Missing>)> =
        sites.into_iter().map(|site| (site, credentials(keys, site))).collect();
    if signed.iter().all(|(_, credentials)| credentials.is_err()) {
        let refused = signed.into_iter().filter_map(|(site, credentials)| {
            credentials.err().map(|missing| (site, Prepare::Refused(SiteRefusal::Missing(missing))))
        });
        return Ok(refused.collect());
    }
    let conn = open_readonly(db).map_err(Refusal::Unopened)?;
    if is_simulated(&conn).map_err(|error| Refusal::Unread(error.into()))? {
        return Err(Refusal::TestData);
    }
    signed
        .into_iter()
        .map(|(site, credentials)| {
            let outcome = match credentials {
                Ok(credentials) => prepare_site(&conn, credentials, options)?,
                Err(missing) => Prepare::Refused(SiteRefusal::Missing(missing)),
            };
            Ok((site, outcome))
        })
        .collect()
}

/// [`prepare`] for the site `credentials` signs in to.
fn prepare_site(
    conn: &Connection,
    credentials: Credentials,
    options: &Options,
) -> Result<Prepare, Refusal> {
    let site = credentials.site();
    let filter = ExportFilter {
        recapture_secs: options.recapture.unwrap_or_else(|| recapture_secs(site)),
        after_uploads: (!options.resend).then_some(site),
        excluded_ssids: options.excluded_ssids.clone(),
    };
    let Body { gz, csv_bytes, summary, previous } = body(conn, &filter, &options.app_version)?;
    if summary.rows == 0 {
        return Ok(match previous {
            Some(previous) => Prepare::NothingNew { previous, summary },
            None => Prepare::NoRows(summary),
        });
    }
    if let Err(refusal) = check_size(site, csv_bytes, gz.len() as u64) {
        return Ok(Prepare::Refused(refusal));
    }
    Ok(Prepare::Ready(Prepared { credentials, body: gz, csv_bytes, summary, previous }))
}

/// Refuse a body over `site`'s limit: `csv_bytes` before compression, `file_bytes` as sent. One
/// exactly at the limit goes.
fn check_size(site: UploadSite, csv_bytes: u64, file_bytes: u64) -> Result<(), SiteRefusal> {
    match site {
        UploadSite::Wdgwars if csv_bytes > wdgwars::MAX_CSV => {
            Err(SiteRefusal::TooLarge { bytes: csv_bytes, limit: wdgwars::MAX_CSV })
        }
        UploadSite::Wigle if file_bytes > wigle::MAX_FILE => {
            Err(SiteRefusal::TooLarge { bytes: file_bytes, limit: wigle::MAX_FILE })
        }
        UploadSite::Wdgwars | UploadSite::Wigle => Ok(()),
    }
}

/// What [`body`] built.
struct Body {
    /// The capture's WiGLE CSV, gzipped.
    gz: Vec<u8>,
    /// The CSV's length before compression, which WDGWars' limit counts.
    csv_bytes: u64,
    /// What went into it.
    summary: ExportSummary,
    /// The upload it follows on from when it is [`ExportFilter::after_uploads`].
    previous: Option<UploadRecord>,
}

/// A writer that counts what it passes on.
struct Counted<W> {
    inner: W,
    bytes: u64,
}

impl<W: Write> Write for Counted<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// The capture's WiGLE CSV, gzipped in memory, with its uncompressed length.
fn body(conn: &Connection, filter: &ExportFilter, app_version: &str) -> Result<Body, Refusal> {
    let mut csv = Counted { inner: GzEncoder::new(Vec::new(), Compression::default()), bytes: 0 };
    let summary = wigle_csv(conn, filter, &mut csv, app_version).map_err(Refusal::Export)?;
    let gz = csv.inner.finish().map_err(Refusal::Compress)?;
    let previous = match filter.after_uploads {
        Some(site) => last_upload(conn, site).map_err(|error| Refusal::Unread(error.into()))?,
        None => None,
    };
    Ok(Body { gz, csv_bytes: csv.bytes, summary, previous })
}

/// The operator's yes to sending one prepared upload. Only a front end builds one, after it has
/// shown what will go and been told yes: an upload cannot be taken back. It names the upload it
/// was given for, so it sends no other, and [`send`] consumes it, so it sends that one once.
#[derive(Debug)]
pub struct Consent {
    site: UploadSite,
    /// The last sighting the upload covers.
    through: Option<i64>,
    rows: u64,
}

impl Consent {
    /// Consent to sending `prepared`.
    #[must_use]
    pub fn to(prepared: &Prepared) -> Self {
        Self {
            site: prepared.site(),
            through: prepared.summary.last_id,
            rows: prepared.summary.rows,
        }
    }

    /// Whether this is consent to sending `prepared`.
    fn covers(&self, prepared: &Prepared) -> bool {
        self.site == prepared.site()
            && self.through == prepared.summary.last_id
            && self.rows == prepared.summary.rows
    }
}

/// Why [`send`] did not end with the site's upload recorded.
#[derive(Debug, thiserror::Error)]
pub enum SendError {
    /// The consent names another upload. Nothing was sent.
    #[error("no consent to this upload")]
    NoConsent,
    /// The capture cannot be opened for writing. Nothing was sent.
    #[error("opening the capture for writing")]
    Capture(#[source] StoreError),
    /// The site did not queue the upload.
    #[error("the site did not queue the upload")]
    Site(#[source] SiteError),
    /// The site queued it as `job` and the capture could not record it, so it is no cutoff.
    #[error("recording job {job}")]
    Unrecorded {
        job: String,
        #[source]
        error: StoreError,
    },
}

/// An upload a site queued.
pub enum Sent {
    /// WDGWars queued a job, which [`wdgwars::wait`] follows and [`finish`] records the end of.
    Wdgwars { client: wdgwars::Client, job: Job },
    /// WiGLE queued a transaction, recorded as `queued`. `unrecorded` is why that result could
    /// not be written; the row is written all the same, so it is still a cutoff.
    Wigle { queued: wigle::Queued, unrecorded: Option<StoreError> },
}

/// A WDGWars job, and the capture's record of it.
pub struct Job {
    pub id: u64,
    /// The rows the upload carried, which the wait grows with ([`wdgwars::give_up`]).
    pub rows: u64,
    record: i64,
    /// The capture, open for writing since before the upload went.
    conn: Connection,
}

/// Check the consent, open `db` for writing, hand `prepared` to its site at `endpoints`, then
/// record that the site queued it.
///
/// # Errors
/// [`SendError`] when nothing was sent, the site did not queue it, or its record was not written.
///
/// # Panics
/// When `prepared` covers no sighting, which [`prepare`] never makes ready.
pub fn send(
    prepared: &Prepared,
    consent: Consent,
    db: &Path,
    endpoints: &Endpoints,
) -> Result<Sent, SendError> {
    if !consent.covers(prepared) {
        return Err(SendError::NoConsent);
    }
    let conn = open_readwrite(db).map_err(SendError::Capture)?;
    let filename = filename(db);
    let through = prepared.summary.last_id.expect("a capture with rows has a last sighting");
    let now = unix_ms();
    let rows = prepared.summary.rows;
    match &prepared.credentials {
        Credentials::Wdgwars { key } => {
            let client = wdgwars::Client::new(&endpoints.wdgwars, key.clone());
            let id = client.submit(&prepared.body, &filename).map_err(SendError::Site)?;
            let job = id.to_string();
            let record = record_upload(&conn, UploadSite::Wdgwars, through, now, &job, rows)
                .map_err(|error| SendError::Unrecorded { job, error: error.into() })?;
            Ok(Sent::Wdgwars { client, job: Job { id, rows, record, conn } })
        }
        Credentials::Wigle { name, token } => {
            let client = wigle::Client::new(&endpoints.wigle, name, token);
            let queued = client.submit(&prepared.body, &filename).map_err(SendError::Site)?;
            let id = &queued.trans_id;
            let record = record_upload(&conn, UploadSite::Wigle, through, now, id, rows)
                .map_err(|error| SendError::Unrecorded { job: id.clone(), error: error.into() })?;
            let unrecorded = set_upload_result(&conn, record, "queued").err().map(StoreError::from);
            Ok(Sent::Wigle { queued, unrecorded })
        }
    }
}

/// Record how the wait for WDGWars' `job` ended, `elapsed` after it began, and log it. The
/// store keeps only the result, so the log is where an unfollowed job's reason survives.
///
/// # Errors
/// [`StoreError`] when the result cannot be written.
pub fn finish(job: &Job, waited: &wdgwars::Waited, elapsed: Duration) -> Result<(), StoreError> {
    let (id, rows, elapsed_s) = (job.id, job.rows, elapsed.as_secs());
    let result = match waited {
        wdgwars::Waited::Done(_) => {
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
        wdgwars::Waited::Unfollowed(reason) => {
            let result = "unfollowed";
            tracing::info!(
                site = "wdgwars",
                job = id,
                rows,
                elapsed_s,
                result,
                reason = ?reason,
                "WDGWars upload"
            );
            result
        }
        wdgwars::Waited::Failed(reason) => {
            let result = "failed";
            tracing::warn!(
                site = "wdgwars",
                job = id,
                rows,
                elapsed_s,
                result,
                reason = reason.as_str(),
                "WDGWars upload"
            );
            result
        }
    };
    set_upload_result(&job.conn, job.record, result)?;
    Ok(())
}

/// What an upload under way reports. Each carries the job, so a front end draws it statelessly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// WDGWars took the file as `job`. Once, before the first poll.
    Queued { job: u64 },
    /// A poll found the job queued or processing.
    State { job: u64, state: wdgwars::JobState },
    /// A poll failed, and the wait carries on.
    PollFailed { job: u64, error: SiteError },
}

/// What a prepared upload came to.
#[derive(Debug)]
pub enum Outcome {
    NotSent(SendError),
    /// WiGLE queued it. `unrecorded`: its `queued` result could not be written; the row is, so
    /// it is still a cutoff.
    Wigle {
        queued: wigle::Queued,
        unrecorded: Option<StoreError>,
    },
    /// WDGWars queued it as `job`, whose status is at `url`. `unrecorded`: how the wait ended
    /// could not be written.
    Wdgwars {
        job: u64,
        url: String,
        waited: wdgwars::Waited,
        unrecorded: Option<StoreError>,
    },
}

impl Outcome {
    /// Whether a retry is called for: never queued, the import failed, or its end is unrecorded.
    /// A WiGLE upload whose `queued` result is unwritten is none.
    #[must_use]
    pub fn failed(&self) -> bool {
        match self {
            Self::NotSent(_) => true,
            Self::Wigle { .. } => false,
            Self::Wdgwars { waited, unrecorded, .. } => {
                matches!(waited, wdgwars::Waited::Failed(_)) || unrecorded.is_some()
            }
        }
    }
}

/// [`send`], then for WDGWars [`wdgwars::wait`] on the real clock and [`finish`]. Reports every
/// step through `say`. Blocks until the upload has ended or the wait has given up.
pub fn upload(
    prepared: &Prepared,
    consent: Consent,
    db: &Path,
    endpoints: &Endpoints,
    mut say: impl FnMut(Progress),
) -> Outcome {
    let (client, job) = match send(prepared, consent, db, endpoints) {
        Err(error) => return Outcome::NotSent(error),
        Ok(Sent::Wigle { queued, unrecorded }) => return Outcome::Wigle { queued, unrecorded },
        Ok(Sent::Wdgwars { client, job }) => (client, job),
    };
    let id = job.id;
    say(Progress::Queued { job: id });
    let started = Instant::now();
    let waited = wdgwars::wait(
        id,
        wdgwars::give_up(job.rows),
        || client.poll(id),
        std::thread::sleep,
        || started.elapsed(),
        &mut say,
    );
    let unrecorded = finish(&job, &waited, started.elapsed()).err();
    Outcome::Wdgwars { job: id, url: client.job_url(id), waited, unrecorded }
}

/// Now, in unix milliseconds.
fn unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| i64::try_from(since.as_millis()).unwrap_or(i64::MAX))
}

/// The name the file goes up under: the capture's, as a gzipped CSV.
fn filename(db: &Path) -> String {
    let stem = db.file_stem().map_or_else(|| "wartui".into(), |s| s.to_string_lossy());
    format!("{stem}.csv.gz")
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::net::TcpListener;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use flate2::read::GzDecoder;
    use wartui_core::export::{ExportFilter, ExportSummary, wigle_csv};
    use wartui_core::store::{
        StoreError, UploadSite, last_upload, open_readonly, open_readwrite, record_upload,
        set_upload_result,
    };

    use super::testing::{
        EPOCH_MS, capture, capture_simulated, prepared, serve_once, serve_sequence, sighting,
    };
    use super::{
        Consent, Endpoints, Job, Missing, Options, Outcome, Prepare, Progress, Refusal, SendError,
        Sent, SiteError, SiteRefusal, body, check_size, finish, prepare, send, upload, wdgwars,
        wigle,
    };
    use crate::settings::ApiKeys;

    /// A local server for a site the test should never reach.
    const NOWHERE: &str = "http://127.0.0.1:9";

    fn keyed() -> ApiKeys {
        ApiKeys { wdgwars: "secret".to_owned(), ..ApiKeys::default() }
    }

    /// [`keyed`], with a WiGLE name and token too.
    fn both() -> ApiKeys {
        ApiKeys {
            wigle_name: "AIDexamplename".to_owned(),
            wigle_token: "exampletoken789".to_owned(),
            ..keyed()
        }
    }

    fn options() -> Options {
        Options::new("test")
    }

    /// Both sites at `wdgwars` and `wigle`.
    fn endpoints(wdgwars: &str, wigle: &str) -> Endpoints {
        Endpoints { wdgwars: wdgwars.to_owned(), wigle: wigle.to_owned() }
    }

    /// `prepare` with `options`, as a list of outcomes.
    fn outcomes_of(keys: &ApiKeys, db: &Path, options: &Options) -> Vec<(UploadSite, Prepare)> {
        prepare(keys, db, options).unwrap()
    }

    /// `prepare` with the default options, for the one site `keys` names.
    fn prepare_one(keys: &ApiKeys, db: &Path, resend: bool) -> Prepare {
        let options = Options { resend, ..options() };
        let mut outcomes = outcomes_of(keys, db, &options);
        assert_eq!(outcomes.len(), 1);
        outcomes.remove(0).1
    }

    /// `site`'s outcome in `outcomes`.
    fn outcome(outcomes: Vec<(UploadSite, Prepare)>, site: UploadSite) -> Prepare {
        outcomes.into_iter().find(|(s, _)| *s == site).expect("an outcome for the site").1
    }

    /// `prepared`, sent with consent to it.
    fn send_consented(
        prepared: &super::Prepared,
        db: &Path,
        endpoints: &Endpoints,
    ) -> Result<Sent, SendError> {
        send(prepared, Consent::to(prepared), db, endpoints)
    }

    #[test]
    fn upload_body_gunzips_to_export_csv_when_capture_has_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            dir.path(),
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        let conn = open_readonly(&db).unwrap();
        let built = body(&conn, &ExportFilter::default(), "test").unwrap();
        assert_eq!(built.summary.rows, 2);

        let mut csv = Vec::new();
        GzDecoder::new(built.gz.as_slice()).read_to_end(&mut csv).unwrap();
        let mut expected = Vec::new();
        wigle_csv(&conn, &ExportFilter::default(), &mut expected, "test").unwrap();
        assert_eq!(csv, expected);
    }

    #[test]
    fn upload_body_counts_csv_bytes_before_compression_when_built() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            dir.path(),
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        let built = body(&open_readonly(&db).unwrap(), &ExportFilter::default(), "test").unwrap();
        let mut csv = Vec::new();
        GzDecoder::new(built.gz.as_slice()).read_to_end(&mut csv).unwrap();
        assert_eq!(built.csv_bytes, csv.len() as u64);
    }

    #[test]
    fn upload_body_names_app_version_when_options_give_one() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let built = body(&open_readonly(&db).unwrap(), &ExportFilter::default(), "9.8.7").unwrap();
        let mut csv = String::new();
        GzDecoder::new(built.gz.as_slice()).read_to_string(&mut csv).unwrap();
        assert!(csv.starts_with("WigleWifi-1.6,appRelease=9.8.7,"), "{csv}");
    }

    #[test]
    fn upload_prepared_debug_prints_no_credentials_when_formatted() {
        for site in [UploadSite::Wdgwars, UploadSite::Wigle] {
            let prepared = prepared(site, b"BODYBYTES".to_vec(), 9, ExportSummary::default(), None);
            let debug = format!("{prepared:?}");
            assert!(!debug.contains("secret"), "{debug}");
            assert!(!debug.contains("AIDexamplename"), "{debug}");
            assert!(!debug.contains("exampletoken789"), "{debug}");
            assert!(!debug.contains("66, 79, 68, 89"), "the body: {debug}");
        }
    }

    #[test]
    fn upload_check_size_refuses_when_csv_over_wdgwars_limit() {
        // WDGWars counts a gzipped file by its decompressed size, so a CSV over the limit is
        // refused however well it compresses.
        assert_eq!(
            check_size(UploadSite::Wdgwars, wdgwars::MAX_CSV + 1, 10),
            Err(SiteRefusal::TooLarge { bytes: wdgwars::MAX_CSV + 1, limit: wdgwars::MAX_CSV })
        );
        assert!(check_size(UploadSite::Wdgwars, wdgwars::MAX_CSV, wdgwars::MAX_CSV * 2).is_ok());
    }

    #[test]
    fn upload_check_size_counts_gzipped_file_when_site_is_wigle() {
        // WiGLE's limit is on the file sent, so a CSV that compresses under it goes.
        assert!(check_size(UploadSite::Wigle, wigle::MAX_FILE * 3, wigle::MAX_FILE).is_ok());
        assert_eq!(
            check_size(UploadSite::Wigle, 10, wigle::MAX_FILE + 1),
            Err(SiteRefusal::TooLarge { bytes: wigle::MAX_FILE + 1, limit: wigle::MAX_FILE })
        );
    }

    #[test]
    fn upload_refuses_without_contacting_server_when_no_credentials() {
        // `prepare` takes no client, so a refusal here cannot have sent anything; the
        // capture does not even exist.
        let refusal = prepare(&ApiKeys::default(), Path::new("/nonexistent/wartui.db"), &options());
        assert!(matches!(refusal, Err(Refusal::NoCredentials)), "{refusal:?}");
    }

    #[test]
    fn upload_refuses_named_site_when_it_has_no_credentials() {
        let options = Options { to: Some(UploadSite::Wdgwars), ..options() };
        let outcomes =
            outcomes_of(&ApiKeys::default(), Path::new("/nonexistent/wartui.db"), &options);
        assert!(
            matches!(
                outcomes.as_slice(),
                [(
                    UploadSite::Wdgwars,
                    Prepare::Refused(SiteRefusal::Missing(Missing::WdgwarsKey))
                )]
            ),
            "{outcomes:?}"
        );
    }

    #[test]
    fn upload_refuses_every_site_when_capture_simulated() {
        let dir = tempfile::tempdir().unwrap();
        let records = vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))];
        let db = capture_simulated(dir.path(), records, true);
        for resend in [false, true] {
            let options = Options { resend, ..options() };
            let refusal = prepare(&both(), &db, &options);
            assert!(matches!(refusal, Err(Refusal::TestData)), "{refusal:?}");
        }
    }

    #[test]
    fn upload_refuses_when_capture_cannot_be_opened() {
        let refusal = prepare(&keyed(), Path::new("/nonexistent/wartui.db"), &options());
        assert!(matches!(refusal, Err(Refusal::Unopened(_))), "{refusal:?}");
    }

    #[test]
    fn upload_sends_nothing_when_capture_has_no_positioned_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, None)]);
        assert!(matches!(prepare_one(&keyed(), &db, false), Prepare::NoRows(_)));
    }

    #[test]
    fn upload_prepare_reports_no_rows_when_every_ssid_is_excluded() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            dir.path(),
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 2], EPOCH_MS + 1_000, Some(37.1)),
            ],
        );
        // `sighting` stores every network as `example`.
        let excluded_ssids = std::collections::HashSet::from([b"example".to_vec()]);
        let options = Options { excluded_ssids, ..options() };
        let outcomes = outcomes_of(&keyed(), &db, &options);
        let Prepare::NoRows(summary) = outcome(outcomes, UploadSite::Wdgwars) else {
            panic!("rows to upload")
        };
        assert_eq!(summary.excluded_ssid, 2);
    }

    #[test]
    fn upload_prepare_covers_every_configured_site_when_none_named() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let outcomes = outcomes_of(&both(), &db, &options());
        let sites: Vec<UploadSite> = outcomes.iter().map(|(site, _)| *site).collect();
        assert_eq!(sites, [UploadSite::Wdgwars, UploadSite::Wigle]);
        assert!(outcomes.iter().all(|(_, outcome)| matches!(outcome, Prepare::Ready(_))));
    }

    #[test]
    fn upload_prepare_covers_only_named_site_when_to_given() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..options() };
        let outcomes = outcomes_of(&both(), &db, &options);
        let [(UploadSite::Wigle, Prepare::Ready(prepared))] = outcomes.as_slice() else {
            panic!("not WiGLE alone")
        };
        assert_eq!(prepared.site(), UploadSite::Wigle);
    }

    #[test]
    fn upload_prepare_refuses_wigle_when_token_missing() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let keys = ApiKeys { wigle_token: "  ".to_owned(), ..both() };
        let outcomes = outcomes_of(&keys, &db, &options());
        assert!(
            matches!(
                outcomes.as_slice(),
                [
                    (UploadSite::Wdgwars, Prepare::Ready(_)),
                    (
                        UploadSite::Wigle,
                        Prepare::Refused(SiteRefusal::Missing(Missing::WigleToken))
                    )
                ]
            ),
            "{outcomes:?}"
        );
    }

    #[test]
    fn upload_prepare_names_what_wigle_lacks_when_credentials_are_half() {
        let path = Path::new("/nonexistent/wartui.db");
        let only = Options { to: Some(UploadSite::Wigle), ..options() };
        for (name, token, missing) in [
            ("", "", Missing::WigleNameAndToken),
            ("AIDexamplename", "", Missing::WigleToken),
            ("", "exampletoken789", Missing::WigleName),
        ] {
            let keys =
                ApiKeys { wigle_name: name.to_owned(), wigle_token: token.to_owned(), ..keyed() };
            let outcomes = outcomes_of(&keys, path, &only);
            let [(UploadSite::Wigle, Prepare::Refused(SiteRefusal::Missing(found)))] =
                outcomes.as_slice()
            else {
                panic!("{outcomes:?}")
            };
            assert_eq!(*found, missing);
        }
    }

    #[test]
    fn upload_prepare_folds_each_site_over_its_window_when_no_recapture_given() {
        // One network seen twice, 10 s apart: past WiGLE's 5 s, inside WDGWars' hour.
        let dir = tempfile::tempdir().unwrap();
        let db = capture(
            dir.path(),
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS + 10_000, Some(37.1)),
            ],
        );
        let mut outcomes = outcomes_of(&both(), &db, &options());
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
            dir.path(),
            vec![
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0)),
                sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS + 10_000, Some(37.1)),
            ],
        );
        for (recapture, rows) in [(3_600, 1), (1, 2)] {
            let options = Options { recapture: Some(recapture), ..options() };
            for (site, outcome) in outcomes_of(&both(), &db, &options) {
                let Prepare::Ready(prepared) = outcome else { panic!("{site:?} not ready") };
                assert_eq!(prepared.summary.rows, rows, "{site:?} at {recapture} s");
            }
        }
    }

    /// Three networks heard a minute apart, positioned, with a WDGWars upload recorded through
    /// the second.
    fn uploaded_through_second(dir: &Path) -> PathBuf {
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
        let db = uploaded_through_second(dir.path());
        let outcomes = outcomes_of(&both(), &db, &options());
        let Prepare::Ready(wigle) = outcome(outcomes, UploadSite::Wigle) else {
            panic!("WiGLE not ready")
        };
        assert_eq!(wigle.summary.rows, 3);
        assert_eq!(wigle.previous, None);
    }

    #[test]
    fn upload_records_job_and_cutoff_when_submit_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare_one(&keyed(), &db, false).ready();
        assert_eq!(prepared.previous, None);

        let (base, server) = serve_once("202 Accepted", r#"{"ok":true,"job_id":42}"#);
        let sent = send_consented(&prepared, &db, &endpoints(&base, NOWHERE)).unwrap();
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
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..options() };
        let prepared = outcome(outcomes_of(&both(), &db, &options), UploadSite::Wigle).ready();

        let (base, server) = serve_once(
            "200 OK",
            r#"{"success":true,"results":{"transids":[{"transId":"20261122-00017"}]}}"#,
        );
        let sent = send_consented(&prepared, &db, &endpoints(NOWHERE, &base)).unwrap();
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
        let outcomes = outcomes_of(&both(), &db, &self::options());
        let Prepare::NothingNew { previous, .. } = outcome(outcomes, UploadSite::Wigle) else {
            panic!("WiGLE has something new")
        };
        assert_eq!(previous.job, "20261122-00017");
        let outcomes = outcomes_of(&both(), &db, &self::options());
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
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let options = Options { to: Some(UploadSite::Wigle), ..options() };
        let prepared = outcome(outcomes_of(&both(), &db, &options), UploadSite::Wigle).ready();
        refuse_result_writes(&db);

        let (base, server) = serve_once(
            "200 OK",
            r#"{"success":true,"results":{"transids":[{"transId":"20261122-00017"}]}}"#,
        );
        let sent = send_consented(&prepared, &db, &endpoints(NOWHERE, &base)).unwrap();
        server.join().unwrap();
        let Sent::Wigle { queued, unrecorded: Some(error) } = sent else {
            panic!("not a WiGLE transaction with an unrecorded result")
        };
        assert_eq!(queued.trans_id, "20261122-00017");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");

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
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
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
        let error = send_consented(&prepared, &db, &endpoints(&base, &base)).err();
        assert!(matches!(error, Some(SendError::Capture(StoreError::ReadOnly(_)))), "{error:?}");
        listener.set_nonblocking(true).unwrap();
        let accepted = listener.accept();
        assert!(accepted.is_err(), "the upload was sent anyway");
    }

    #[test]
    fn upload_send_refuses_and_sends_nothing_when_consent_is_for_another_upload() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let mut outcomes = outcomes_of(&both(), &db, &options());
        let wigle = outcomes.pop().unwrap().1.ready();
        let wdgwars = outcomes.pop().unwrap().1.ready();

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        // Consent to WiGLE's upload sends neither WDGWars' nor, rebuilt, a larger one.
        let error = send(&wdgwars, Consent::to(&wigle), &db, &endpoints(&base, &base)).err();
        assert!(matches!(error, Some(SendError::NoConsent)), "{error:?}");
        let mut grown = prepared(UploadSite::Wigle, Vec::new(), 0, wigle.summary.clone(), None);
        grown.summary.rows += 1;
        let error = send(&grown, Consent::to(&wigle), &db, &endpoints(&base, &base)).err();
        assert!(matches!(error, Some(SendError::NoConsent)), "{error:?}");

        listener.set_nonblocking(true).unwrap();
        assert!(listener.accept().is_err(), "the upload was sent anyway");
        assert!(last_upload(&open_readonly(&db).unwrap(), UploadSite::Wdgwars).unwrap().is_none());
    }

    #[test]
    fn upload_sends_only_new_sightings_when_repeated() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(dir.path());
        let prepared = prepare_one(&keyed(), &db, false).ready();
        assert_eq!(macs(&prepared.body), ["10:00:00:00:00:03"]);
        assert_eq!(prepared.previous.map(|p| p.job).as_deref(), Some("7"));
    }

    #[test]
    fn upload_sends_everything_when_resend() {
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(dir.path());
        let prepared = prepare_one(&keyed(), &db, true).ready();
        assert_eq!(prepared.summary.rows, 3);
        assert_eq!(prepared.previous, None);
    }

    #[test]
    fn upload_finish_records_failed_when_job_failed() {
        // A failed job is no cutoff, so the next upload sends what it carried again.
        let dir = tempfile::tempdir().unwrap();
        let db = uploaded_through_second(dir.path());
        let conn = open_readwrite(&db).unwrap();
        let record =
            record_upload(&conn, UploadSite::Wdgwars, 3, EPOCH_MS + 150_000, "8", 1).unwrap();
        let job = Job { id: 8, rows: 1, record, conn };
        let failed = wdgwars::Waited::Failed("failed".to_owned());
        finish(&job, &failed, Duration::from_secs(3)).unwrap();

        let previous = last_upload(&open_readonly(&db).unwrap(), UploadSite::Wdgwars)
            .unwrap()
            .expect("a record");
        assert_eq!(previous.job, "7");
    }

    #[test]
    fn upload_reports_queued_then_outcome_when_job_is_followed() {
        let dir = tempfile::tempdir().unwrap();
        let db = capture(dir.path(), vec![sighting([0x10, 0, 0, 0, 0, 1], EPOCH_MS, Some(37.0))]);
        let prepared = prepare_one(&keyed(), &db, false).ready();
        let (base, server) = serve_sequence(&[
            ("202 Accepted", r#"{"ok":true,"job_id":42}"#),
            ("200 OK", r#"{"ok":true,"job_id":42,"status":"processing"}"#),
            ("200 OK", r#"{"ok":true,"job_id":42,"status":"done","result":{"imported":1}}"#),
        ]);
        let mut said = Vec::new();
        let outcome = upload(
            &prepared,
            Consent::to(&prepared),
            &db,
            &endpoints(&base, NOWHERE),
            |progress| said.push(progress),
        );
        server.join().unwrap();

        assert_eq!(
            said,
            [
                Progress::Queued { job: 42 },
                Progress::State { job: 42, state: wdgwars::JobState::Processing },
            ]
        );
        let Outcome::Wdgwars { job: 42, url, waited, unrecorded: None } = &outcome else {
            panic!("{outcome:?}")
        };
        assert_eq!(url, &format!("{base}/api/v2/upload-job/42"));
        let imported =
            wdgwars::Count { name: "imported".to_owned(), value: wdgwars::CountValue::Number(1) };
        assert_eq!(waited, &wdgwars::Waited::Done(vec![imported]));
        assert!(!outcome.failed());
        let conn = open_readonly(&db).unwrap();
        let result: Option<String> =
            conn.query_row("SELECT result FROM upload", [], |row| row.get(0)).unwrap();
        assert_eq!(result.as_deref(), Some("done"));
    }

    fn queued() -> wigle::Queued {
        wigle::Queued { trans_id: "20261122-00017".to_owned(), warning: None }
    }

    /// A WDGWars outcome that waited to `waited`, its end recorded unless `unrecorded`.
    fn followed(waited: wdgwars::Waited, unrecorded: bool) -> Outcome {
        Outcome::Wdgwars {
            job: 42,
            url: format!("{NOWHERE}/api/v2/upload-job/42"),
            waited,
            unrecorded: unrecorded.then_some(StoreError::NotACapture),
        }
    }

    #[test]
    fn upload_outcome_fails_when_not_sent() {
        assert!(Outcome::NotSent(SendError::NoConsent).failed());
        let site = SendError::Site(SiteError::TooLarge);
        assert!(Outcome::NotSent(site).failed());
    }

    #[test]
    fn upload_outcome_fails_when_import_failed() {
        assert!(followed(wdgwars::Waited::Failed("bad header".to_owned()), false).failed());
    }

    #[test]
    fn upload_outcome_fails_when_end_unrecorded() {
        assert!(followed(wdgwars::Waited::Done(Vec::new()), true).failed());
    }

    #[test]
    fn upload_outcome_succeeds_when_wigle_queued_with_an_unwritten_result() {
        // WiGLE has the file and the row is a cutoff, so a retry would send nothing new.
        let outcome =
            Outcome::Wigle { queued: queued(), unrecorded: Some(StoreError::NotACapture) };
        assert!(!outcome.failed());
    }

    #[test]
    fn upload_outcome_succeeds_when_done() {
        assert!(!followed(wdgwars::Waited::Done(Vec::new()), false).failed());
        let unfollowed = wdgwars::Unfollowed::TimedOut { secs: 30, last: None };
        assert!(!followed(wdgwars::Waited::Unfollowed(unfollowed), false).failed());
        assert!(!Outcome::Wigle { queued: queued(), unrecorded: None }.failed());
    }
}
