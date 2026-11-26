//! A capture, started over a [`Link`] and followed as a stream of views.
//!
//! `wartui_app::session` assembles and runs the capture: the store, the engine, the follow task
//! that saves the bridge and the link. [`Session::start`] builds what `wartui run` builds for it,
//! from what the phone gives: the position tiers in `wartui_app::position`'s order, the fix wait,
//! the saved excluded list and receiver rate, and the link to save. A capture over the simulator,
//! or placed by a typed-in position, is test data; the phone cannot say otherwise.
//!
//! **[`Session::start`] is async.** It takes the link back from the task that reads it while idle
//! (`link.rs`), and creates the store, which is disk I/O. Both run on the library's runtime, so
//! the phone's thread only awaits them. The link is lent for as long as that work runs.
//!
//! **Views are numbered.** A task on the library's runtime turns each snapshot into a `View` with
//! the next `seq`, the first 1, so [`Session::next_view`] waits for one newer than the phone last
//! drew and hands it the latest, skipping any it was too slow for. The stream ends with the
//! capture, and [`Session::stop`] returns once its last view is numbered.
//!
//! **The link goes back on every way out.** [`Session::stop`] commits the last batch and gives
//! the `Link` back, parked, with the bridge it still had announced, so the next capture starts on
//! the one connection. A start the store refuses gives it back too. A session the phone lets go
//! of without `stop` is stopped the same way in the background. Its stop channel's sender
//! dropped would end the capture as a stop does, but the link would go with it.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::watch;
use tokio::task::JoinHandle;
use wartui_app::position::{Fixed, Sources};
use wartui_app::session::{Controls, HostBuild, Refused, Session as Capture, SessionConfig};
use wartui_app::settings::{Busy, Link as Over};
use wartui_app::view::View;
use wartui_core::engine::Command;
use wartui_core::gps::Gps;
use wartui_core::position::DEFAULT_MAX_AGE;
use wartui_core::store::{StoreConfig, StoreError};
use wartui_core::ubx;

use crate::feeds::{HealthFeed, LocationFeed};
use crate::link::{Link, LinkError};
use crate::runtime::spawn;
use crate::settings::{Settings, chain};

/// The build a capture records as its provenance.
#[uniffi::remote(Record)]
pub struct HostBuild {
    /// The app's version.
    pub host_version: Option<String>,
    /// The short git hash it was built from.
    pub host_commit: Option<String>,
    /// Its release tag, when it is a tagged build.
    pub release_tag: Option<String>,
}

/// A typed-in position.
#[uniffi::remote(Record)]
pub struct Fixed {
    /// Degrees north.
    pub lat: f64,
    /// Degrees east.
    pub lon: f64,
    /// Meters above mean sea level, if given.
    pub alt: Option<f64>,
}

/// What a capture is started with.
#[derive(Debug, Clone, uniffi::Record)]
pub struct CaptureConfig {
    /// Where the capture's database goes. It must not exist.
    pub db_path: String,
    /// The operator's notes on the capture.
    pub notes: Option<String>,
    /// Whether each bridge frame is stored raw beside what it decoded to.
    pub record_raw: bool,
    /// The app's build, recorded as the capture's provenance.
    pub build: Option<HostBuild>,
}

/// Where the rows' positions come from.
#[derive(Debug, Clone, uniffi::Record)]
pub struct PositionSources {
    /// Read the bridge's own receiver.
    pub bridge_receiver: bool,
    /// The platform's location services.
    pub location: Option<Arc<LocationFeed>>,
    /// A typed-in position, which marks the capture as test data.
    pub fixed: Option<Fixed>,
}

/// One view, numbered.
#[derive(Debug, Clone, PartialEq, uniffi::Record)]
pub struct Viewed {
    /// Its place in the session's views, the first 1.
    pub seq: u64,
    pub view: View,
}

/// What a stopped capture wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Record)]
pub struct CaptureEnd {
    /// Rows written.
    pub written: u64,
    /// Rows dropped, by a full queue or a failed batch.
    pub dropped: u64,
}

