//! A capture sent to WDGWars and WiGLE: `wartui_app::upload`, for the phone.
//!
//! **Two steps, as the terminal takes them.** [`prepare_upload`] reads the capture and builds each
//! site's body, contacting nothing, so a confirm screen can show what would go. Each ready site
//! is a [`PreparedUpload`], opaque because it holds the credentials and the body, and its
//! [`PreparedUpload::send`] is the operator's yes.
//!
//! **The consent is built inside `send` and nowhere else.** An upload publishes to the operator's
//! profile and cannot be taken back, so the app layer sends only with a `Consent` naming the very
//! upload shown (`wartui_app::upload` `//!`). Nothing exported builds one, and none is kept: the
//! phone's call to `send`, made from the operator's confirmation, is the only way one comes to
//! exist, and it is spent on the upload that call takes. A second `send` finds nothing to take.
//!
//! **Off the phone's thread.** Preparing reads the whole capture, and sending takes as long as
//! the uplink and WDGWars' job, so both run on the runtime's blocking pool and the phone awaits
//! them. A listener is called from that pool, with no lock held; one that fails is ignored.
//!
//! **The sites themselves, and no other.** A [`PreparedUpload`] made by [`prepare_upload`] goes to
//! `Endpoints::live()`. Behind the `testing` feature, `prepare_upload_at` sends to the base URLs a
//! test gives, which is the only way a test sends anything.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use wartui_app::export::{Exported as AppExported, NodeNames};
use wartui_app::upload::wdgwars::{Count, CountValue, JobState, Last, Unfollowed, Waited};
use wartui_app::upload::wigle::Queued;
use wartui_app::upload::{
    self as app, Consent, Endpoints, Missing, Outcome, Prepare, Prepared, Progress, Refusal,
    SendError, SiteError, SiteRefusal,
};
use wartui_core::export::ExportError as CoreError;
use wartui_core::store::{UploadRecord, UploadSite, open_readonly};

use crate::export::Exported;
use crate::log::SinkError;
use crate::runtime::runtime;
use crate::settings::{Settings, chain};

/// A site a capture is uploaded to.
#[uniffi::remote(Enum)]
pub enum UploadSite {
    /// wdgwars.pl.
    Wdgwars,
    /// wigle.net.
    Wigle,
}

/// Which credentials a site lacks. Both languages call it `MissingCredentials`, since inside
/// `SiteRefusal.Missing` Kotlin would read `Missing` as that variant (`uniffi.toml`).
#[uniffi::remote(Enum)]
pub enum Missing {
    WdgwarsKey,
    WigleNameAndToken,
    WigleName,
    WigleToken,
}

/// Why one site cannot be sent to. The other still goes.
#[uniffi::remote(Enum)]
pub enum SiteRefusal {
    /// Its credentials are missing, whole or half.
    Missing(Missing),
    /// `bytes` over `limit`: the CSV before compression for WDGWars, the file as sent for WiGLE.
    TooLarge { bytes: u64, limit: u64 },
}

/// What a site's API did other than queue the upload or answer a poll.
#[uniffi::remote(Enum)]
pub enum SiteError {
    /// The upload's request failed in transport, with the transport's reason. When it was the
    /// answer that was lost, the site may still have taken the file.
    Unsent { url: String, error: String },
    /// A poll's request failed in transport, with the transport's reason.
    Unreached { url: String, error: String },
    /// WDGWars redirected to its login page: the endpoint wants a logged-in session.
    LoginRequired { location: String },
    /// The site redirected elsewhere, to the `Location` it named, if it named one.
    Redirected { location: Option<String> },
    /// The site refused the credentials, with its reason.
    CredentialsRejected { reason: Option<String> },
    /// WDGWars' 413.
    TooLarge,
    /// WiGLE's 429, with its reason.
    TooManyUploads { reason: Option<String> },
    /// Any other status, with what the site said.
    Status { code: u16, said: Option<String> },
    /// A 2xx body that could not be read, with the transport's reason.
    Unread { error: String },
    /// A 2xx body that is not UTF-8.
    NotText,
    /// An answer that is not a JSON object, quoted.
    NotJson { quoted: String },
    /// The answer says it failed, with its reason.
    Refused { reason: String },
    /// The upload was accepted, naming no job or transaction. The answer, quoted.
    Unnamed { quoted: String },
    /// A job status this build does not know. The answer, quoted.
    UnknownState { quoted: String },
}

