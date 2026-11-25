//! The host side of the link to the P4 bridge.
//!
//! Everything above this crate talks to a [`LinkHandle`] and never learns
//! whether the frames came from a real dongle or from [`sim`]. That is what
//! lets the fleet engine and the TUI be built and tested with nothing plugged
//! in.
//!
//! A real bridge is reached two ways: over USB through its CH343 ([`serial`]), or over Bluetooth
//! through its C6 ([`ble`]). A Bluetooth scan finds a bridge by the id it advertises, its last two
//! octets, and never by its name, which is a label for a person and free to change. Both admit
//! what the bridge sends through one rule, that a connection starts at its first `Ready`
//! (`gate`). They differ where the links do. Both ask with `Identify`
//! until a `Ready` answers. USB also sends a keepalive, since the P4's USB gate closes after 15 s
//! of silence; a Bluetooth subscription is the attachment and that gate never closes on silence,
//! so Bluetooth sends none. A Bluetooth write is a fragment of the stream, so each connection's
//! first starts with a `0x00` to end any partial frame. And over Bluetooth, the P4's notice that a
//! USB host took the bridge reads as the link going down, with the central still subscribed for
//! the handback.
//!
//! **A Bluetooth link is closed before the process exits.** The bridge serves a central for as
//! long as it stays subscribed, and a process that exits with the link up leaves BlueZ connected
//! to it, so the bridge keeps serving a host that is gone. Unsubscribing and disconnecting are
//! async calls, which a runtime shutting down never finishes. So every Bluetooth transport counts
//! itself running until its supervisor returns, [`LinkHandle::close`] waits for its own, and
//! [`close_all`] waits for every one before `main` returns, so no command path has to remember
//! to. Serial and the simulator close at once.
//!
//! **Every command a transport takes is reported done exactly once**, written or dropped
//! ([`CommandRx::done`], [`CommandRx::discard`]). [`LinkHandle::written_count`] is that count and
//! [`LinkHandle::backlog`] what is queued past it, so a sender that waits on it ([`pace`]) waits
//! for the link rather than a timer, and never hangs on a command the transport let go. Serial
//! is done with a command once its writer thread has written it, or at the connection's end for
//! one it never wrote, the simulator once it has handled one, and Bluetooth once the write group
//! carrying it is answered or has failed. Bluetooth also drops
//! what was queued before a connection's `Ready`, as the gate drops what decodes before it: a
//! command queued during a gap would reach the next connection stale. USB keeps it, since
//! `wartui reset` writes before any `Ready`. A transport outside this crate builds its link with
//! [`link_pair`] or [`link_pair_awaited`].
//!
//! It also owns the host's serial ports generally, in [`ports`]: what is attached
//! and what the OS says it is, with no judgment about which of them is a bridge.
//! Keeping that in one place is what lets anything else that opens a device share
//! the enumeration rather than write a second one.

pub mod ble;
#[cfg(target_os = "linux")]
pub mod bluez;
mod gate;
pub mod pace;
pub mod ports;
pub mod remember;
pub mod serial;
pub mod sim;
pub mod spec;
pub mod wire;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use thiserror::Error;
use tokio::sync::{mpsc, watch};
use wartui_proto::bus::NodeBoard;
use wartui_proto::link::{BridgeToHost, HostToBridge, LinkError, LoopPhase, RadioInfo};
use wartui_proto::mac::Mac;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::reset::ResetCause;

/// Inbound event queue depth.
///
/// Bounded on purpose: if the engine ever wedges, we want the reader to stop
/// pulling bytes rather than grow without limit.
pub const EVENT_CAPACITY: usize = 4096;

/// Depth of the urgent command queue, which carries channel assignments.
///
/// Small, because an assignment is answered by the next heartbeat and a deep backlog would
/// be stale by the time it went out.
pub const URGENT_CAPACITY: usize = 64;

