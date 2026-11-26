//! The link to the bridge, held between captures.
//!
//! A [`Link`] holds the `LinkHandle` a transport gives, so a phone opens it once and runs several
//! captures over the one connection. It is idle, in use by a
//! [`Session`](crate::session::Session) or by [`Link::ready`], or closed. An idle link is parked
//! (`wartui_app::ops::park`): a task reads it, so the bridge's stream never backs up into the next
//! capture, and keeps the bridge it has announced, since the transport announces a bridge once per
//! life and the next capture is handed the announcement (`wartui_app::session` `//!`). Taking a
//! parked link back waits on that task, so lending and closing are async.
//!
//! **[`Link::bluetooth`] pins the link to the bridge named, else to the one the settings remember
//! when it opens.** Its transport refuses every other bridge through each reconnect, so a capture
//! or an update over it reaches that bridge and no other (`update.rs`). Only with neither does it
//! take the only bridge heard, which a capture may and an update may not. This differs from
//! `wartui run`, which takes another bridge in the remembered one's absence: a phone moves to
//! another bridge by naming it.
//!
//! The transport remembers into the settings' bridge memory, so a capture saves the bridge it
//! connected to. The settings read an idle link's announcement for a save ([`Settings::save`]).
//!
//! **[`Link::ready`] waits for the announcement before a capture**, so a phone can show the
//! bridge, or why none answered, before it starts one. It is `wartui_app::ops::ready`, run whole
//! on the library's runtime: the link is taken, waited on and given back, parked with the
//! announcement, on every way out. A phone that stops awaiting ends the wait and has the link back
//! idle, rather than in use until the wait runs out, which a pairing notice stretches. The wait a
//! `Ready` gets once a link is up is `wartui_app::ops::REPLY_TIMEOUT`, as the terminal's commands
//! give it. A link that holds an announcement already answers with it at once.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::oneshot;
use wartui_app::ops::{self, NotReady, Parked, ReadyNotice, park};
use wartui_app::settings::Link as Over;
use wartui_app::view::BridgeView;
use wartui_bridge::ble::BleTransport;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::spec::BridgeSpec;
use wartui_bridge::{BridgeInfo, LinkHandle};
use wartui_proto::mac::Mac;

use crate::log::SinkError;
use crate::radio::Radio;
use crate::runtime::{runtime, spawn};
use crate::settings::Settings;

/// Why a link could not be opened or closed.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum LinkError {
    /// `given` names a USB port, not a bridge: name a bridge by its address or its last octets.
    #[error("{given} names no bridge")]
    NotABridgeName { given: String },
    /// The transport would not start.
    #[error("the link did not start: {detail}")]
    Start { detail: String },
    /// A session or a wait for the bridge holds the link.
    #[error("the link is in use")]
    InUse,
    /// The link is closed.
    #[error("the link is closed")]
    Closed,
}

/// A notice the wait for the bridge's announcement gives as it goes, carrying the transport's
/// own text.
#[uniffi::remote(Enum)]
pub enum ReadyNotice {
    /// The transport waits on the OS's pairing dialog.
    Pairing(String),
    /// The subscription that waited on the pairing is through.
    Through(String),
}

/// What the wait for the bridge's announcement says as it goes.
#[uniffi::export(with_foreign)]
pub trait ReadyListener: Send + Sync {
    /// One notice, on any thread. A listener that fails is ignored.
    fn notice(&self, notice: ReadyNotice) -> Result<(), SinkError>;
}

/// The bridge's announcement.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Announced {
    /// The bridge, as a capture's view shows it.
    pub bridge: BridgeView,
    /// A frame arrived that did not decode: a bridge running firmware from another tree.
    pub garbled: bool,
}

/// Why the wait for the bridge's announcement ended without one.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ReadyError {
    /// The link went down, in the transport's words.
    #[error("the link went down: {detail}")]
    Down { detail: String },
    /// The wait ran out while the transport retried this failure.
    #[error("the transport retried: {detail}")]
    Retried { detail: String },
    /// The wait ran out after the subscription that waited on the pairing went through: the
    /// bridge did not answer.
    #[error("the bridge did not answer after the pairing")]
    SilentAfterPairing,
    /// The wait ran out after the OS's pairing dialog was announced.
    #[error("the pairing wait ran out")]
    PairingRanOut,
    /// The wait ran out with nothing to name. `garbled`: a frame arrived that did not decode, a
    /// bridge running firmware from another tree.
    #[error("no bridge answered")]
    Silent { garbled: bool },
    /// A session or another wait holds the link.
    #[error("the link is in use")]
    InUse,
    /// The link is closed, or its transport ended.
    #[error("the link is closed")]
    Closed,
}

impl From<NotReady> for ReadyError {
    fn from(why: NotReady) -> Self {
        match why {
            NotReady::Down(detail) => Self::Down { detail },
            NotReady::Closed => Self::Closed,
            NotReady::Retried(detail) => Self::Retried { detail },
            NotReady::SilentAfterPairing => Self::SilentAfterPairing,
            NotReady::PairingRanOut => Self::PairingRanOut,
        }
    }
}

