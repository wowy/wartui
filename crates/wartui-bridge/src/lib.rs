//! The host side of the link to the USB ESP-NOW bridge.
//!
//! Everything above this crate talks to a [`LinkHandle`] and never learns
//! whether the frames came from a real dongle or from [`sim`]. That is what
//! lets the fleet engine and the TUI be built and tested with nothing plugged
//! in.
//!
//! It also owns the host's serial ports generally, in [`ports`]: what is attached
//! and what the OS says it is, with no judgement about which of them is a bridge.
//! Keeping that in one place is what lets anything else that opens a device share
//! the enumeration rather than write a second one.

pub mod ports;
pub mod serial;
pub mod sim;

use thiserror::Error;
use tokio::sync::mpsc;
use wartui_proto::link::{BridgeToHost, Chip, HostToBridge, LinkError, LoopPhase, Mac, ResetCause};

/// Inbound event queue depth.
///
/// Bounded on purpose: if the engine ever wedges, we want the reader to stop
/// pulling bytes rather than grow without limit.
pub const EVENT_CAPACITY: usize = 4096;

/// Depth of the urgent command queue, which carries channel assignments.
///
/// Small, because anything queued here is racing a node's 100 ms admin window
/// and a deep backlog would be stale by the time it went out.
pub const URGENT_CAPACITY: usize = 64;

/// Depth of the bulk command queue: status polls, peer setup, resets.
pub const BULK_CAPACITY: usize = 512;

/// What the bridge announced about itself when it came up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    /// Which chip the firmware is running on.
    pub chip: Chip,
    /// The bridge's MAC, which nodes see as the core's address.
    pub mac: Mac,
    /// Bridge firmware version.
    pub fw_version: String,
    /// Why the bridge is running this life rather than the last one. Carried up here
    /// rather than logged and dropped, because a bridge restarts underneath a host
    /// that has no other way to tell.
    pub reset_cause: ResetCause,
    /// Where the previous life stopped, when the reset preserved it.
    pub last_phase: LoopPhase,
    /// Bytes free in the radio blobs' heap at the moment it announced.
    pub heap_free: u32,
    /// How long the bridge had been up when it announced itself.
    ///
    /// Small means this connection is talking to a bridge that has just restarted;
    /// see `serial::is_a_new_life`.
    pub uptime_ms: u32,
}

/// Something that happened on the link.
// `Message` is much larger than the other variants because it carries an
// inline ESP-NOW payload. Boxing it would mean an allocation for every frame
// received, and buys nothing: tokio's channels allocate blocks as messages
// arrive rather than reserving the full capacity up front, so an idle queue
// costs nothing either way.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A bridge came up and its protocol version was accepted.
    Connected(BridgeInfo),
    /// The link went away. The transport keeps trying to reconnect.
    Disconnected {
        /// Why, in terms fit to show a user.
        reason: String,
    },
    /// A decoded message from the bridge.
    Message(BridgeToHost),
    /// A frame arrived that could not be decoded. Counted rather than fatal:
    /// resets and cable events produce these, and the framing recovers.
    Garbled(LinkError),
}

/// Why the link could not be established or used.
#[derive(Debug, Error)]
pub enum TransportError {
    /// No serial port looked like an Espressif device.
    #[error("no bridge found; name one with --bridge")]
    NoBridgeFound,
    /// A bridge was named and nothing attached answers to that name.
    #[error("no board attached is {spec}")]
    NoSuchBridge {
        /// The path or address that was asked for.
        spec: String,
    },
    /// Opening the port failed.
    #[error("could not open {port}: {source}")]
    Open {
        /// The port path.
        port: String,
        /// The underlying failure.
        #[source]
        source: serialport::Error,
    },
    /// Enumerating serial ports failed.
    #[error("could not list serial ports: {0}")]
    Enumerate(#[source] serialport::Error),
    /// The bridge speaks a different revision of the link protocol.
    #[error("{0}")]
    Protocol(LinkError),
}

/// Why a command could not be queued.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SendError {
    /// The transport has shut down.
    #[error("link closed")]
    Closed,
    /// The queue is full. For urgent commands this means the bridge is not
    /// keeping up and the assignment should be retried on the next heartbeat
    /// rather than blocking the engine.
    #[error("command queue full")]
    Full,
}

/// The engine's end of the link.
#[derive(Debug)]
pub struct LinkHandle {
    events: mpsc::Receiver<LinkEvent>,
    urgent: mpsc::Sender<HostToBridge>,
    bulk: mpsc::Sender<HostToBridge>,
}