/// Depth of the bulk command queue: status polls, update records, resets.
pub const BULK_CAPACITY: usize = 512;

/// The longest [`LinkHandle::close`] and [`close_all`] wait for a Bluetooth link to unsubscribe
/// and disconnect. The unsubscribe takes milliseconds, and frees the bridge for another host.
/// The disconnect takes BlueZ 2 s, the timer its `Device1.Disconnect` runs before it drops a
/// link, and BlueZ abandons it if the caller exits first: on D0:1C a 2 s wait left the PC
/// connected after five of six `status` runs. A transport still connecting takes longer, and is
/// left to the runtime's shutdown.
pub const CLOSE_WITHIN: Duration = Duration::from_secs(4);

/// What the bridge announced about itself when it came up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeInfo {
    /// The bridge's MAC: its C6's, which names the bridge.
    pub mac: Mac,
    /// Bridge firmware version.
    pub fw_version: String,
    /// Why the bridge is running this life rather than the last one. Carried up here
    /// rather than logged and dropped, because a bridge restarts underneath a host
    /// that has no other way to tell.
    pub reset_cause: ResetCause,
    /// Where the previous life stopped, when the reset preserved it.
    pub last_phase: LoopPhase,
    /// How long the bridge had been up when it announced itself.
    ///
    /// Small means this connection is talking to a bridge that has just restarted;
    /// see `gate::is_a_new_life`.
    pub uptime_ms: u32,
    /// What carries the nodes, which names their slots for the operator.
    pub node_board: NodeBoard,
    /// The C6 behind the P4, as it described itself. Its reset cause is the only record of
    /// a radio crash under a running bridge. `None` until it answers.
    pub radio: Option<RadioInfo>,
    /// The digest appended to the P4's booted image. `wartui flash-bridge` reads it to tell an
    /// installed image from a rollback. `None` when the P4 cannot read one: its partition
    /// table did not read, or the image carries no appended digest.
    pub image: Option<[u8; DIGEST_LEN]>,
}

/// Something that happened on the link.
// `Message` is much larger than the other variants because it carries an
// inline node record. Boxing it would mean an allocation for every frame
// received, and buys nothing: tokio's channels allocate blocks as messages
// arrive rather than reserving the full capacity up front, so an idle queue
// costs nothing either way.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkEvent {
    /// A bridge came up and its protocol version was accepted.
    Connected(BridgeInfo),
    /// The link went away, or is up and not serving this host ([`DisconnectCause`] says which).
    /// The transport keeps trying to reconnect.
    Disconnected {
        /// Why, in terms fit to show a user.
        reason: String,
        /// What the transport knows of why, for a caller that acts on it. Nothing reads
        /// `reason`'s words for this.
        cause: DisconnectCause,
    },
    /// A decoded message from the bridge.
    Message(BridgeToHost),
    /// A frame arrived that could not be decoded. Counted rather than fatal:
    /// resets and cable events produce these, and the framing recovers.
    Garbled(LinkError),
}

/// Why a link is down, as far as the transport can tell. Only the Bluetooth transport names a
/// cause other than [`DisconnectCause::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectCause {
    /// Nothing the transport can name: a cable, range, a port or a connect that failed, a reset.
    Other,
    /// A USB host took the bridge from this Bluetooth central, which stays subscribed for the
    /// handback.
    UsbTookBridge,
    /// The subscription waits on the OS, which can show a pairing dialog. The transport is
    /// still connected and waiting, and a `Connected` or another `Disconnected` follows.
    AwaitingPairing,
    /// The subscription that waited on the OS's pairing is through, and the bridge has not
    /// announced itself: a USB host may hold it. The transport stays subscribed, and a
    /// `Connected` or another `Disconnected` follows.
    AwaitingReady,
    /// The bridge refused this host's subscription or write request with an ATT authentication or
    /// encryption error: it holds no bond for this host. [`TransportError::NotBonded`] words it.
    NotBonded,
    /// The bridge dropped this host before any `Ready`, when the C6 drops a central it holds no
    /// bond for. A P4 reset or range lost at that moment looks the same, so it proves nothing.
    /// [`TransportError::MaybeNotBonded`] words it.
    MaybeNotBonded,
}