/// The link's state.
#[derive(Debug)]
pub(crate) enum Held {
    /// Open, held by no session, and read.
    Idle(Parked),
    /// A session or a wait for the bridge holds it.
    InUse,
    /// Closed, or lost with a task that panicked.
    Closed,
}

/// The link to a bridge: simulated, or over Bluetooth.
#[derive(Debug, uniffi::Object)]
pub struct Link {
    inner: Arc<Inner>,
}

/// A [`Link`]'s state, shared with the work its calls run on the runtime, and read by the
/// [`Settings`] it was opened with.
#[derive(Debug)]
pub(crate) struct Inner {
    simulated: bool,
    /// The bridge the transport was opened for, which it refuses every other for. `None` for the
    /// simulator, and for a Bluetooth link opened with no bridge named or remembered.
    pinned: Option<BridgeSpec>,
    held: Mutex<Held>,
}

#[uniffi::export]
impl Link {
    /// A simulated bridge with `nodes` nodes. A capture over it is test data, and its view's
    /// `link.host` shows the simulator as a bridge on USB, as the terminal does: it describes no
    /// real link.
    #[uniffi::constructor]
    #[must_use]
    pub fn simulated(nodes: u8) -> Arc<Self> {
        let _inside = runtime().enter();
        let config = SimConfig { node_count: nodes, ..SimConfig::default() };
        let held = match SimTransport::new(config).start() {
            Ok(handle) => Held::Idle(park(handle)),
            Err(error) => {
                tracing::warn!(%error, "the simulator did not start");
                Held::Closed
            }
        };
        Self::holding(true, None, held)
    }

    /// The Bluetooth link to the bridge `bridge` names (a whole address, or its last octets as
    /// `44:C0`), else the one `settings` remember now, pinned to it whatever the settings do
    /// afterwards. With neither it takes the only bridge heard and takes no update. The settings
    /// remember the bridge it connects to.
    ///
    /// # Errors
    /// [`LinkError::NotABridgeName`] when `bridge` names no bridge, and [`LinkError::Start`]
    /// when the transport does not start.
    #[uniffi::constructor]
    pub fn bluetooth(
        radio: Arc<Radio>,
        settings: Arc<Settings>,
        bridge: Option<String>,
    ) -> Result<Arc<Self>, LinkError> {
        let spec = match bridge.map(|given| given.parse::<BridgeSpec>()) {
            Some(Ok(BridgeSpec::Path(given))) => return Err(LinkError::NotABridgeName { given }),
            Some(Ok(spec)) => Some(spec),
            Some(Err(never)) => match never {},
            // The address alone: a save that turns remembering on holds it before a connection
            // has stored the Bluetooth one.
            None => settings.app().memory().recall().map(BridgeSpec::Mac),
        };
        let transport = match spec.clone() {
            Some(spec) => BleTransport::with_spec(radio, spec),
            None => BleTransport::new(radio),
        };
        let _inside = runtime().enter();
        let handle = transport
            .remember_in(settings.app().memory())
            .start()
            .map_err(|error| LinkError::Start { detail: error.to_string() })?;
        let link = Self::holding(false, spec, Held::Idle(park(handle)));
        settings.attach(&link.inner);
        Ok(link)
    }

    /// Whether the bridge is the simulator's.
    #[must_use]
    pub fn is_simulated(&self) -> bool {
        self.inner.simulated
    }

    /// Wait up to `within_ms` for the bridge to announce itself, and keep the announcement for
    /// the next capture. The link is in use while this runs. Each notice goes to `listener`. A
    /// phone that stops awaiting ends the wait and has the link back idle.
    ///
    /// # Errors
    /// [`ReadyError::InUse`] or [`ReadyError::Closed`] for a link not idle, and otherwise why
    /// the wait ended without an announcement.
    pub async fn ready(
        &self,
        within_ms: u32,
        listener: Option<Arc<dyn ReadyListener>>,
    ) -> Result<Announced, ReadyError> {
        let inner = Arc::clone(&self.inner);
        let within = Duration::from_millis(u64::from(within_ms));
        // Dropped with this future, which ends the wait.
        let (_awaited, dropped) = oneshot::channel::<()>();
        // On the runtime, so a phone that stops awaiting still has the link given back.
        match spawn(Arc::clone(&inner).ready(within, listener, dropped)).await {
            Ok(waited) => waited,
            // The link was in the work that panicked, and went with it.
            Err(_) => {
                inner.lose();
                Err(ReadyError::Closed)
            }
        }
    }

    /// Close the link, letting the bridge go. Closing a closed link does nothing.
    ///
    /// # Errors
    /// [`LinkError::InUse`] while a session or a wait for the bridge holds it.
    pub async fn close(&self) -> Result<(), LinkError> {
        let parked = {
            let mut held = self.inner.lock();
            match std::mem::replace(&mut *held, Held::Closed) {
                Held::Idle(parked) => parked,
                Held::InUse => {
                    *held = Held::InUse;
                    return Err(LinkError::InUse);
                }
                Held::Closed => return Ok(()),
            }
        };
        // The Bluetooth transport's unsubscribe and disconnect take the runtime's timers.
        let closing = spawn(async move {
            match parked.resume().await {
                Ok((handle, _)) => handle.close().await,
                // The link went with the task.
                Err(error) => tracing::warn!(%error, "the idle link was lost"),
            }
        });
        if let Err(error) = closing.await {
            tracing::warn!(%error, "closing the link panicked");
        }
        Ok(())
    }
}