impl LinkHandle {
    /// Wait for the next event, or `None` once the transport has stopped.
    pub async fn recv(&mut self) -> Option<LinkEvent> {
        self.events.recv().await
    }

    /// Queue a command that is racing a node's admin window.
    ///
    /// Never blocks: a full queue means the bridge is behind, and the engine
    /// must keep running rather than wait on it.
    ///
    /// # Errors
    /// [`SendError`] if the queue is full or the link has closed.
    pub fn send_urgent(&self, cmd: HostToBridge) -> Result<(), SendError> {
        Self::try_send(&self.urgent, cmd)
    }

    /// Queue a command that can wait behind urgent traffic.
    ///
    /// # Errors
    /// [`SendError`] if the queue is full or the link has closed.
    pub fn send_bulk(&self, cmd: HostToBridge) -> Result<(), SendError> {
        Self::try_send(&self.bulk, cmd)
    }

    fn try_send(tx: &mpsc::Sender<HostToBridge>, cmd: HostToBridge) -> Result<(), SendError> {
        tx.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Full,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })
    }
}

/// The transport's end of the same channels.
#[derive(Debug)]
pub(crate) struct LinkPlumbing {
    pub events: mpsc::Sender<LinkEvent>,
    pub commands: CommandRx,
}

/// Build a connected pair of endpoints.
pub(crate) fn link_pair() -> (LinkHandle, LinkPlumbing) {
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (urgent_tx, urgent_rx) = mpsc::channel(URGENT_CAPACITY);
    let (bulk_tx, bulk_rx) = mpsc::channel(BULK_CAPACITY);
    (
        LinkHandle { events: event_rx, urgent: urgent_tx, bulk: bulk_tx },
        LinkPlumbing { events: event_tx, commands: CommandRx { urgent: urgent_rx, bulk: bulk_rx } },
    )
}

/// Receives commands, always preferring urgent ones.
///
/// Without this bias a burst of status polls can queue ahead of a channel
/// assignment and push it past the node's 100 ms admin window, costing a whole
/// sweep. It is the concrete mechanism protecting that deadline.
#[derive(Debug)]
pub(crate) struct CommandRx {
    urgent: mpsc::Receiver<HostToBridge>,
    bulk: mpsc::Receiver<HostToBridge>,
}

impl CommandRx {
    /// Next command, urgent first. `None` once both senders are dropped.
    pub async fn recv(&mut self) -> Option<HostToBridge> {
        tokio::select! {
            biased;
            Some(cmd) = self.urgent.recv() => Some(cmd),
            Some(cmd) = self.bulk.recv() => Some(cmd),
            else => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn urgent_commands_overtake_a_backlog_of_bulk_ones() {
        // Without this bias a burst of status polls can queue ahead of a channel
        // assignment and push it past the node's 100 ms admin window.
        let (handle, mut plumbing) = link_pair();
        for channel in 0..32 {
            handle.send_bulk(HostToBridge::SetChannel { channel }).expect("queued");
        }
        handle.send_urgent(HostToBridge::Reset).expect("queued");

        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::Reset));
    }

    #[tokio::test]
    async fn bulk_commands_still_drain_once_urgent_is_empty() {
        let (handle, mut plumbing) = link_pair();
        handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        handle.send_urgent(HostToBridge::Reset).expect("queued");

        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::Reset));
        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::GetStatus));
    }

    #[tokio::test]
    async fn commands_stop_when_the_handle_is_dropped() {
        let (handle, mut plumbing) = link_pair();
        drop(handle);
        assert_eq!(plumbing.commands.recv().await, None);
    }

    #[tokio::test]
    async fn a_full_urgent_queue_is_reported_rather_than_blocking() {
        // The engine must keep running: a missed assignment is retried on the
        // next heartbeat, but a stalled engine misses everything.
        let (handle, _plumbing) = link_pair();
        for _ in 0..URGENT_CAPACITY {
            handle.send_urgent(HostToBridge::Reset).expect("queued");
        }
        assert_eq!(handle.send_urgent(HostToBridge::Reset), Err(SendError::Full));
    }

    #[tokio::test]
    async fn sending_after_shutdown_reports_closed() {
        let (handle, plumbing) = link_pair();
        drop(plumbing);
        assert_eq!(handle.send_urgent(HostToBridge::Reset), Err(SendError::Closed));
    }
}
