//! Talking to a real bridge over USB CDC.
//!
//! Deliberately built on blocking [`serialport`] reads and writes on dedicated
//! threads rather than an async serial crate. A tty file descriptor is a poor
//! fit for kqueue/epoll readiness, and the extra hop costs latency exactly
//! where it is scarce — a node holds its admin window open for only 100 ms.
//! Two threads and a pair of channels are simpler and more predictable.

use std::io::{ErrorKind, Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LINK_PROTO_VERSION, LinkError, MAX_FRAME, Mac,
    decode_frame, encode_frame,
};

use crate::ports::{self, PortCandidate};
use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// USB CDC ignores the rate, but the field still has to be given.
const BAUD: u32 = 921_600;

/// Long enough not to spin, short enough that shutdown feels immediate.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// How often to re-ask a silent bridge to identify itself. The first tick of a
/// tokio interval fires immediately, so the usual case costs one frame.
const IDENTIFY_INTERVAL: Duration = Duration::from_millis(500);

/// How many unanswered `Identify` frames before the attempt is abandoned.
///
/// The thirteenth tick gives up, at 6.0 s — a second past the CLI's five-second
/// notice, so the operator reads the long form before the one-line reason.
///
/// Bounded at all because asking forever wedges the process, and pointing `wartui`
/// at a node rather than the bridge is all it takes: frames pile up in that tty's
/// output queue, which closing the port waits on and `SIGKILL` cannot interrupt
/// (`crates/wartui/src/main.rs`, `Terminate`). Giving up before anything is left
/// queued is what prevents it, and [`supervise`] retries on its own cadence.
const IDENTIFY_ATTEMPTS: u32 = 12;

/// The Espressif boards attached, which are the bridge and any node on USB.
///
/// # Errors
/// [`TransportError::Enumerate`] if the ports cannot be listed.
pub fn discover_ports() -> Result<Vec<PortCandidate>, TransportError> {
    let mut found = ports::list()?;
    found.retain(ports::could_be_a_bridge);
    Ok(found)
}

/// How a bridge was named on the command line.
///
/// A MAC is worth accepting because it is the only name that survives everything:
/// re-enumeration moves a device node, replugging into another socket moves a
/// `by-path` name, and an ESP32's address moves only when the board does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BridgeSpec {
    /// A device path, opened as given and never traded for another.
    Path(String),
    /// A board's address, matched against what the OS reports as its serial number.
    Mac(Mac),
}

impl std::str::FromStr for BridgeSpec {
    type Err = std::convert::Infallible;

    /// Six colon-separated hex pairs is an address; everything else is a path.
    /// Nothing else can be: no device node is spelled that way.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(ports::parse_mac(text).map_or_else(|| Self::Path(text.to_owned()), Self::Mac))
    }
}

impl std::fmt::Display for BridgeSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(path) => f.write_str(path),
            Self::Mac(mac) => {
                for (i, byte) in mac.iter().enumerate() {
                    if i > 0 {
                        f.write_str(":")?;
                    }
                    write!(f, "{byte:02X}")?;
                }
                Ok(())
            }
        }
    }
}

/// The port a spec names, out of what is attached.
///
/// A spec that names a board not attached is refused rather than answered with
/// another: an operator who named a board meant that board, and quietly opening a
/// different one is how a capture ends up attributed to the wrong fleet.
///
/// # Errors
/// [`TransportError::NoSuchBridge`] if nothing attached carries that address.
pub fn resolve(spec: &BridgeSpec) -> Result<String, TransportError> {
    match spec {
        BridgeSpec::Path(path) => Ok(path.clone()),
        BridgeSpec::Mac(wanted) => ports::list()?
            .into_iter()
            .find(|candidate| candidate.mac().as_ref() == Some(wanted))
            .map(|candidate| candidate.path)
            .ok_or_else(|| TransportError::NoSuchBridge { spec: spec.to_string() }),
    }
}

