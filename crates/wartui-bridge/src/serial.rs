//! Talking to a real bridge over USB CDC.
//!
//! Deliberately built on blocking [`serialport`] reads and writes on dedicated
//! threads rather than an async serial crate. A tty file descriptor is a poor
//! fit for kqueue/epoll readiness, and the extra hop costs latency exactly
//! where it is scarce — a node holds its admin window open for only 300 ms.
//! Two threads and a pair of channels are simpler and more predictable.

use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LinkError, MAX_FRAME,
    decode_frame, encode_frame,
};

use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// Espressif's USB vendor ID, shared by the C5's and C6's native USB Serial/JTAG.
pub const ESPRESSIF_VID: u16 = 0x303A;

/// USB CDC ignores the rate, but the field still has to be given.
const BAUD: u32 = 921_600;

/// Long enough not to spin, short enough that shutdown feels immediate.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// How often to re-ask a silent bridge to identify itself. The first tick of a
/// tokio interval fires immediately, so the usual case costs one frame.
const IDENTIFY_INTERVAL: Duration = Duration::from_millis(500);

/// How many unanswered `Identify` frames before the attempt is abandoned.
///
/// Twelve go out at 0.0 s to 5.5 s — `interval`'s first tick is immediate — and
/// the thirteenth tick, at 6.0 s, is the one that gives up. That is a second
/// past the CLI's five-second "nothing has identified itself" notice, so the
/// operator still gets the long form, which names the three things it usually
/// is, before the one-line reason lands under it.
///
/// It is bounded at all because asking forever wedges the process, and pointing
/// `wartui` at a node instead of the bridge is all it takes to get there. Node
/// firmware never reads the USB endpoint the host writes to, so frames sent to
/// one pile up in the kernel's output queue for that tty. The fd is blocking —
/// `serialport` clears `O_NONBLOCK` once the port is open — and closing a tty
/// waits for its output queue to drain, against a device that will never take
/// it. That wait is inside the driver, so the process survives `SIGKILL` and
/// keeps the port held: the operator's only way out is unplugging the board.
/// Giving up early enough that nothing is left queued is what prevents it, and
/// [`supervise`] retries on its own cadence afterwards, so a dongle that is
/// merely slow to boot still gets found.
const IDENTIFY_ATTEMPTS: u32 = 12;

/// A serial port that might be a bridge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortCandidate {
    /// Device path to open.
    pub path: String,
    /// USB vendor ID, when the OS reported one.
    pub vid: Option<u16>,
    /// USB product ID, when the OS reported one.
    pub pid: Option<u16>,
    /// Product string, for showing the user which device was picked.
    pub product: Option<String>,
}

/// List serial ports that look like an Espressif device.
///
/// # Errors
/// [`TransportError::Enumerate`] if the ports cannot be listed.
pub fn discover_ports() -> Result<Vec<PortCandidate>, TransportError> {
    let ports = serialport::available_ports().map_err(TransportError::Enumerate)?;
    let mut found: Vec<PortCandidate> = ports
        .into_iter()
        .filter_map(|p| match p.port_type {
            serialport::SerialPortType::UsbPort(usb) if usb.vid == ESPRESSIF_VID => {
                Some(PortCandidate {
                    path: p.port_name,
                    vid: Some(usb.vid),
                    pid: Some(usb.pid),
                    product: usb.product,
                })
            }
            _ => None,
        })
        .filter(|c| is_usable_path(&c.path))
        .collect();
    found.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(found)
}

/// On macOS every USB serial device appears twice. `/dev/tty.*` is the callout
/// side and blocks on carrier detect, so only `/dev/cu.*` is usable.
#[must_use]
pub fn is_usable_path(path: &str) -> bool {
    !path.starts_with("/dev/tty.")
}

/// A link to a real bridge, reconnecting on its own when the cable moves.
#[derive(Debug, Clone)]
pub struct SerialTransport {
    port: Option<String>,
    reconnect_delay: Duration,
}

impl Default for SerialTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl SerialTransport {
    /// Find a bridge automatically.
    #[must_use]
    pub const fn new() -> Self {
        Self { port: None, reconnect_delay: Duration::from_millis(750) }
    }

    /// Use a specific device path instead of searching.
    #[must_use]
    pub fn with_port(port: impl Into<String>) -> Self {
        Self { port: Some(port.into()), reconnect_delay: Duration::from_millis(750) }
    }