/// Why a capture did not start.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StartError {
    /// Another session holds the link.
    #[error("another session holds the link")]
    LinkInUse,
    /// The link is closed.
    #[error("the link is closed")]
    LinkClosed,
    /// A file is at `path` already: a capture holds one run.
    #[error("{path} exists")]
    Exists { path: String },
    /// The store could not be created.
    #[error("the store did not open: {detail}")]
    Store { detail: String },
}

/// Why a stop did not end a capture.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum StopError {
    /// The capture was stopped already.
    #[error("the capture was stopped already")]
    AlreadyStopped,
    /// A capture task panicked, so the capture may not have closed out. The link is closed.
    #[error("a capture task panicked")]
    Panicked,
}

/// Why the capture did not take a command.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SteerError {
    /// The engine takes no command: its queue is full, or the capture has stopped.
    #[error("the capture takes no command")]
    Busy,
}

/// A running capture.
#[derive(Debug, uniffi::Object)]
pub struct Session {
    link: Arc<Link>,
    /// The capture, until it is stopped.
    running: Mutex<Option<Running>>,
    controls: Controls,
    views: watch::Receiver<Viewed>,
}

#[uniffi::export]
impl Session {
    /// Start a capture over `link`, which the session holds until it stops.
    ///
    /// # Errors
    /// [`StartError::LinkInUse`] or [`StartError::LinkClosed`] for a link not idle, and
    /// [`StartError::Exists`] or [`StartError::Store`] when the store cannot be created, which
    /// leaves the link as it was. [`StartError::LinkClosed`] too when the start's work panicked,
    /// which takes the link with it.
    #[uniffi::constructor]
    pub async fn start(
        config: CaptureConfig,
        link: Arc<Link>,
        settings: Arc<Settings>,
        position: PositionSources,
        health: Option<Arc<HealthFeed>>,
    ) -> Result<Arc<Self>, StartError> {
        let lent = Arc::clone(&link);
        // On the runtime: the store's creation is disk I/O, kept off the phone's thread.
        match spawn(start(config, link, settings, position, health)).await {
            Ok(started) => started,
            // The link was lent to the work that panicked, and went with it.
            Err(_) => {
                lent.lose();
                Err(StartError::LinkClosed)
            }
        }
    }

    /// The latest view, at once.
    #[must_use]
    pub fn view(&self) -> Viewed {
        self.views.borrow().clone()
    }

    /// The first view past `after` (0 for the first), or `None` once the capture has ended and
    /// no newer view exists.
    pub async fn next_view(&self, after: u64) -> Option<Viewed> {
        let mut views = self.views.clone();
        loop {
            {
                let view = views.borrow_and_update();
                if view.seq > after {
                    return Some(view.clone());
                }
            }
            // Fails only once the stream has ended and its last view is read.
            views.changed().await.ok()?;
        }
    }

    /// Whether the app shows the receiver's sky, which the bridge's receiver is asked for only
    /// while it is shown.
    pub fn show_sky(&self, visible: bool) {
        self.controls.show_sky(visible);
    }

    /// Start the scan without waiting for a GPS fix. Nothing once the wait is over.
    ///
    /// # Errors
    /// [`SteerError::Busy`] when the engine takes no command: its queue is full, or the capture
    /// has stopped.
    pub fn start_without_fix(&self) -> Result<(), SteerError> {
        self.controls.command(Command::StartWithoutFix).map_err(|Busy| SteerError::Busy)
    }

    /// Stop the capture, wait for its last rows to commit and its last view, and give the link
    /// back to its `Link`.
    ///
    /// # Errors
    /// [`StopError::AlreadyStopped`] on a second call, and [`StopError::Panicked`] when a capture
    /// task panicked.
    pub async fn stop(&self) -> Result<CaptureEnd, StopError> {
        let running = self.lock().take().ok_or(StopError::AlreadyStopped)?;
        let link = Arc::clone(&self.link);
        // On the runtime, so a phone that stops waiting still has the link given back.
        match spawn(stop(running, Arc::clone(&link))).await {
            Ok(ended) => ended,
            Err(_) => {
                link.lose();
                Err(StopError::Panicked)
            }
        }
    }
}

impl Session {
    fn lock(&self) -> MutexGuard<'_, Option<Running>> {
        // `take` is the only write, so a panic elsewhere leaves nothing torn.
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let running = self.running.get_mut().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(running) = running {
            let link = Arc::clone(&self.link);
            drop(spawn(async move {
                if let Err(error) = stop(running, link).await {
                    tracing::warn!(%error, "a capture let go of did not stop cleanly");
                }
            }));
        }
    }
}