/// What the host sees of a P4 with no firmware, and where its first flash is described.
///
/// Here rather than in `wartui` because [`TransportError::NoBridgeFound`] says it too, and this
/// crate sits below that one.
pub const NO_FIRMWARE_P4: &str = "A P4 with no firmware is a CH343 that does not answer; see \
     firmware/p4-bridge/README.md § \"First flash\".";

/// Why the link could not be established or used.
#[derive(Debug, Error)]
pub enum TransportError {
    /// No CH343 is attached. A bridge on any other port is reached only by its path.
    #[error(
        "name a bridge on any other port by its path, --bridge PATH: no bridge found, as no \
         CH343 is attached. {}",
        NO_FIRMWARE_P4
    )]
    NoBridgeFound,
    /// A bridge was named by its address, and no attached CH343 answered with it.
    #[error("no attached bridge answered as {spec}")]
    NoSuchBridge {
        /// The address that was asked for.
        spec: String,
    },
    /// Several CH343s are attached and nothing says which of them is the bridge.
    #[error(
        "several CH343s are attached and none is known to be the bridge ({boards}); \
             name one with --bridge"
    )]
    AmbiguousBridge {
        /// The CH343s that were found, by path.
        boards: String,
    },
    /// A bridge was named by the end of its address, and several bridges answered ending that
    /// way.
    #[error("several attached bridges end with {spec} ({boards}); name one by more of its address")]
    AmbiguousName {
        /// The octets that were asked for.
        spec: String,
        /// The bridges that end with them.
        boards: String,
    },
    /// The OS refused the port.
    #[error(
        "could not open {port}: permission denied. A serial port belongs to the \
             `dialout` group: `sudo usermod -aG dialout $USER`, then log in again"
    )]
    Forbidden {
        /// The port path.
        port: String,
    },
    /// Opening the port failed.
    #[error("could not open {port}: {source}")]
    Open {
        /// The port path.
        port: String,
        /// The OS's reason.
        #[source]
        source: std::io::Error,
    },
    /// Enumerating serial ports failed.
    #[error("could not list serial ports: {0}")]
    Enumerate(#[source] std::io::Error),
    /// The bridge speaks a different revision of the link protocol.
    #[error("{0}")]
    Protocol(LinkError),
    /// The OS has no Bluetooth adapter to use.
    #[error(
        "no Bluetooth adapter is available: check this PC has one, that it is on, and that its \
         Bluetooth service is running"
    )]
    NoAdapter,
    /// No bridge the spec names advertised its id within the scan.
    #[error(
        "{} over Bluetooth in {secs}s; check the bridge is powered and in range",
        not_advertised(.bridge.as_deref())
    )]
    NotFound {
        /// The bridge scanned for: its octets, or the spec as written when it gives one octet.
        /// `None` for any bridge.
        bridge: Option<String>,
        /// How long the scan ran.
        secs: u64,
    },
    /// Several bridges advertise, and nothing says which is meant.
    #[error("several bridges advertise over Bluetooth ({bridges}); name one with --bridge XX:YY")]
    SeveralAdvertising {
        /// Their octets, `XX:YY`.
        bridges: String,
    },
    /// The bridge refused this host's link, which is not encrypted with a bond it holds.
    #[error(
        "the bridge {bridge} refused this PC over Bluetooth because they are not paired; pair \
         once with `wartui bluetooth pair --bridge {bridge}`"
    )]
    NotBonded {
        /// The bridge, as its last two octets.
        bridge: String,
    },
    /// The bridge dropped this host before serving it, when it drops a host it holds no bond for.
    #[error(
        "if this PC is not paired, pair once with `wartui bluetooth pair --bridge {bridge}`: the \
         bridge {bridge} dropped this PC over Bluetooth before serving it, as it drops a PC it is \
         not paired with"
    )]
    MaybeNotBonded {
        /// The bridge, as its last two octets.
        bridge: String,
    },
    /// The Bluetooth stack failed, in its own words.
    #[error("Bluetooth: {0}")]
    Bluetooth(String),
}