/// An upload WiGLE queued.
#[uniffi::remote(Record)]
pub struct Queued {
    /// WiGLE's name for it.
    pub trans_id: String,
    /// What WiGLE warned of, when it warned.
    pub warning: Option<String>,
}

/// Where a WDGWars job stands while it has not ended.
#[uniffi::remote(Enum)]
pub enum JobState {
    Queued,
    Processing,
}

/// One count a finished WDGWars job reports.
#[uniffi::remote(Record)]
pub struct Count {
    /// The site's name for it, such as `imported`.
    pub name: String,
    pub value: CountValue,
}

/// A count's value: a whole number, or whatever else the site sent, as text.
#[uniffi::remote(Enum)]
pub enum CountValue {
    Number(u64),
    Text(String),
}

/// How the wait for a WDGWars job ended.
#[uniffi::remote(Enum)]
pub enum Waited {
    /// Imported, with the counts the site sent: `imported`, `captured` and `updated` first.
    Done(Vec<Count>),
    /// Not followed to the end. The upload stands.
    Unfollowed(Unfollowed),
    /// The site reports the import failed, with its reason. It imported nothing.
    Failed(String),
}

/// Why the wait stopped before the job ended. Both languages call it `JobUnfollowed`, since
/// inside `Waited.Unfollowed` Kotlin would read `Unfollowed` as that variant (`uniffi.toml`).
#[uniffi::remote(Enum)]
pub enum Unfollowed {
    /// The site will not tell an API key about the job: `LoginRequired` or `CredentialsRejected`.
    Unreadable(SiteError),
    /// The give-up time passed, `secs` rounded to the second. `last`: the state the job was last
    /// in, else the last poll's error, else nothing.
    TimedOut { secs: u64, last: Option<Last> },
}

/// What the polls last heard of a job that timed out.
#[uniffi::remote(Enum)]
pub enum Last {
    State(JobState),
    PollFailed(SiteError),
}

/// What an upload covers, beside the capture.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UploadOptions {
    /// The one site to send to; every site with credentials when `None`.
    #[uniffi(default)]
    pub to: Option<UploadSite>,
    /// How long a network's sightings fold into one row, in seconds, for every site; each site's
    /// own window when `None`.
    #[uniffi(default)]
    pub recapture_secs: Option<u32>,
    /// Send everything, ignoring what earlier uploads sent.
    #[uniffi(default)]
    pub resend: bool,
    /// Wi-Fi networks left out, matched byte for byte.
    #[uniffi(default)]
    pub excluded_ssids: Vec<String>,
    /// The app's version, which the CSV's header names.
    pub app_version: String,
}

impl UploadOptions {
    fn into_app(self) -> app::Options {
        app::Options {
            to: self.to,
            recapture: self.recapture_secs.map(u64::from),
            resend: self.resend,
            excluded_ssids: wartui_app::settings::excluded_ssid_bytes(&self.excluded_ssids)
                .collect(),
            app_version: self.app_version,
        }
    }
}

/// An earlier upload to a site, which this one follows on from.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PreviousUpload {
    /// The site's name for it: WDGWars' job number, or WiGLE's transaction id.
    pub job: String,
    /// When the site queued it, in Unix milliseconds.
    pub uploaded_at_ms: i64,
    /// Rows it sent.
    pub rows: u64,
}

impl From<UploadRecord> for PreviousUpload {
    fn from(record: UploadRecord) -> Self {
        let UploadRecord { job, uploaded_at_ms, rows } = record;
        Self { job, uploaded_at_ms, rows }
    }
}

/// What a confirm screen shows of one site's upload.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UploadShown {
    /// The site it goes to.
    pub site: UploadSite,
    /// What went into the CSV.
    pub summary: Exported,
    /// The CSV's length before compression, the size WDGWars' limit counts.
    pub csv_bytes: u64,
    /// The size of the file as sent.
    pub file_bytes: u64,
    /// The upload to this site this one follows on from. `None` when resending.
    pub previous: Option<PreviousUpload>,
}