/// [`Session::start`]'s work, on the library's runtime. The link is lent until the capture holds
/// it, and goes back, parked, when the store refuses it.
async fn start(
    config: CaptureConfig,
    link: Arc<Link>,
    settings: Arc<Settings>,
    position: PositionSources,
    health: Option<Arc<HealthFeed>>,
) -> Result<Arc<Session>, StartError> {
    let (handle, announced) = link.lend().await.map_err(|error| match error {
        LinkError::InUse => StartError::LinkInUse,
        _ => StartError::LinkClosed,
    })?;
    let simulated = link.is_simulated();
    let sources = Sources {
        named: None,
        bridge: position.bridge_receiver.then(|| Gps::bridged(DEFAULT_MAX_AGE)),
        platform: position.location.as_ref().map(|feed| feed.feed()),
        searched: None,
        fixed: position.fixed,
    };
    let settings = settings.app();
    let saved = settings.saved();
    let capture = SessionConfig {
        store: StoreConfig::new(&config.db_path),
        notes: config.notes,
        test_data: simulated || sources.is_test_data(),
        link: (!simulated).then_some(Over::Bluetooth.into()),
        position: sources.chain(DEFAULT_MAX_AGE),
        wait_for_fix: sources.waits_for_fix(saved.gps.waits_for_fix(), simulated),
        record_raw: config.record_raw,
        excluded_ssids: saved.excluded_ssids,
        gps_rate_hz: saved.gps.bridge_rate_hz.unwrap_or(ubx::DEFAULT_RATE_HZ),
        build: config.build,
    };
    let settings = if simulated { settings.simulated() } else { settings.over(Over::Bluetooth) };
    let health = health.map(|feed| feed.reader());
    let capture = match Capture::start(capture, handle, announced.clone(), health, settings) {
        Ok(capture) => capture,
        Err(Refused { error, link: handle }) => {
            link.give_back(*handle, announced);
            return Err(match error {
                StoreError::Exists(path) => StartError::Exists { path: path.display().to_string() },
                error => StartError::Store { detail: chain(&error) },
            });
        }
    };
    // The simulator stands in for a bridge on USB, as the terminal shows it.
    let over = if simulated { Over::Usb } else { Over::Bluetooth };
    let (views, viewing) = follow(&capture, over);
    let controls = capture.controls();
    let running = Mutex::new(Some(Running { capture, viewing }));
    Ok(Arc::new(Session { link, running, controls, views }))
}

/// A capture under way, and the task numbering its views.
#[derive(Debug)]
struct Running {
    capture: Capture,
    viewing: JoinHandle<()>,
}

/// Stop the capture, wait for its last view, and give its link back to `link`, or close `link`
/// when a task panicked.
async fn stop(running: Running, link: Arc<Link>) -> Result<CaptureEnd, StopError> {
    match running.capture.stop().await {
        Ok(end) => {
            link.give_back(end.link, end.bridge);
            // The capture's snapshots have ended, so this task ends on the last of them.
            if let Err(error) = running.viewing.await {
                tracing::warn!(%error, "the view task panicked");
            }
            Ok(CaptureEnd { written: end.report.written, dropped: end.report.dropped })
        }
        Err(_) => {
            link.lose();
            Err(StopError::Panicked)
        }
    }
}

/// The numbered views of `capture`'s snapshots over `over`, the first 1, until its snapshots end,
/// and the task that numbers them.
fn follow(capture: &Capture, over: Over) -> (watch::Receiver<Viewed>, JoinHandle<()>) {
    let mut snapshots = capture.snapshots();
    let first = View::of(&snapshots.borrow_and_update(), over);
    let (sender, views) = watch::channel(Viewed { seq: 1, view: first });
    let viewing = spawn(async move {
        let mut seq = 1;
        while snapshots.changed().await.is_ok() {
            seq += 1;
            let view = View::of(&snapshots.borrow_and_update(), over);
            // A session let go of reads no more.
            if sender.send(Viewed { seq, view }).is_err() {
                break;
            }
        }
    });
    (views, viewing)
}
