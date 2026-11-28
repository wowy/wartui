//! A capture, assembled and run the same way for every front end.
//!
//! **A session owns the capture.** [`Session::start`] creates the store, builds the engine and
//! spawns two tasks: the capture, `wartui_core::runtime::drive`, and the task that follows it.
//! The front end keeps what is its own: which link reaches the bridge and opening it, the position
//! tiers and their order, and the view. It steers the capture through [`Controls`], and reads it
//! through [`Session::snapshots`].
//!
//! **What a capture needs on every snapshot happens whether or not a view is drawing.** The follow
//! task runs [`Controls::follow`] on the first snapshot, on every change, and when a front end
//! shows or hides the receiver's sky: it saves the bridge the transport remembered or forgot and
//! the link a bridge announced itself over, and asks the bridge's receiver for its sky lists only
//! while a front end shows them. A view that stops drawing changes none of it.
//!
//! **[`Session::stop`] is the one way out.** It stops the capture, whose engine stops the scan
//! first, and waits for the store's last batch to commit and the capture's end time to be written,
//! so a stopped capture is a complete database. It then gives the link back, open, with the bridge
//! it still had announced, so a phone can go on to update or start another capture over the one
//! connection: the transport announces a bridge once per life, so the next capture is handed the
//! announcement rather than waiting for one. A start that is refused gives the link back too.
//!
//! **A link heard as incompatible is refused before the store exists.** A bridge whose last `Ready`
//! this host read only for its frozen core takes no capture, so a front end that waited on it is
//! told why ([`Refusal::Incompatible`]) with no capture file left behind, the link dropped since or
//! not (`LinkHandle::heard_incompatible`). A link not yet read, as `run` hands one, has heard
//! nothing: there the capture starts, and its engine shows the fault.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::{JoinError, JoinHandle};
use wartui_bridge::{BridgeCore, BridgeInfo, LinkHandle};
use wartui_core::engine::{Command, EngineConfig, FleetEngine, Snapshot, StoreStats};
use wartui_core::health::HealthReader;
use wartui_core::position::PositionChain;
use wartui_core::record::HostLink;
use wartui_core::runtime::{COMMAND_QUEUE, drive, now};
use wartui_core::store::{
    CaptureInfo, CaptureProvenance, Store, StoreConfig, StoreError, StoreReport,
};

use crate::settings::{
    self, Applied, Busy, SaveError, Settings, SettingsEdit, excluded_ssid_bytes,
};

/// The build a capture records as its provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostBuild {
    /// The front end's package version.
    pub host_version: Option<String>,
    /// The short git hash it was built from.
    pub host_commit: Option<String>,
    /// Its release tag, when it is a tagged build.
    pub release_tag: Option<String>,
}

/// What a capture is started with. The front end decides all of it.
#[derive(Debug)]
pub struct SessionConfig {
    /// Where the capture goes, and the store's tuning.
    pub store: StoreConfig,
    /// The operator's notes on the capture.
    pub notes: Option<String>,
    /// Whether the capture holds test data, which is never uploaded.
    pub test_data: bool,
    /// The link the host reaches the bridge over, or `None` under the simulator.
    pub link: Option<HostLink>,
    /// Where each record is, tier by tier, in the order the front end built them.
    pub position: PositionChain,
    /// Whether the scan waits for a GPS fix before it starts.
    pub wait_for_fix: bool,
    /// Whether each bridge frame is stored raw beside what it decoded to.
    pub record_raw: bool,
    /// The Wi-Fi networks never captured, as the settings hold them.
    pub excluded_ssids: Vec<String>,
    /// The bridge receiver's rate, in hertz.
    pub gps_rate_hz: u8,
    /// The build recorded as the capture's provenance. `None` records none, and no initial
    /// settings either.
    pub build: Option<HostBuild>,
}

/// A running capture.
#[derive(Debug)]
pub struct Session {
    snapshots: watch::Receiver<Arc<Snapshot>>,
    controls: Controls,
    stop: oneshot::Sender<()>,
    capture: JoinHandle<(StoreReport, LinkHandle)>,
    follow: JoinHandle<()>,
}

/// What a stopped capture leaves.
#[derive(Debug)]
pub struct SessionEnd {
    /// What the store's writer did.
    pub report: StoreReport,
    /// The link the capture ran over, still open.
    pub link: LinkHandle,
    /// The bridge the link still had announced when the capture stopped, or `None` when the link
    /// was down. Hand it to the next [`Session::start`] with the link.
    pub bridge: Option<BridgeInfo>,
}