/// What [`prepare_upload`] found for one site.
#[derive(Debug, uniffi::Enum)]
pub enum SitePrepared {
    /// Built, and waiting for the operator's yes.
    Ready { upload: Arc<PreparedUpload> },
    /// Every row was covered by `previous`.
    NothingNew { site: UploadSite, previous: PreviousUpload, summary: Exported },
    /// No positioned row, and no upload before this one.
    NoRows { site: UploadSite, summary: Exported },
    /// The site cannot be sent to.
    Refused { site: UploadSite, refusal: SiteRefusal },
}

/// Why the whole upload is refused before anything is built or sent. `detail` is the store's,
/// SQLite's or the OS's own account.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UploadRefusal {
    /// No site has credentials.
    #[error("no upload credentials")]
    NoCredentials,
    /// The capture holds test data: a simulated fleet or a typed-in position.
    #[error("the capture holds test data")]
    TestData,
    /// The capture cannot be opened.
    #[error("opening the capture: {detail}")]
    Unopened { detail: String },
    /// The capture cannot be read.
    #[error("reading the capture: {detail}")]
    Unread { detail: String },
    /// The CSV cannot be built.
    #[error("exporting the capture: {detail}")]
    Export { detail: String },
    /// The gzip could not be finished.
    #[error("compressing the upload: {detail}")]
    Compress { detail: String },
    /// The work preparing it panicked.
    #[error("preparing the upload panicked")]
    Panicked,
}

impl From<Refusal> for UploadRefusal {
    fn from(refusal: Refusal) -> Self {
        match refusal {
            Refusal::NoCredentials => Self::NoCredentials,
            Refusal::TestData => Self::TestData,
            Refusal::Unopened(error) => Self::Unopened { detail: chain(&error) },
            Refusal::Unread(error) => Self::Unread { detail: chain(&error) },
            Refusal::Export(CoreError::Sqlite(error)) => Self::Export { detail: chain(&error) },
            Refusal::Export(CoreError::Io(error)) => Self::Export { detail: chain(&error) },
            Refusal::Compress(error) => Self::Compress { detail: chain(&error) },
        }
    }
}

/// What an upload under way reports. Each carries the job, so a screen draws it statelessly.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum UploadProgress {
    /// WDGWars took the file as `job`. Once, before the first poll.
    Queued { job: u64 },
    /// A poll found the job queued or processing.
    State { job: u64, state: JobState },
    /// A poll failed, and the wait carries on.
    PollFailed { job: u64, error: SiteError },
}

impl From<Progress> for UploadProgress {
    fn from(progress: Progress) -> Self {
        match progress {
            Progress::Queued { job } => Self::Queued { job },
            Progress::State { job, state } => Self::State { job, state },
            Progress::PollFailed { job, error } => Self::PollFailed { job, error },
        }
    }
}

/// Why a send did not end with the site's upload recorded. `detail` is the store's own account.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum UploadSendFailure {
    /// The consent named another upload. Nothing was sent.
    NoConsent,
    /// The capture cannot be opened for writing. Nothing was sent.
    Capture { detail: String },
    /// The site did not queue the upload.
    Site { error: SiteError },
    /// The site queued it as `job` and the capture could not record it, so a retry sends its
    /// rows again.
    Unrecorded { job: String, detail: String },
}

impl From<SendError> for UploadSendFailure {
    fn from(error: SendError) -> Self {
        match error {
            SendError::NoConsent => Self::NoConsent,
            SendError::Capture(error) => Self::Capture { detail: chain(&error) },
            SendError::Site(error) => Self::Site { error },
            SendError::Unrecorded { job, error } => Self::Unrecorded { job, detail: chain(&error) },
        }
    }
}