impl Link {
    fn holding(simulated: bool, pinned: Option<BridgeSpec>, held: Held) -> Arc<Self> {
        let inner = Inner { simulated, pinned, held: Mutex::new(held) };
        Arc::new(Self { inner: Arc::new(inner) })
    }

    /// The link's state, for work run on the runtime.
    pub(crate) fn shared(&self) -> Arc<Inner> {
        Arc::clone(&self.inner)
    }

    /// Take the link for a session: idle to in use, with the bridge it has announced now. A link
    /// whose parking task panicked is closed.
    pub(crate) async fn lend(&self) -> Result<(LinkHandle, Option<BridgeInfo>), LinkError> {
        self.inner.lend().await
    }

    /// Give the link back, parked: in use to idle, with the bridge the handle holds announced.
    pub(crate) fn give_back(&self, handle: LinkHandle) {
        self.inner.give_back(handle);
    }

    /// The session holding the link lost it.
    pub(crate) fn lose(&self) {
        self.inner.lose();
    }
}

impl Inner {
    /// Whether the bridge is the simulator's.
    pub(crate) fn is_simulated(&self) -> bool {
        self.simulated
    }

    /// Whether an update may go over the link: the simulator's, or a Bluetooth link pinned to the
    /// bridge named or remembered when it was opened.
    pub(crate) fn takes_updates(&self) -> bool {
        self.simulated || self.pinned.is_some()
    }

    /// The address the bridge of an idle link has announced, read without taking the link;
    /// `None` while in use or closed.
    pub(crate) fn announced_mac(&self) -> Option<Mac> {
        match &*self.lock() {
            Held::Idle(parked) => parked.announced().map(|bridge| bridge.mac),
            Held::InUse | Held::Closed => None,
        }
    }

    pub(crate) async fn lend(&self) -> Result<(LinkHandle, Option<BridgeInfo>), LinkError> {
        let parked = {
            let mut held = self.lock();
            match std::mem::replace(&mut *held, Held::InUse) {
                Held::Idle(parked) => parked,
                Held::InUse => return Err(LinkError::InUse),
                Held::Closed => {
                    *held = Held::Closed;
                    return Err(LinkError::Closed);
                }
            }
        };
        parked.resume().await.map_err(|error| {
            tracing::warn!(%error, "the idle link was lost");
            self.lose();
            LinkError::Closed
        })
    }

    pub(crate) fn give_back(&self, handle: LinkHandle) {
        let _inside = runtime().enter();
        *self.lock() = Held::Idle(park(handle));
    }

    pub(crate) fn lose(&self) {
        *self.lock() = Held::Closed;
    }

    /// [`Link::ready`]'s work, on the library's runtime. `dropped` resolves once the phone stops
    /// awaiting, which ends the wait with the link given back idle.
    async fn ready(
        self: Arc<Self>,
        within: Duration,
        listener: Option<Arc<dyn ReadyListener>>,
        dropped: oneshot::Receiver<()>,
    ) -> Result<Announced, ReadyError> {
        let (mut handle, announced) = self.lend().await.map_err(|error| match error {
            LinkError::InUse => ReadyError::InUse,
            _ => ReadyError::Closed,
        })?;
        if let Some(bridge) = announced {
            self.give_back(handle);
            return Ok(Announced { bridge: BridgeView::of(&bridge), garbled: false });
        }
        // The simulator stands in for a bridge on USB, as a capture over it does.
        let over = if self.simulated { Over::Usb } else { Over::Bluetooth };
        let mut notice = |notice: ReadyNotice| {
            if let Some(listener) = &listener {
                // A listener that fails is the phone's to notice; the wait goes on.
                let _ = listener.notice(notice);
            }
        };
        // `ops::ready` is safe to drop mid-wait: the handle keeps the `Connected` it reads, and the
        // parked link one that arrives after.
        let waited = tokio::select! {
            biased;
            _ = dropped => None,
            waited = ops::ready(&mut handle, over, within, ops::REPLY_TIMEOUT, &mut notice) => {
                Some(waited)
            }
        };
        self.give_back(handle);
        // Nobody reads the result of a wait the phone stopped awaiting.
        let Some(waited) = waited else { return Err(ReadyError::Closed) };
        match waited.ready {
            Ok(Some(bridge)) => {
                Ok(Announced { bridge: BridgeView::of(&bridge), garbled: waited.garbled })
            }
            Ok(None) => Err(ReadyError::Silent { garbled: waited.garbled }),
            Err(why) => Err(why.into()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        // Each state is written whole, so a panic elsewhere leaves nothing torn.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