/// A capture that did not start, and the link it would have run over, still open.
#[derive(Debug)]
pub struct Refused {
    /// Why it did not start.
    pub reason: Refusal,
    /// Boxed, since the handle is large and an error travels by value.
    pub link: Box<LinkHandle>,
}

/// Why a capture did not start.
#[derive(Debug)]
pub enum Refusal {
    /// The store could not be created.
    Store(StoreError),
    /// The link's last `Ready` was a bridge's this host reads only for its frozen core: another
    /// major, or another build of this one. No store was created.
    Incompatible(BridgeCore),
}

/// A session's task panicked, so its capture may not have closed out.
#[derive(Debug, thiserror::Error)]
#[error("a session task panicked")]
pub struct Panicked(#[source] JoinError);

impl Session {
    /// Create the store, build the engine, and spawn the capture and the task that follows it.
    /// A capture starts the scan, and stops it when it ends. Call it inside a tokio runtime.
    ///
    /// `announced` is the bridge `link` announced before this capture: `None` for a link not yet
    /// read from, which is what `run` and `bench` hold, and `Some` for one a front end waited on
    /// for its `Ready` (`ops::ready`) or a [`SessionEnd`] gave back. A transport remembers a
    /// bridge only as it connects, so an announced bridge is remembered here when remembering
    /// is on and holds none: the switch turned on after the link connected.
    ///
    /// `health` is read once per host sample for the host's own health; `None` records nothing.
    ///
    /// # Errors
    /// [`Refused`], with the link, when the link was heard as incompatible
    /// ([`Refusal::Incompatible`]) or the store cannot be created ([`Refusal::Store`]). A capture
    /// that exists already is refused.
    pub fn start(
        config: SessionConfig,
        link: LinkHandle,
        announced: Option<BridgeInfo>,
        health: Option<HealthReader>,
        settings: Settings,
    ) -> Result<Self, Refused> {
        if announced.is_none()
            && let Some(&core) = link.heard_incompatible()
        {
            return Err(Refused { reason: Refusal::Incompatible(core), link: Box::new(link) });
        }
        let started = now();
        let info =
            CaptureInfo { notes: config.notes, simulated: config.test_data, link: config.link };
        let engine = FleetEngine::new(
            EngineConfig {
                auto_start: true,
                wait_for_fix: config.wait_for_fix,
                record_raw: config.record_raw,
                position: config.position,
                excluded_ssids: excluded_ssid_bytes(&config.excluded_ssids).collect(),
                gps_rate_hz: config.gps_rate_hz,
                link: info.link,
                ..EngineConfig::default()
            },
            started,
        );
        let store = match config.build {
            Some(build) => {
                let provenance = CaptureProvenance {
                    host_version: build.host_version,
                    host_commit: build.host_commit,
                    release_tag: build.release_tag,
                    initial_settings: Some(engine.capture_settings()),
                };
                Store::create_with_provenance(&config.store, &info, started.unix_ms, &provenance)
            }
            None => Store::create(&config.store, &info, started.unix_ms),
        };
        let store = match store {
            Ok(store) => store,
            Err(error) => {
                return Err(Refused { reason: Refusal::Store(error), link: Box::new(link) });
            }
        };

        if let Some(bridge) = &announced {
            settings.remember_announced(bridge.mac);
        }
        let (snapshot_tx, snapshots) =
            watch::channel(Arc::new(engine.snapshot(started, StoreStats::default())));
        let (stop, stop_rx) = oneshot::channel();
        let (commands, command_rx) = mpsc::channel(COMMAND_QUEUE);
        let controls = Controls::new(commands, settings);
        let capture = tokio::spawn(async move {
            let mut link = link;
            let report = drive(
                &mut link,
                announced,
                store,
                engine,
                snapshot_tx,
                command_rx,
                stop_rx,
                health,
            )
            .await;
            (report, link)
        });
        let follow = tokio::spawn(follow(controls.clone(), snapshots.clone()));
        Ok(Self { snapshots, controls, stop, capture, follow })
    }

    /// The capture's latest snapshot, and each one after it. The channel starts with the
    /// engine's first.
    #[must_use]
    pub fn snapshots(&self) -> watch::Receiver<Arc<Snapshot>> {
        self.snapshots.clone()
    }

    /// What a front end steers the capture with.
    #[must_use]
    pub fn controls(&self) -> Controls {
        self.controls.clone()
    }

    /// Stop the capture, whose engine stops the scan first, wait for the last batch to commit,
    /// and give the link back with the bridge it still has announced.
    ///
    /// # Errors
    /// [`Panicked`] when a session task panicked.
    pub async fn stop(self) -> Result<SessionEnd, Panicked> {
        // A capture whose link closed has ended already, and has nothing to stop.
        let _ = self.stop.send(());
        let (report, link) = self.capture.await.map_err(Panicked)?;
        // The capture dropped its snapshot channel on the way out, which ends this one.
        self.follow.await.map_err(Panicked)?;
        // The capture's last snapshot, which it publishes on the way out.
        let last = self.snapshots.borrow().clone();
        let bridge = last.bridge.clone().filter(|_| last.link_up);
        Ok(SessionEnd { report, link, bridge })
    }
}

/// Follow the capture: [`Controls::follow`] on the first snapshot, on every change and when the
/// sky's visibility changes, until the capture ends.
async fn follow(controls: Controls, mut snapshots: watch::Receiver<Arc<Snapshot>>) {
    loop {
        let snapshot = snapshots.borrow_and_update().clone();
        controls.follow(&snapshot);
        tokio::select! {
            changed = snapshots.changed() => {
                if changed.is_err() {
                    break;
                }
            }
            () = controls.sky.changed.notified() => {}
        }
    }
}

/// What a front end steers a running capture with. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Controls {
    commands: mpsc::Sender<Command>,
    settings: Settings,
    sky: Arc<Sky>,
}