/// What one site's upload came to.
#[derive(Debug, PartialEq, uniffi::Enum)]
pub enum SiteOutcome {
    /// The send did not end with the upload recorded.
    SendFailed { failure: UploadSendFailure },
    /// WiGLE queued it. `unrecorded`: the store's account of why its `queued` result could not be
    /// written; the upload is recorded all the same.
    Wigle { queued: Queued, unrecorded: Option<String> },
    /// WDGWars queued it as `job`, whose status is at `url`. `unrecorded`: the store's account of
    /// why how the wait ended could not be written.
    Wdgwars { job: u64, url: String, waited: Waited, unrecorded: Option<String> },
}

/// What a send came to, and whether a retry is called for.
#[derive(Debug, PartialEq, uniffi::Record)]
pub struct UploadOutcome {
    pub outcome: SiteOutcome,
    /// A retry is called for: not sent, sent and left unrecorded, the import failed, or its end
    /// is unrecorded.
    pub failed: bool,
}

impl From<Outcome> for UploadOutcome {
    fn from(outcome: Outcome) -> Self {
        let failed = outcome.failed();
        let outcome = match outcome {
            Outcome::SendFailed(error) => SiteOutcome::SendFailed { failure: error.into() },
            Outcome::Wigle { queued, unrecorded } => {
                SiteOutcome::Wigle { queued, unrecorded: unrecorded.map(|error| chain(&error)) }
            }
            Outcome::Wdgwars { job, url, waited, unrecorded } => SiteOutcome::Wdgwars {
                job,
                url,
                waited,
                unrecorded: unrecorded.map(|error| chain(&error)),
            },
        };
        Self { outcome, failed }
    }
}

/// Why [`PreparedUpload::send`] sent nothing.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum UploadError {
    /// This upload was sent already.
    #[error("the upload was sent already")]
    AlreadySent,
    /// The work sending it panicked, so whether the site took it is unknown.
    #[error("the upload panicked")]
    Panicked,
}

/// What an upload under way says as it goes.
#[uniffi::export(with_foreign)]
pub trait UploadListener: Send + Sync {
    /// One step, on any thread. A listener that fails is ignored.
    fn progress(&self, progress: UploadProgress) -> Result<(), SinkError>;
}

/// One site's upload, built and not yet sent. Opaque: it holds the credentials and the body.
#[derive(Debug, uniffi::Object)]
pub struct PreparedUpload {
    /// The upload, until it is sent.
    prepared: Mutex<Option<Prepared>>,
    shown: UploadShown,
    db: PathBuf,
    endpoints: Endpoints,
}

#[uniffi::export]
impl PreparedUpload {
    /// What a confirm screen shows.
    #[must_use]
    pub fn shown(&self) -> UploadShown {
        self.shown.clone()
    }

    /// The operator said yes: send it, follow WDGWars' job to its end, and record it in the
    /// capture. Each step goes to `listener`.
    ///
    /// An upload publishes to the operator's profile and cannot be taken back, so call this only
    /// from the operator's confirmation of what [`Self::shown`] showed. It can run for minutes,
    /// so the app runs it in a background task. A send whose answer is lost reads as a failed
    /// send, though the site may have taken the file, and a retry may then send the rows again.
    ///
    /// # Errors
    /// [`UploadError::AlreadySent`] on a second call; nothing is sent.
    pub async fn send(
        &self,
        listener: Option<Arc<dyn UploadListener>>,
    ) -> Result<UploadOutcome, UploadError> {
        let prepared = self.lock().take().ok_or(UploadError::AlreadySent)?;
        let (db, endpoints) = (self.db.clone(), self.endpoints.clone());
        let sending = runtime().spawn_blocking(move || {
            // Built here, from the upload this call took, and spent on it.
            let consent = Consent::to(&prepared);
            let say = |progress: Progress| {
                if let Some(listener) = &listener {
                    // A listener that fails is the phone's to notice; the upload goes on.
                    let _ = listener.progress(progress.into());
                }
            };
            app::upload(&prepared, consent, &db, &endpoints, say)
        });
        match sending.await {
            Ok(outcome) => Ok(outcome.into()),
            Err(_) => Err(UploadError::Panicked),
        }
    }
}