/// A link to a real bridge, reconnecting on its own when the cable moves.
#[derive(Debug, Clone)]
pub struct SerialTransport {
    spec: Option<BridgeSpec>,
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
        Self { spec: None, reconnect_delay: Duration::from_millis(750) }
    }

    /// Use the board the operator named, by path or by address.
    #[must_use]
    pub fn with_spec(spec: BridgeSpec) -> Self {
        Self { spec: Some(spec), reconnect_delay: Duration::from_millis(750) }
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
        if let Some(spec) = &self.spec {
            return resolve(spec);
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
    // listening is the next question. Which line is last in the log is the diagnosis.
    progress!(port = %path, "port open; asking the bridge to identify itself");
    let writer = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;
    // A third handle, given to the guard below so that the cleanup it does is
    // owned by a value rather than by a code path.
    let flush = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let announced = Arc::new(AtomicBool::new(false));
    // Distinct from `announced`, and load-bearing: a bridge whose fleet is busy can
    // lose every `Ready` to its own oldest-first transmit rings while its
    // observations arrive perfectly well, so giving up on `announced` alone would
    // tear down a working link every few seconds. A frame that would not decode does
    // not count — a board running node firmware talks constantly and none of it is
    // a frame.
    let decoded = Arc::new(AtomicBool::new(false));
    let (dead_tx, mut dead_rx) = mpsc::channel::<String>(1);
    // Unbounded, and deliberately so. The biased select below arbitrates when a
    // command is taken rather than when it reaches the wire, so a burst of bulk
    // commands drains into this queue and an assignment behind them inherits the
    // latency — 28-35 ms against a node's 100 ms admin window, and that is the
    // pessimistic figure, taken from unacknowledged sends (`docs/phase-4-findings.md`).
    // Two bounded channels and a condvar in `write_loop` is the fix if that ever
    // changes, and `assignment.latency_us` is where it would show.
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
                    // Says what was observed, and stops short of concluding
                    // "it is not a bridge", which is false in the case an
                    // operator actually hits: it *is* the bridge, its transmit
                    // endpoint has stopped draining, and it is still reading
                    // every frame sent to it — which is why the remedy named
                    // here is a command rather than a shrug.
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
/// A `Drop` rather than a few lines at the end of [`connect`], because the end that
/// matters does not reach those lines: quitting a capture drops the task, so the
/// runtime stops it at an await and the reader and writer threads keep their handles
/// on the port. Whatever is queued for a device that is not reading then goes into a
/// `close` that waits for it — the wedge, by the one path that had no cleanup.
///
/// Clearing the queue is also what releases a writer already blocked inside a write:
/// the handles share one open file description.
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
    // Local rather than shared: a property of one connection, not of the link.
    let mut last_uptime: Option<u32> = None;

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
                    uptime_ms,
                    ..
                }) => {
                    decoded.store(true, Ordering::Relaxed);
                    announced.store(true, Ordering::Relaxed);
                    // Two `Ready` frames arrive on one connection for two very
                    // different reasons, and the uptime is what separates them.
                    // One is a second answer to an `Identify` sent before the
                    // first came back: same life, and reporting it would look
                    // like a reconnect that never happened. The other is a
                    // bridge that rebooted underneath us — a software reset does
                    // not re-enumerate the USB device, so this descriptor reads
                    // straight through it — and that has to be reported, or the
                    // engine keeps the dead life's `BridgeInfo` and goes on
                    // believing in a peer table the reboot emptied. An uptime
                    // that went backwards is the second case and cannot be the
                    // first.
                    let fresh_life = is_a_new_life(uptime_ms, last_uptime);
                    last_uptime = Some(uptime_ms);
                    if !fresh_life {
                        continue;
                    }
                    // Without it, a log of a link that dropped and came back ends
                    // at "opening the bridge" — as does one that never did.
                    let hex = mac.map(|byte| format!("{byte:02X}")).join(":");
                    tracing::info!(
                        chip = ?chip,
                        mac = %hex,
                        fw = %fw_version.as_str(),
                        reset = ?reset_cause,
                        phase = ?last_phase,
                        heap_free,
                        uptime_ms,
                        "the bridge announced itself"
                    );
                    LinkEvent::Connected(BridgeInfo {
                        chip,
                        mac,
                        fw_version: fw_version.as_str().to_owned(),
                        reset_cause,
                        last_phase,
                        heap_free,
                        uptime_ms,
                    })
                }
                Ok(msg) => {
                    decoded.store(true, Ordering::Relaxed);
                    LinkEvent::Message(msg)
                }
                // Reset banners and half-frames land here; the framing has already
                // resynchronised, so this is a counter rather than a fault. At
                // `debug`, or a bad cable writes the log as fast as the bridge talks.
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
        // Not `write_all`, which cannot be told to stop. Emptying the output queue
        // is what releases a write blocked against a device that is not reading,
        // and `write_all` would answer by writing the rest of the frame straight
        // back into the queue just cleared. So the frame is abandoned: nobody is
        // reading it, and its connection is over.
        let mut sent = 0;
        while sent < n {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            match port.write(&buf[sent..n]) {
                Ok(0) => return,
                Ok(written) => sent += written,
                // A write timeout is the port being slow, not shut: the loop
                // re-checks `stop`, which is how this thread notices a shutdown
                // while the fleet is quiet.
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                Err(e) => {
                    tracing::warn!("serial write failed: {e}");
                    return;
                }
            }
        }
    }
}

/// Whether a `Ready` came from a life that started after the last one seen.
///
/// The first is always a new life. After that, the only thing separating a reboot
/// from a second answer to an `Identify` already in flight is that the reboot's
/// clock started again. See the call site for why both arrive on one connection.
fn is_a_new_life(uptime_ms: u32, last_seen: Option<u32>) -> bool {
    last_seen.is_none_or(|previous| uptime_ms < previous)
}

#[cfg(test)]
mod tests {
    use super::is_a_new_life;

    #[test]
    fn the_first_ready_on_a_connection_is_always_the_bridge_arriving() {
        assert!(is_a_new_life(0, None));
        assert!(is_a_new_life(10_800_000, None), "attaching to one already up for hours");
    }

    #[test]
    fn a_second_answer_to_an_identify_is_not_a_reconnect() {
        // Two `Identify` frames in flight, answered two milliseconds apart.
        assert!(!is_a_new_life(1_202, Some(1_200)));
        assert!(!is_a_new_life(1_200, Some(1_200)), "the same millisecond counts as the same life");
    }

    #[test]
    fn a_bridge_that_rebooted_underneath_the_host_is_reported() {
        // What a software reset produces: no re-enumeration, so this arrives on
        // the connection the old life was announced on.
        assert!(is_a_new_life(180, Some(10_800_000)), "a stall reset after hours of service");
        assert!(is_a_new_life(0, Some(3_100)), "and one only seconds into a life");
    }
}