/// Whether a front end shows the receiver's sky, and what the engine was last asked for.
#[derive(Debug, Default)]
struct Sky {
    /// Whether a front end is showing the sky.
    visible: AtomicBool,
    /// What the engine was last told: off, as it starts.
    sent: AtomicBool,
    /// Woken when [`Self::visible`] changes, so the follow task does not wait out a tick.
    changed: Notify,
}

impl Controls {
    /// Controls sending on `commands` and saving through `settings`: a session's, and a front
    /// end's tests', which read the other end of `commands`.
    #[must_use]
    pub fn new(commands: mpsc::Sender<Command>, settings: Settings) -> Self {
        Self { commands, settings, sky: Arc::default() }
    }

    /// Send `command` to the engine.
    ///
    /// # Errors
    /// [`Busy`] when the engine takes no command: its queue is full, or it has stopped.
    pub fn command(&self, command: Command) -> Result<(), Busy> {
        self.commands.try_send(command).map_err(|_| Busy)
    }

    /// Put `edit`'s rows in force and save them ([`settings`]'s rules).
    ///
    /// # Errors
    /// [`Busy`] when the engine takes no command, and then nothing is sent, switched or saved.
    pub fn apply(&self, edit: &SettingsEdit, snapshot: &Snapshot) -> Result<Applied, Busy> {
        settings::apply(edit, snapshot, &self.commands, &self.settings)
    }

    /// Put `ssids` in force as the excluded list, and save only them.
    ///
    /// # Errors
    /// [`Busy`] when the engine takes no command, and then nothing is saved.
    pub fn apply_excluded(&self, ssids: &[String]) -> Result<Result<String, SaveError>, Busy> {
        settings::apply_excluded(ssids, &self.commands, &self.settings)
    }

    /// The settings these controls save through.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Whether a front end is showing the receiver's sky. The session asks the bridge's receiver
    /// for its sky lists only while one is, and only while the receiver read is the bridge's.
    pub fn show_sky(&self, visible: bool) {
        if self.sky.visible.swap(visible, Ordering::Relaxed) != visible {
            self.sky.changed.notify_one();
        }
    }

    /// What the session does on every snapshot: save the bridge and the link as the settings
    /// say, and ask for the bridge receiver's sky lists when what is wanted changed. A full queue
    /// is tried again on the next call.
    pub fn follow(&self, snapshot: &Snapshot) {
        self.settings.follow(snapshot);
        let wanted = self.sky.visible.load(Ordering::Relaxed)
            && snapshot.gps.as_ref().is_some_and(|gps| gps.via_bridge);
        if wanted != self.sky.sent.load(Ordering::Relaxed)
            && self.commands.try_send(Command::SetGpsSky(wanted)).is_ok()
        {
            self.sky.sent.store(wanted, Ordering::Relaxed);
        }
    }
}