    /// How long to wait between reconnection attempts.
    #[must_use]
    pub const fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Open the link and start pumping it.
    ///
    /// Returns as soon as the supervisor is running; the first
    /// [`LinkEvent::Connected`] arrives once a bridge has announced itself.
    ///
    /// # Errors
    /// Currently infallible — connection problems surface as
    /// [`LinkEvent::Disconnected`] so a cable that is not plugged in yet is not
    /// a fatal startup error.
    pub fn start(self) -> Result<LinkHandle, TransportError> {
        let (handle, plumbing) = link_pair();
        tokio::spawn(supervise(self, plumbing));
        Ok(handle)
    }

    fn resolve_port(&self) -> Result<String, TransportError> {
        if let Some(port) = &self.port {
            return Ok(port.clone());
        }
        discover_ports()?.into_iter().next().map(|c| c.path).ok_or(TransportError::NoBridgeFound)
    }
}

/// Keep a link up: open, pump, report the failure, wait, try again.
async fn supervise(transport: SerialTransport, mut plumbing: crate::LinkPlumbing) {
    // A link that keeps failing the same way is one event, not thousands. Only
    // a change of reason earns a line at `warn`: a capture left running against
    // a port that is somebody else's retries every `reconnect_delay` for as long
    // as it runs, and the log file the README sends operators to would be those
    // same few lines a hundred thousand times over.
    let mut previous: Option<String> = None;
    let mut repeats = 0_u32;
    loop {
        match connect(&transport, &mut plumbing, previous.is_none()).await {
            Ok(()) => return, // The handle was dropped; nobody is listening.
            Err(reason) => {
                if previous.as_deref() == Some(reason.as_str()) {
                    repeats = repeats.saturating_add(1);
                    tracing::debug!(reason = %reason, repeats, "link still down");
                } else {
                    // The view has one line to say this on and shares it with
                    // everything else, so a log file is where a link that keeps
                    // failing the same way becomes obvious.
                    tracing::warn!(
                        reason = %reason,
                        retry_in = ?transport.reconnect_delay,
                        "link down"
                    );
                    previous = Some(reason.clone());
                    repeats = 0;
                }
                let event = LinkEvent::Disconnected { reason };
                if plumbing.events.send(event).await.is_err() {
                    return;
                }
            }
        }
        tokio::time::sleep(transport.reconnect_delay).await;
    }
}

