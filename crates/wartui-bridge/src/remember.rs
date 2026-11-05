//! Which board answered the link protocol last time, and on which port.
//!
//! **Two values: the bridge's address and its CH343's serial number.** The address is the
//! bridge's identity, which `Ready` carries. The serial number is how the port is found again
//! without asking every CH343, since the OS reports it before anything is opened.
//!
//! **Held in memory, kept by the caller.** The transport reads the serial number to
//! order its first sweep, writes both on every connection and clears them when the
//! remembered board goes quiet, but it touches no file. Whoever seeds the handle
//! decides whether the values outlive the process: `wartui run` keeps them in
//! `wartui.toml` as `[bridge] mac` and `serial`, and every other command lets them go at exit.
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

/// The bridge's address and port, shared by every clone of one handle.
#[derive(Debug, Clone)]
pub struct BridgeMemory {
    /// What is remembered, if anything. Shared by every clone.
    slot: Arc<Mutex<Slot>>,
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
        let slot = Slot { mac, serial: None };
        Self { slot: Arc::new(Mutex::new(slot)), enabled: Arc::new(AtomicBool::new(true)) }
    }

    /// The same handle, holding `serial` as the remembered bridge's CH343 serial number.
    #[must_use]
    pub fn on_port(self, serial: Option<String>) -> Self {
        self.slot().serial = serial;
        self
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
        self.slot().mac
    }

    /// The CH343 serial number held. Nothing while remembering is off.
    #[must_use]
    pub fn recall_serial(&self) -> Option<String> {
        if !self.is_enabled() {
            return None;
        }
        self.slot().serial.clone()
    }

    /// Whether this handle recalls exactly `mac` and `serial`, compared under the lock rather
    /// than cloned out. Nothing held while remembering is off, as [`Self::recall`] says.
    #[must_use]
    pub fn matches(&self, mac: Option<Mac>, serial: Option<&str>) -> bool {
        if !self.is_enabled() {
            return mac.is_none() && serial.is_none();
        }
        let slot = self.slot();
        slot.mac == mac && slot.serial.as_deref() == serial
    }

    /// Hold `mac`, answering on the CH343 with `serial`, in place of whatever was there.
    /// Nothing changes while remembering is off.
    ///
    /// No `serial` for the address already held keeps the port held with it: a caller that
    /// does not know the port has not learned it moved. For another address it holds no port,
    /// since the old one would put the wrong board first.
    pub fn remember_on(&self, mac: Mac, serial: Option<String>) {
        if !self.is_enabled() {
            return;
        }
        let mut slot = self.slot();
        let serial = match serial {
            None if slot.mac == Some(mac) => slot.serial.take(),
            serial => serial,
        };
        *slot = Slot { mac: Some(mac), serial };
    }

    /// Forget whatever was there, so the next connection sweeps instead.
    pub fn forget(&self) {
        *self.slot() = Slot::default();
    }

    /// The slot, even after a panic elsewhere: each value is written whole, so a
    /// poisoned lock holds nothing torn.
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What one handle and its clones remember.
#[derive(Debug, Default)]
struct Slot {
    mac: Option<Mac>,
    serial: Option<String>,
}