/// What [`TransportError::NotFound`] says did not advertise: the `bridge` named, or any.
fn not_advertised(bridge: Option<&str>) -> String {
    bridge.map_or_else(
        || "no bridge advertised".to_owned(),
        |bridge| format!("bridge {bridge} did not advertise"),
    )
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
    /// Commands accepted by either queue.
    queued: AtomicU64,
    /// Commands the transport is done with.
    written: watch::Receiver<u64>,
    /// Ends when a transport that must finish before exit has finished; `None` for the rest.
    running: Option<Finished>,
}

impl LinkHandle {
    /// Close the link, and wait up to [`CLOSE_WITHIN`] for the transport to let the bridge go.
    /// Over Bluetooth that is the unsubscribe and the disconnect; over USB and in the simulator
    /// it is at once.
    pub async fn close(self) {
        let Self { events, urgent, bulk, running, .. } = self;
        drop((events, urgent, bulk));
        if let Some(running) = running {
            let _ = tokio::time::timeout(CLOSE_WITHIN, running.wait()).await;
        }
    }

    /// Wait for the next event, or `None` once the transport has stopped.
    pub async fn recv(&mut self) -> Option<LinkEvent> {
        self.events.recv().await
    }

    /// Queue a command that answers a node's heartbeat.
    ///
    /// Never blocks: a full queue means the bridge is behind, and the engine
    /// must keep running rather than wait on it.
    ///
    /// # Errors
    /// [`SendError`] if the queue is full or the link has closed.
    pub fn send_urgent(&self, cmd: HostToBridge) -> Result<(), SendError> {
        self.try_send(&self.urgent, cmd)
    }

    /// Queue a command that can wait behind urgent traffic.
    ///
    /// # Errors
    /// [`SendError`] if the queue is full or the link has closed.
    pub fn send_bulk(&self, cmd: HostToBridge) -> Result<(), SendError> {
        self.try_send(&self.bulk, cmd)
    }

    /// How many commands either queue has accepted.
    #[must_use]
    pub fn queued(&self) -> u64 {
        self.queued.load(Ordering::Relaxed)
    }

    /// How many commands the transport is done with: written, or dropped.
    #[must_use]
    pub fn written_count(&self) -> u64 {
        *self.written.borrow()
    }

    /// How many accepted commands the transport is not yet done with.
    #[must_use]
    pub fn backlog(&self) -> u64 {
        // The transport can be done with a command before its send has counted it.
        self.queued().saturating_sub(self.written_count())
    }

    /// A waiter on [`Self::written_count`], to wake a sender as soon as the link catches up.
    #[must_use]
    pub fn written(&self) -> Written {
        Written(self.written.clone())
    }

    fn try_send(
        &self,
        tx: &mpsc::Sender<HostToBridge>,
        cmd: HostToBridge,
    ) -> Result<(), SendError> {
        tx.try_send(cmd).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SendError::Full,
            mpsc::error::TrySendError::Closed(_) => SendError::Closed,
        })?;
        self.queued.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// Wakes when the transport is done with more commands ([`LinkHandle::written`]).
#[derive(Debug, Clone)]
pub struct Written(watch::Receiver<u64>);

