//! The link to the bridge, held between captures.
//!
//! A [`Link`] holds the `LinkHandle` a transport gives, so a phone opens it once and runs several
//! captures over the one connection. It is idle, lent to the [`Session`](crate::session::Session)
//! running over it, or closed. An idle link keeps the bridge it last had announced, since the
//! transport announces a bridge once per life and the next capture is handed the announcement
//! (`wartui_app::session` `//!`).
//!
//! [`Link::bluetooth`] picks the bridge as `wartui run` does over Bluetooth: the bridge named,
//! else the one the settings remember, else the only one heard. The transport remembers into the
//! settings' bridge memory, so a capture saves the bridge it connected to.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use wartui_bridge::ble::BleTransport;
use wartui_bridge::sim::{SimConfig, SimTransport};
use wartui_bridge::spec::BridgeSpec;
use wartui_bridge::{BridgeInfo, LinkHandle};

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
    /// A session holds the link.
    #[error("a session holds the link")]
    Lent,
    /// The link is closed.
    #[error("the link is closed")]
    Closed,
}

/// The link's state.
#[derive(Debug)]
pub(crate) enum Held {
    /// Open, and held by no session, with the bridge it last had announced.
    Idle { handle: Box<LinkHandle>, announced: Option<BridgeInfo> },
    /// A session holds it.
    Lent,
    /// Closed, or lost with a session that panicked.
    Closed,
}

/// The link to a bridge: simulated, or over Bluetooth.
#[derive(Debug, uniffi::Object)]
pub struct Link {
    simulated: bool,
    held: Mutex<Held>,
}

#[uniffi::export]
impl Link {
    /// A simulated bridge with `nodes` nodes. A capture over it is test data.
    #[uniffi::constructor]
    #[must_use]
    pub fn simulated(nodes: u8) -> Arc<Self> {
        let _inside = runtime().enter();
        let config = SimConfig { node_count: nodes, ..SimConfig::default() };
        let held = match SimTransport::new(config).start() {
            Ok(handle) => Held::Idle { handle: Box::new(handle), announced: None },
            Err(error) => {
                tracing::warn!(%error, "the simulator did not start");
                Held::Closed
            }
        };
        Arc::new(Self { simulated: true, held: Mutex::new(held) })
    }

    /// The Bluetooth link to the bridge `bridge` names (a whole address, or its last octets as
    /// `44:C0`), else the one `settings` remember, else the only one heard. The settings remember
    /// the bridge it connects to.
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
            None => None,
        };
        let transport = match spec {
            Some(spec) => BleTransport::with_spec(radio, spec),
            None => BleTransport::new(radio),
        };
        let _inside = runtime().enter();
        let handle = transport
            .remember_in(settings.app().memory())
            .start()
            .map_err(|error| LinkError::Start { detail: error.to_string() })?;
        let held = Held::Idle { handle: Box::new(handle), announced: None };
        Ok(Arc::new(Self { simulated: false, held: Mutex::new(held) }))
    }

    /// Whether the bridge is the simulator's.
    #[must_use]
    pub fn is_simulated(&self) -> bool {
        self.simulated
    }

    /// Close the link, letting the bridge go. Closing a closed link does nothing.
    ///
    /// # Errors
    /// [`LinkError::Lent`] while a session holds it.
    pub async fn close(&self) -> Result<(), LinkError> {
        let handle = {
            let mut held = self.lock();
            match std::mem::replace(&mut *held, Held::Closed) {
                Held::Idle { handle, .. } => handle,
                Held::Lent => {
                    *held = Held::Lent;
                    return Err(LinkError::Lent);
                }
                Held::Closed => return Ok(()),
            }
        };
        // The Bluetooth transport's unsubscribe and disconnect take the runtime's timers.
        let closing = spawn(async move { handle.close().await });
        if let Err(error) = closing.await {
            tracing::warn!(%error, "closing the link panicked");
        }
        Ok(())
    }
}

impl Link {
    /// Take the link for a session: idle to lent.
    pub(crate) fn lend(&self) -> Result<(LinkHandle, Option<BridgeInfo>), LinkError> {
        let mut held = self.lock();
        match std::mem::replace(&mut *held, Held::Lent) {
            Held::Idle { handle, announced } => Ok((*handle, announced)),
            Held::Lent => Err(LinkError::Lent),
            Held::Closed => {
                *held = Held::Closed;
                Err(LinkError::Closed)
            }
        }
    }

    /// Give the link back: lent to idle, with the bridge it still has announced.
    pub(crate) fn give_back(&self, handle: LinkHandle, announced: Option<BridgeInfo>) {
        *self.lock() = Held::Idle { handle: Box::new(handle), announced };
    }

    /// The session holding the link lost it.
    pub(crate) fn lose(&self) {
        *self.lock() = Held::Closed;
    }

    fn lock(&self) -> MutexGuard<'_, Held> {
        // Each state is written whole, so a panic elsewhere leaves nothing torn.
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
