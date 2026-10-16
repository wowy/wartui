//! Which board answered the link protocol last time.
//!
//! **Held in memory, kept by the caller.** The transport reads the address to
//! order its first sweep, writes it on every connection and clears it when the
//! remembered board goes quiet, but it touches no file. Whoever seeds the handle
//! decides whether the value outlives the process: `wartui run` keeps it in
//! `wartui.toml` as `[bridge] mac`, and every other command lets it go at exit.
//! `wartui flash-bridge` writes that key too, for the board it flashed, but opens
//! no link and holds no handle. One owner for the file within a process is what
//! keeps two threads from racing on whole-file saves.
//!
//! **Whether to remember at all is the operator's decision**, made in
//! `wartui.toml` as `[bridge] remember` or from the settings modal. Off holds
//! nothing: turning it off clears the slot, and nothing is recalled or
//! remembered while it stays off. The slot and the switch are shared by every
//! clone of one handle, because the transport and the view each hold one: a
//! change from the view has to reach the next reconnect, and a connection the
//! transport makes has to reach the view that saves it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use wartui_proto::mac::Mac;

/// The bridge's address, shared by every clone of one handle.
#[derive(Debug, Clone)]
pub struct BridgeMemory {
    /// The address remembered, if any. Shared by every clone.
    mac: Arc<Mutex<Option<Mac>>>,
    /// Whether the operator wants the address remembered. Shared by every clone.
    enabled: Arc<AtomicBool>,
}

impl Default for BridgeMemory {
    fn default() -> Self {
        Self::none()
    }
}

impl BridgeMemory {
    /// A handle holding `mac`, with remembering on.
    #[must_use]
    pub fn holding(mac: Option<Mac>) -> Self {
        Self { mac: Arc::new(Mutex::new(mac)), enabled: Arc::new(AtomicBool::new(true)) }
    }

    /// A handle holding nothing, with remembering on.
    #[must_use]
    pub fn none() -> Self {
        Self::holding(None)
    }

    /// Turn remembering on or off for this handle and every clone of it. Off also
    /// clears the address, because off means nothing held.
    pub fn set_enabled(&self, on: bool) {
        self.enabled.store(on, Ordering::Relaxed);
        if !on {
            self.forget();
        }
    }

    /// Whether remembering is on.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Relaxed)
    }

    /// The address held. Nothing while remembering is off.
    #[must_use]
    pub fn recall(&self) -> Option<Mac> {
        if !self.is_enabled() {
            return None;
        }
        *self.slot()
    }

    /// Hold `mac` in place of whatever was there. Nothing changes while
    /// remembering is off.
    pub fn remember(&self, mac: Mac) {
        if self.is_enabled() {
            *self.slot() = Some(mac);
        }
    }

    /// Forget whatever was there, so the next connection sweeps instead.
    pub fn forget(&self) {
        *self.slot() = None;
    }

    /// The slot, even after a panic elsewhere: an address is whole or absent, so a
    /// poisoned lock holds nothing torn.
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<Mac>> {
        self.mac.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