impl Written {
    /// Resolves once the count moves. Pends forever once the transport is gone, so it can sit in
    /// a `select!` beside the link's events, which say so.
    pub async fn changed(&mut self) {
        if self.0.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

/// The transport's end of the same channels.
///
/// A transport sends [`LinkEvent`]s on `events` and takes commands from `commands`. It reports
/// every command it takes done exactly once, through [`CommandRx::done`] or
/// [`CommandRx::discard`] (the crate `//!`, "Every command a transport takes is reported done
/// exactly once"). Dropping the plumbing ends the link.
#[derive(Debug)]
pub struct LinkPlumbing {
    /// Where the transport reports what happened on the link.
    pub events: mpsc::Sender<LinkEvent>,
    /// The commands queued for the bridge.
    pub commands: CommandRx,
    /// Held while a transport that must finish before exit runs; dropped with it.
    _running: Option<watch::Sender<()>>,
}

/// Build a connected pair of endpoints.
#[must_use]
pub fn link_pair() -> (LinkHandle, LinkPlumbing) {
    let (event_tx, event_rx) = mpsc::channel(EVENT_CAPACITY);
    let (urgent_tx, urgent_rx) = mpsc::channel(URGENT_CAPACITY);
    let (bulk_tx, bulk_rx) = mpsc::channel(BULK_CAPACITY);
    let (written_tx, written_rx) = watch::channel(0);
    (
        LinkHandle {
            events: event_rx,
            urgent: urgent_tx,
            bulk: bulk_tx,
            queued: AtomicU64::new(0),
            written: written_rx,
            running: None,
        },
        LinkPlumbing {
            events: event_tx,
            commands: CommandRx {
                urgent: urgent_rx,
                bulk: bulk_rx,
                held: None,
                written: written_tx,
            },
            _running: None,
        },
    )
}

/// [`link_pair`] for a transport that must finish before the process exits: the handle's
/// [`LinkHandle::close`] and [`close_all`] wait until the plumbing is dropped.
#[must_use]
pub fn link_pair_awaited() -> (LinkHandle, LinkPlumbing) {
    let (mut handle, mut plumbing) = link_pair();
    let (tx, rx) = watch::channel(());
    running().push(Finished(rx.clone()));
    handle.running = Some(Finished(rx));
    plumbing._running = Some(tx);
    (handle, plumbing)
}

/// Ends once its transport has finished: the sender in its plumbing is dropped.
#[derive(Debug, Clone)]
struct Finished(watch::Receiver<()>);

impl Finished {
    async fn wait(mut self) {
        while self.0.changed().await.is_ok() {}
    }
}

/// Every transport this process started that must finish before it exits.
static RUNNING: Mutex<Vec<Finished>> = Mutex::new(Vec::new());

fn running() -> std::sync::MutexGuard<'static, Vec<Finished>> {
    RUNNING.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wait up to `within` for every Bluetooth transport this process started to let its bridge
/// go. Each finishes once its [`LinkHandle`] is dropped or closed, so `main` calls this after the
/// command has returned, on every path.
pub async fn close_all(within: Duration) {
    let all = std::mem::take(&mut *running());
    let waits = futures_util::future::join_all(all.into_iter().map(Finished::wait));
    let _ = tokio::time::timeout(within, waits).await;
}

/// Receives commands, always preferring urgent ones.
///
/// Without this bias a burst of status polls can queue ahead of a channel
/// assignment and hold it back from the heartbeat it answers. It is the
/// concrete mechanism keeping assignments prompt.
///
/// It also keeps the count of commands the transport is done with, which the handle reads.
#[derive(Debug)]
pub struct CommandRx {
    urgent: mpsc::Receiver<HostToBridge>,
    bulk: mpsc::Receiver<HostToBridge>,
    /// A command taken and given back ([`Self::hold`]), returned before either queue.
    held: Option<HostToBridge>,
    written: watch::Sender<u64>,
}

impl CommandRx {
    /// Next command, urgent first. `None` once both senders are dropped.
    pub async fn recv(&mut self) -> Option<HostToBridge> {
        if let Some(cmd) = self.held.take() {
            return Some(cmd);
        }
        tokio::select! {
            biased;
            Some(cmd) = self.urgent.recv() => Some(cmd),
            Some(cmd) = self.bulk.recv() => Some(cmd),
            else => None,
        }
    }

    /// The next command already queued, urgent first, without waiting.
    pub fn try_recv(&mut self) -> Option<HostToBridge> {
        self.held
            .take()
            .or_else(|| self.urgent.try_recv().ok())
            .or_else(|| self.bulk.try_recv().ok())
    }

    /// Give back `cmd`, taken and not used: the next [`Self::recv`] or [`Self::try_recv`]
    /// returns it, and it stays owed a [`Self::done`].
    pub fn hold(&mut self, cmd: HostToBridge) {
        debug_assert!(self.held.is_none(), "one command held at a time");
        self.held = Some(cmd);
    }

    /// The transport is done with `n` more commands: written, or given up on.
    pub fn done(&mut self, n: u64) {
        if n > 0 {
            self.written.send_modify(|written| *written += n);
        }
    }

    /// Drop every command queued, counting each done, and return how many there were.
    pub fn discard(&mut self) -> usize {
        let mut dropped = 0;
        while self.try_recv().is_some() {
            dropped += 1;
        }
        self.done(dropped as u64);
        dropped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn link_handle_prioritizes_urgent_commands_when_bulk_commands_are_backlogged() {
        // Without this bias a burst of status polls can queue ahead of a channel
        // assignment and hold it back from the heartbeat it answers.
        let (handle, mut plumbing) = link_pair();
        for _ in 0..32 {
            handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        }
        handle.send_urgent(HostToBridge::Reset).expect("queued");

        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::Reset));
    }

    #[tokio::test]
    async fn link_handle_drains_bulk_commands_when_urgent_queue_is_empty() {
        let (handle, mut plumbing) = link_pair();
        handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        handle.send_urgent(HostToBridge::Reset).expect("queued");

        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::Reset));
        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::GetStatus));
    }

    #[tokio::test]
    async fn command_rx_returns_none_when_link_handle_is_dropped() {
        let (handle, mut plumbing) = link_pair();
        drop(handle);
        assert_eq!(plumbing.commands.recv().await, None);
    }

    #[tokio::test]
    async fn link_handle_returns_full_error_when_urgent_queue_capacity_exceeded() {
        // The engine must keep running: a missed assignment is retried on the
        // next heartbeat, but a stalled engine misses everything.
        let (handle, _plumbing) = link_pair();
        for _ in 0..URGENT_CAPACITY {
            handle.send_urgent(HostToBridge::Reset).expect("queued");
        }
        assert_eq!(handle.send_urgent(HostToBridge::Reset), Err(SendError::Full));
    }

    /// A transport that lets its bridge go once the handle is gone, after `takes`.
    fn transport(
        mut plumbing: LinkPlumbing,
        takes: std::time::Duration,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while plumbing.commands.recv().await.is_some() {}
            tokio::time::sleep(takes).await;
            drop(plumbing);
        })
    }

    #[tokio::test]
    async fn link_handle_waits_for_transport_when_closed() {
        let (handle, plumbing) = link_pair_awaited();
        let task = transport(plumbing, std::time::Duration::from_millis(50));
        handle.close().await;
        assert!(task.is_finished(), "close returned before the transport let go");
    }

    #[tokio::test]
    async fn close_all_waits_for_transport_when_its_handle_was_dropped() {
        // The path every command takes: the handle goes with the command's future, and `main`
        // waits.
        let (handle, plumbing) = link_pair_awaited();
        let task = transport(plumbing, std::time::Duration::from_millis(50));
        drop(handle);
        close_all(CLOSE_WITHIN).await;
        assert!(task.is_finished(), "close_all returned before the transport let go");
    }

    #[tokio::test(start_paused = true)]
    async fn link_handle_gives_up_on_transport_when_it_outlasts_the_bound() {
        let (handle, plumbing) = link_pair_awaited();
        let task = transport(plumbing, CLOSE_WITHIN * 10);
        let started = tokio::time::Instant::now();
        handle.close().await;
        assert_eq!(started.elapsed(), CLOSE_WITHIN);
        assert!(!task.is_finished());
    }

    #[tokio::test]
    async fn link_handle_closes_at_once_when_transport_is_not_awaited() {
        // Serial and the simulator: nothing to wait for.
        let (handle, _plumbing) = link_pair();
        tokio::time::timeout(std::time::Duration::from_millis(100), handle.close())
            .await
            .expect("closed at once");
    }

    #[tokio::test]
    async fn link_handle_counts_command_written_when_transport_is_done_with_it() {
        let (handle, mut plumbing) = link_pair();
        let mut written = handle.written();
        handle.send_urgent(HostToBridge::Reset).expect("queued");
        handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        assert_eq!((handle.queued(), handle.written_count(), handle.backlog()), (2, 0, 2));

        assert_eq!(plumbing.commands.recv().await, Some(HostToBridge::Reset));
        plumbing.commands.done(1);
        tokio::time::timeout(Duration::from_millis(100), written.changed())
            .await
            .expect("the waiter wakes");
        assert_eq!((handle.written_count(), handle.backlog()), (1, 1));
    }

    #[tokio::test]
    async fn command_rx_counts_commands_written_when_queue_is_discarded() {
        // A sender waiting on the backlog must see it empty, or it waits forever.
        let (handle, mut plumbing) = link_pair();
        for _ in 0..3 {
            handle.send_urgent(HostToBridge::Reset).expect("queued");
            handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        }
        assert_eq!(plumbing.commands.discard(), 6);
        assert_eq!((handle.written_count(), handle.backlog()), (6, 0));
        assert_eq!(plumbing.commands.try_recv(), None);
    }

    #[tokio::test]
    async fn command_rx_returns_urgent_first_when_try_recv_finds_both() {
        let (handle, mut plumbing) = link_pair();
        handle.send_bulk(HostToBridge::GetStatus).expect("queued");
        handle.send_urgent(HostToBridge::Reset).expect("queued");
        assert_eq!(plumbing.commands.try_recv(), Some(HostToBridge::Reset));
        assert_eq!(plumbing.commands.try_recv(), Some(HostToBridge::GetStatus));
        assert_eq!(plumbing.commands.try_recv(), None);
    }

    #[tokio::test]
    async fn written_pends_when_transport_is_gone() {
        // Safe as a `select!` arm: a closed link must not spin the loop.
        let (handle, plumbing) = link_pair();
        let mut written = handle.written();
        drop(plumbing);
        let woke = tokio::time::timeout(Duration::from_millis(50), written.changed()).await;
        assert!(woke.is_err());
    }

    #[test]
    fn transport_error_words_open_failure_when_given_the_os_reason() {
        let error = TransportError::Open {
            port: "/dev/ttyACM0".into(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory"),
        };
        assert_eq!(error.to_string(), "could not open /dev/ttyACM0: No such file or directory");
    }

    #[test]
    fn transport_error_keeps_os_reason_as_its_source_when_listing_fails() {
        let error = TransportError::Enumerate(std::io::Error::other("udev is unavailable"));
        let source = std::error::Error::source(&error).expect("the OS's reason");
        assert_eq!(source.to_string(), "udev is unavailable");
        assert_eq!(error.to_string(), "could not list serial ports: udev is unavailable");
    }

    #[tokio::test]
    async fn link_handle_returns_closed_error_when_sending_after_plumbing_dropped() {
        let (handle, plumbing) = link_pair();
        drop(plumbing);
        assert_eq!(handle.send_urgent(HostToBridge::Reset), Err(SendError::Closed));
    }
}