/// One connection's lifetime. `Err` carries a reason to show the user.
async fn connect(
    transport: &SerialTransport,
    plumbing: &mut crate::LinkPlumbing,
    unreported: bool,
) -> Result<(), String> {
    // An attempt that is retrying a failure already reported is not news, and
    // there is one of them every `reconnect_delay`. `RUST_LOG=debug` keeps them.
    macro_rules! progress {
        ($($arg:tt)*) => {
            if unreported { tracing::info!($($arg)*) } else { tracing::debug!($($arg)*) }
        };
    }
    let path = transport.resolve_port().map_err(|e| e.to_string())?;
    progress!(port = %path, "opening the bridge");

    let port = serialport::new(&path, BAUD)
        .timeout(READ_TIMEOUT)
        .open()
        .map_err(|e| format!("could not open {path}: {e}"))?;
    // Distinct from being connected: the port is ours, and whether anything is
    // listening on the other end is the next question. Which of these two lines
    // is the last one in the log is the whole diagnosis.
    progress!(port = %path, "port open; asking the bridge to identify itself");
    let writer = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;
    // A third handle, given to the guard below so that the cleanup it does is
    // owned by a value rather than by a code path.
    let flush = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let announced = Arc::new(AtomicBool::new(false));
    // Distinct from `announced`, and the distinction is load-bearing. A bridge
    // whose fleet is busy can lose every `Ready` it sends to its own transmit
    // rings, which evict oldest-first, while its observations arrive perfectly
    // well — `sniff` says the same thing where it decides whether to accuse a
    // port of not being a bridge. Giving up on `announced` alone would tear
    // down a link that is working and delivering, every few seconds, which is
    // worse than the wedge this bound exists to prevent. A frame that would not
    // decode does not count: a board running node firmware talks constantly and
    // none of it is a frame.
    let decoded = Arc::new(AtomicBool::new(false));
    let (dead_tx, mut dead_rx) = mpsc::channel::<String>(1);
    // Unbounded, and now deliberately so rather than provisionally. The biased
    // select below arbitrates at the moment a command is taken rather than the
    // moment it reaches the wire, so a burst of bulk commands drains straight
    // into this queue and an assignment behind them inherits their latency.
    //
    // Phase 4 measured what that costs. On a C6 bridge, heartbeat-to-transmit-
    // callback came out at 28-35 ms against a node's 300 ms admin window —
    // about a tenth of the budget, and that figure is the pessimistic one,
    // taken from *unacknowledged* sends where the callback fires only after the
    // radio exhausts its retry chain. A successful send is far quicker. There
    // is no case for two bounded channels and a condvar in `write_loop` at
    // those numbers; if the fleet ever grows enough bulk traffic to change
    // them, that is the fix, and `assignment.latency_us` is where it shows up.
    let (write_tx, write_rx) = std::sync::mpsc::channel::<HostToBridge>();

    let reader = std::thread::Builder::new()
        .name(format!("wartui-serial-rx {path}"))
        .spawn({
            let events = plumbing.events.clone();
            let stop = Arc::clone(&stop);
            let announced = Arc::clone(&announced);
            let decoded = Arc::clone(&decoded);
            move || {
                let reason = read_loop(port, &events, &stop, &announced, &decoded);
                let _ = dead_tx.blocking_send(reason);
            }
        })
        .map_err(|e| format!("could not start reader thread: {e}"))?;

    // Before the second spawn, not after both: a `?` there would otherwise
    // return with the reader thread still running on a port nobody will ever
    // stop, while `supervise` reopens the same path every `reconnect_delay`.
    let shutdown = Shutdown { stop: Arc::clone(&stop), port: flush };

    let writer_thread = std::thread::Builder::new()
        .name(format!("wartui-serial-tx {path}"))
        .spawn({
            let stop = Arc::clone(&stop);
            move || write_loop(writer, &write_rx, &stop)
        })
        .map_err(|e| format!("could not start writer thread: {e}"))?;

    // A bridge announces itself at boot, and the host is rarely watching at
    // that moment: unplugging the dongle is not part of restarting the TUI.
    // So we ask, and go on asking while the radio could still be coming up —
    // but not for ever, which is what [`IDENTIFY_ATTEMPTS`] bounds.
    let mut identify = tokio::time::interval(IDENTIFY_INTERVAL);
    let mut asked = 0_u32;

    // Forward commands, preserving the urgent-first bias, until either the
    // reader dies or the engine drops its handle.
    let outcome = loop {
        tokio::select! {
            biased;
            reason = dead_rx.recv() => {
                break Err(reason.unwrap_or_else(|| "reader stopped".to_owned()));
            }
            _ = identify.tick(), if !announced.load(Ordering::Relaxed) => {
                if asked >= IDENTIFY_ATTEMPTS && !decoded.load(Ordering::Relaxed) {
                    // Says what was observed, and stops short of the conclusion
                    // it used to draw. "It is not a bridge" is false in the case
                    // an operator actually hits: it *is* the bridge, its
                    // transmit endpoint has stopped draining, and it is still
                    // reading every frame sent to it — which is why the remedy
                    // named here is a command rather than a shrug.
                    let seconds =
                        IDENTIFY_INTERVAL.as_millis() * u128::from(IDENTIFY_ATTEMPTS) / 1000;
                    break Err(format!(
                        "nothing on {path} answered the link protocol in {seconds}s; \
                         if it is the bridge, `wartui reset` reboots one that has \
                         stopped answering"
                    ));
                }
                asked += 1;
                if write_tx.send(HostToBridge::Identify).is_err() {
                    break Err("writer stopped".to_owned());
                }
            }
            cmd = plumbing.commands.recv() => match cmd {
                Some(cmd) => {
                    if write_tx.send(cmd).is_err() {
                        break Err("writer stopped".to_owned());
                    }
                }
                None => break Ok(()),
            },
        }
    };

    // Explicitly, because the joins below wait on threads that only stop once
    // it has run. On the path where this task is dropped instead, the same work
    // happens without this line being reached at all.
    drop(shutdown);
    drop(write_tx);
    let _ = reader.join();
    let _ = writer_thread.join();
    outcome
}

/// Stops the port's threads and empties its output queue, however the
/// connection ends.
///
/// This is a `Drop` and not a few lines at the end of [`connect`] because the
/// end that matters does not run those lines. `connect` is an async fn, and
/// quitting a capture drops the task running it: the runtime stops it at an
/// await and everything after the select loop is simply skipped. The reader and
/// writer threads then keep their handles on the port, and whatever is queued
/// for a device that is not reading goes with them into a `close` that waits
/// for that device to take it — which is the wedge, reached by the one path
/// that had no cleanup on it.
///
/// Clearing the queue is also what releases a writer already blocked inside a
/// write: the handles share one open file description, so the discard reaches
/// the queue that thread is stuck on.
struct Shutdown {
    stop: Arc<AtomicBool>,
    port: Box<dyn serialport::SerialPort>,
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.port.clear(serialport::ClearBuffer::Output);
    }
}