impl PreparedUpload {
    fn lock(&self) -> MutexGuard<'_, Option<Prepared>> {
        // `take` is the only write, so a panic elsewhere leaves nothing torn.
        self.prepared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Read the capture at `db_path` and build each site's upload, with the API keys `settings` hold,
/// one outcome per site. Contacts nothing.
///
/// # Errors
/// [`UploadRefusal`] refuses the whole upload: no site has credentials, or the capture holds
/// test data or cannot be read.
#[uniffi::export]
pub async fn prepare_upload(
    settings: Arc<Settings>,
    db_path: String,
    options: UploadOptions,
) -> Result<Vec<SitePrepared>, UploadRefusal> {
    prepare(&settings, db_path, options, Endpoints::live()).await
}

/// Where a test's uploads go: the base URL of each site's local listener.
#[cfg(feature = "testing")]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UploadEndpoints {
    pub wdgwars: String,
    pub wigle: String,
}

/// [`prepare_upload`], whose uploads go to `endpoints`, for a test's local listeners.
///
/// # Errors
/// As [`prepare_upload`].
#[cfg(feature = "testing")]
#[uniffi::export]
pub async fn prepare_upload_at(
    settings: Arc<Settings>,
    db_path: String,
    options: UploadOptions,
    endpoints: UploadEndpoints,
) -> Result<Vec<SitePrepared>, UploadRefusal> {
    let UploadEndpoints { wdgwars, wigle } = endpoints;
    prepare(&settings, db_path, options, Endpoints { wdgwars, wigle }).await
}

/// [`prepare_upload`]'s work, on the blocking pool, with uploads going to `endpoints`.
async fn prepare(
    settings: &Settings,
    db_path: String,
    options: UploadOptions,
    endpoints: Endpoints,
) -> Result<Vec<SitePrepared>, UploadRefusal> {
    let keys = settings.app().saved().api_keys;
    let preparing = runtime().spawn_blocking(move || {
        let db = PathBuf::from(db_path);
        let outcomes = app::prepare(&keys, &db, &options.into_app())?;
        let names = names(&db, &outcomes);
        let prepared = outcomes.into_iter().map(|(site, prepare)| match prepare {
            Prepare::Ready(prepared) => {
                let shown = UploadShown {
                    site,
                    summary: exported(prepared.summary().clone(), &names),
                    csv_bytes: prepared.csv_bytes(),
                    file_bytes: prepared.file_bytes(),
                    previous: prepared.previous().cloned().map(PreviousUpload::from),
                };
                let upload = PreparedUpload {
                    prepared: Mutex::new(Some(prepared)),
                    shown,
                    db: db.clone(),
                    endpoints: endpoints.clone(),
                };
                SitePrepared::Ready { upload: Arc::new(upload) }
            }
            Prepare::NothingNew { previous, summary } => SitePrepared::NothingNew {
                site,
                previous: previous.into(),
                summary: exported(summary, &names),
            },
            Prepare::NoRows(summary) => {
                SitePrepared::NoRows { site, summary: exported(summary, &names) }
            }
            Prepare::Refused(refusal) => SitePrepared::Refused { site, refusal },
        });
        Ok(prepared.collect())
    });
    match preparing.await {
        Ok(prepared) => prepared.map_err(|refusal: Refusal| refusal.into()),
        Err(_) => Err(UploadRefusal::Panicked),
    }
}

/// `summary` as the binding carries it, its nodes named by `names`.
fn exported(summary: wartui_core::export::ExportSummary, names: &NodeNames) -> Exported {
    AppExported { summary, names: names.clone() }.into()
}

/// What the summaries of `outcomes` call the nodes of the capture at `db`, read once. With no
/// summary among them the capture is not opened: it may not exist. One that cannot be opened
/// names every node by its address tail, as the terminal does.
fn names(db: &Path, outcomes: &[(UploadSite, Prepare)]) -> NodeNames {
    if outcomes.iter().all(|(_, outcome)| matches!(outcome, Prepare::Refused(_))) {
        return NodeNames::by_address();
    }
    match open_readonly(db) {
        Ok(conn) => NodeNames::of(&conn),
        Err(error) => {
            tracing::warn!(%error, "cannot open the capture for its node slots");
            NodeNames::by_address()
        }
    }
}