/// Blocking read loop. Returns the reason it stopped.
fn read_loop(
    mut port: Box<dyn serialport::SerialPort>,
    events: &mpsc::Sender<LinkEvent>,
    stop: &AtomicBool,
    announced: &AtomicBool,
    decoded: &AtomicBool,
) -> String {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut buf = [0u8; 1024];

    while !stop.load(Ordering::Relaxed) {
        let read = match port.read(&mut buf) {
            Ok(0) => continue,
            Ok(n) => n,
            // A timeout just means the fleet is quiet.
            Err(e) if e.kind() == ErrorKind::TimedOut => continue,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return format!("read failed: {e}"),
        };

        for &byte in &buf[..read] {
            let Some(frame) = acc.push(byte) else { continue };
            let event = match decode_frame::<BridgeToHost>(frame) {
                Ok(BridgeToHost::Ready { chip, mac, fw_version, proto_version, .. })
                    if proto_version != LINK_PROTO_VERSION =>
                {
                    let _ = (chip, mac, fw_version);
                    return LinkError::VersionMismatch {
                        ours: LINK_PROTO_VERSION,
                        theirs: proto_version,
                    }
                    .to_string();
                }
                Ok(BridgeToHost::Ready {
                    chip,
                    mac,
                    fw_version,
                    reset_cause,
                    last_phase,
                    heap_free,
                    ..
                }) => {
                    decoded.store(true, Ordering::Relaxed);
                    // A reboot re-enumerates the USB device and so ends this
                    // connection outright; a second `Ready` inside one can only
                    // be the answer to an `Identify` we sent before the first
                    // answer arrived. Reporting it again would look like a
                    // reconnect that never happened.
                    if announced.swap(true, Ordering::Relaxed) {
                        continue;
                    }
                    // The success this whole sequence is about. Without it a log
                    // of a link that dropped and came back would end at
                    // "opening the bridge", which is what a link that never
                    // came back looks like too.
                    let hex = mac.map(|byte| format!("{byte:02X}")).join(":");
                    tracing::info!(
                        chip = ?chip,
                        mac = %hex,
                        fw = %fw_version.as_str(),
                        reset = ?reset_cause,
                        phase = ?last_phase,
                        heap_free,
                        "the bridge announced itself"
                    );
                    LinkEvent::Connected(BridgeInfo {
                        chip,
                        mac,
                        fw_version: fw_version.as_str().to_owned(),
                        reset_cause,
                        last_phase,
                        heap_free,
                    })
                }
                Ok(msg) => {
                    decoded.store(true, Ordering::Relaxed);
                    LinkEvent::Message(msg)
                }
                // Reset banners and half-frames land here; the framing has
                // already resynchronised, so this is a counter, not a fault.
                // Logged at `debug` because a cable bad enough to garble every
                // frame would otherwise write the log file as fast as the bridge
                // can talk.
                Err(e) => {
                    tracing::debug!(error = %e, "undecodable frame");
                    LinkEvent::Garbled(e)
                }
            };
            if events.blocking_send(event).is_err() {
                return "engine stopped listening".to_owned();
            }
        }
    }
    if announced.load(Ordering::Relaxed) {
        "link closed".to_owned()
    } else {
        "bridge never announced itself".to_owned()
    }
}

/// Blocking write loop.
fn write_loop(
    mut port: Box<dyn serialport::SerialPort>,
    commands: &std::sync::mpsc::Receiver<HostToBridge>,
    stop: &AtomicBool,
) {
    let mut buf = [0u8; MAX_FRAME];
    while let Ok(cmd) = commands.recv() {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        let Ok(n) = encode_frame(&cmd, &mut buf) else {
            // Only reachable if a command outgrew MAX_FRAME, which the
            // const assertion in wartui-proto is there to prevent.
            tracing::error!("command did not fit in a frame; dropping it");
            continue;
        };
        // Not `write_all`, which cannot be told to stop. Emptying the output
        // queue is what releases a write blocked against a device that is not
        // reading, and `write_all` would answer that by writing the rest of the
        // frame straight back into the queue it was just cleared from — after
        // the flush, and on the path where this thread is never joined there is
        // no second one. So the frame is abandoned instead: nobody is reading
        // it, and the connection it belonged to is over.
        let mut sent = 0;
        while sent < n {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match port.write(&buf[sent..n]) {
                Ok(0) => return,
                Ok(written) => sent += written,
                // A write timeout is the port being slow, not shut: the loop
                // re-checks `stop` and tries again, which is also what makes
                // this thread notice a shutdown while the fleet is quiet.
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                Err(e) => {
                    tracing::warn!("serial write failed: {e}");
                    return;
                }
            }
        }
    }
}
