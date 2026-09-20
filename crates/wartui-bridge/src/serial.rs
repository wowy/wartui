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

use crate::ports::{self, BRIDGE_PID, PortCandidate};
use crate::remember::BridgeMemory;
use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// USB CDC ignores the rate, but the field still has to be given.
const BAUD: u32 = 921_600;

/// Long enough not to spin, short enough that shutdown feels immediate.
const READ_TIMEOUT: Duration = Duration::from_millis(50);

/// How long one tick of the patience below is. The first tick of a tokio interval
/// fires immediately, which is when the single `Identify` goes out.
const IDENTIFY_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for a board when there is no other to try, in ticks of
/// [`IDENTIFY_INTERVAL`].
///
/// The thirteenth tick gives up, at 6.0 s — a second past the CLI's five-second
/// notice, so the operator reads the long form before the one-line reason. Long
/// because a dongle still bringing its radio up is not a wedged one, and the
/// remedy for the wedge (`wartui reset`) is worth being sure about.
///
/// Patience is about how long a board is worth *before another is tried*, which is
/// why this is not what a board answered last time gets when something else is
/// attached: opening the other board costs a second and a half and settles it, and
/// waiting six for the first would put the answer past the notice the CLI prints at
/// five. A board passed over that way is tried again on the next pass, 750 ms later.
const SETTLE_TICKS: u32 = 12;

/// How long a board gets when there is another to try, in the same ticks.
///
/// 1.5 s, because a healthy bridge answers in about two milliseconds
/// (`docs/phase-3-findings.md`) and a sweep pays this for every board attached.
/// Passing over the real bridge costs a retry rather than a failure, which is the
/// trade [`SETTLE_TICKS`] explains.
const PROBE_TICKS: u32 = 3;

/// How many undecodable frames from an unproven board before it is passed over.
///
/// A board running node firmware talks constantly and none of it is a frame, so
/// this is the difference between rejecting one in a tenth of a second and waiting
/// out [`PROBE_TICKS`]. It is never applied to a proven board: a bridge forwarding
/// a busy fleet down a bad cable produces these too, and giving up on it would tear
/// down a working link.
const PROBE_GARBLE_LIMIT: u32 = 64;

/// Slowest retry after a sweep that opened several boards and found no bridge.
///
/// A sweep costs an open and a wait per board, so repeating one every
/// `reconnect_delay` would have wartui opening every attached tty twice a second
/// for as long as it runs. A single candidate keeps the short delay: retrying one
/// port is cheap, and it is the case where something is about to be plugged in.
const MAX_SWEEP_DELAY: Duration = Duration::from_secs(5);

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
/// # Errors
/// [`TransportError::Enumerate`] if the ports cannot be listed, or
/// [`TransportError::NoSuchBridge`] if nothing attached carries that address.
pub fn resolve(spec: &BridgeSpec) -> Result<String, TransportError> {
    match spec {
        // A path is answered without enumerating: whether it exists is the
        // question the open asks, and the OS gives a better answer than a list.
        BridgeSpec::Path(path) => Ok(path.clone()),
        BridgeSpec::Mac(_) => resolve_in(&ports::list()?, spec),
    }
}

/// The same choice, against a list rather than against what is plugged in.
///
/// A spec that names a board not attached is refused rather than answered with
/// another: an operator who named a board meant that board, and quietly opening a
/// different one is how a capture ends up attributed to the wrong fleet.
///
/// # Errors
/// [`TransportError::NoSuchBridge`] if nothing in `candidates` is that board.
pub fn resolve_in(
    candidates: &[PortCandidate],
    spec: &BridgeSpec,
) -> Result<String, TransportError> {
    match spec {
        BridgeSpec::Path(path) => Ok(path.clone()),
        BridgeSpec::Mac(wanted) => candidates
            .iter()
            .find(|candidate| candidate.mac().as_ref() == Some(wanted))
            .map(|candidate| candidate.path.clone())
            .ok_or_else(|| TransportError::NoSuchBridge { spec: spec.to_string() }),
    }
}

/// The boards to try, in the order they are worth trying.
///
/// A named board is the only candidate there is, however many are attached: an
/// operator who named one meant that one. Otherwise the order puts the board that
/// answered last first — so the ordinary run opens one port and no others — and
/// after that is simply deterministic, which is what makes a sweep testable and
/// keeps two runs on one machine from disagreeing about what they tried.
#[must_use]
pub fn select(
    candidates: &[PortCandidate],
    spec: Option<&BridgeSpec>,
    remembered: Option<Mac>,
) -> Vec<PortCandidate> {
    match spec {
        // Not matched against the enumeration: a path is opened as given, and the
        // OS says better than a list whether anything is there.
        Some(BridgeSpec::Path(path)) => vec![ports::candidate(path, None, None, None)],
        Some(BridgeSpec::Mac(wanted)) => candidates
            .iter()
            .find(|candidate| candidate.mac().as_ref() == Some(wanted))
            .cloned()
            .into_iter()
            .collect(),
        None => {
            let mut ordered = candidates.to_vec();
            ordered.sort_by_key(|candidate| {
                (
                    candidate.mac().is_none() || candidate.mac() != remembered,
                    candidate.pid != Some(BRIDGE_PID),
                    candidate.path.clone(),
                )
            });
            ordered
        }
    }
}

/// The one board a command may reach without being asked which.
///
/// `wartui reset` transmits before anything has identified itself — that is the
/// point of it, since the board it is for answers nothing — so it can never be
/// pointed at a sweep: a `Reset` sent to a node reboots the node and costs it the
/// addresses it was holding back. It gets an unambiguous answer or none.
///
/// # Errors
/// [`TransportError::NoBridgeFound`] if nothing is attached, or
/// [`TransportError::AmbiguousBridge`] if several boards are and none is known.
pub fn unambiguous_bridge(
    candidates: &[PortCandidate],
    remembered: Option<Mac>,
) -> Result<String, TransportError> {
    if let Some(wanted) = remembered
        && let Some(known) = candidates.iter().find(|candidate| candidate.mac() == Some(wanted))
    {
        return Ok(known.path.clone());
    }
    match candidates {
        [] => Err(TransportError::NoBridgeFound),
        [only] => Ok(only.path.clone()),
        several => Err(TransportError::AmbiguousBridge {
            boards: several
                .iter()
                .map(|candidate| {
                    candidate.mac().as_ref().map_or_else(|| candidate.path.clone(), ports::mac_text)
                })
                .collect::<Vec<_>>()
                .join(", "),
        }),
    }
}

/// A link to a real bridge, reconnecting on its own when the cable moves.
#[derive(Debug, Clone)]
pub struct SerialTransport {
    spec: Option<BridgeSpec>,
    memory: BridgeMemory,
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
        Self {
            spec: None,
            memory: BridgeMemory::none(),
            reconnect_delay: Duration::from_millis(750),
        }
    }

    /// Use the board the operator named, by path or by address.
    #[must_use]
    pub fn with_spec(spec: BridgeSpec) -> Self {
        Self { spec: Some(spec), ..Self::new() }
    }

    /// Carry what was learned into the next run, and start from what the last one
    /// learned.
    #[must_use]
    pub fn remember_in(mut self, memory: BridgeMemory) -> Self {
        self.memory = memory;
        self
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
}

/// Keep a link up: sweep for a bridge, pump it, report the failure, try again.
///
/// The sweep is why this is a loop over a list rather than over one path. A node
/// plugged in by USB is the same vendor and product as the bridge, so the board
/// that sorts first is not reliably the one that answers — and opening it for ever
/// is a capture that never starts, with a log that says nothing but the same line.
///
/// Once a board *has* answered, this stops sweeping and waits for that board.
/// Reaching for another would mean transmitting into a node the moment the bridge
/// is unplugged, which is exactly what an operator does mid-session to reflash one.
async fn supervise(transport: SerialTransport, mut plumbing: crate::LinkPlumbing) {
    // A link that keeps failing the same way is one event, not thousands. Only
    // a change of reason earns a line at `warn`: a capture left running against
    // a port that is somebody else's retries every `reconnect_delay` for as long
    // as it runs, and the log file the README sends operators to would be those
    // same few lines a hundred thousand times over. A whole sweep folds to one
    // reason for the same purpose — otherwise the flood is per board per retry.
    let mut previous: Option<String> = None;
    let mut repeats = 0_u32;
    let mut delay = transport.reconnect_delay;
    // The board this run has settled on. `remembered` only orders the first sweep;
    // this replaces it entirely, and is what stops a sweep reaching a node later.
    let mut settled: Option<Mac> = None;
    let remembered = transport.memory.recall();

    loop {
        let reason =
            match sweep(&transport, &mut plumbing, settled, remembered, previous.is_none()).await {
                Pass::HandleDropped => return, // Nobody is listening.
                Pass::Connected(mac) => {
                    // The file was written the moment this board announced itself,
                    // not here: a command as short as `wartui status` exits with its
                    // connection still open and would otherwise learn nothing.
                    settled = Some(mac);
                    previous = None;
                    repeats = 0;
                    delay = transport.reconnect_delay;
                    // Waited out like any other retry, rather than reopening at once.
                    // A board that announces itself and then drops — a marginal cable,
                    // or one rebooting in a loop — would otherwise be reopened and
                    // asked again as fast as the port can be opened, for as long as it
                    // kept doing it. `Disconnected` has already gone out for this one.
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Pass::Failed { reason, opened } => {
                    // One open is one port, retried cheaply. Several is a sweep, and
                    // repeating one every 750 ms opens every attached tty twice a
                    // second for as long as the capture runs.
                    delay = if opened > 1 {
                        (delay * 2).min(MAX_SWEEP_DELAY)
                    } else {
                        transport.reconnect_delay
                    };
                    reason
                }
            };

        if previous.as_deref() == Some(reason.as_str()) {
            repeats = repeats.saturating_add(1);
            tracing::debug!(reason = %reason, repeats, "link still down");
        } else {
            // The view has one line to say this on and shares it with everything
            // else, so a log file is where a link that keeps failing the same way
            // becomes obvious.
            tracing::warn!(reason = %reason, retry_in = ?delay, "link down");
            previous = Some(reason.clone());
            repeats = 0;
        }
        if plumbing.events.send(LinkEvent::Disconnected { reason }).await.is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
    }
}

/// What one pass over the candidates came to.
enum Pass {
    /// The engine dropped its handle; there is nothing left to do.
    HandleDropped,
    /// A board announced itself, and the connection to it has since ended.
    Connected(Mac),
    /// Nothing answered. `opened` counts the ports that were actually opened, which
    /// is what decides how long to wait before trying again: a candidate the OS
    /// refused, or that vanished between the enumeration and the open, cost no time
    /// and is no reason to wait longer.
    Failed { reason: String, opened: usize },
}

/// Try each candidate in turn until one answers, or the list runs out.
///
/// Strictly in order, never at once: every open holds a [`Shutdown`] guard and a
/// pair of threads, and two boards answering together would leave one connection
/// with nobody reading it.
async fn sweep(
    transport: &SerialTransport,
    plumbing: &mut crate::LinkPlumbing,
    settled: Option<Mac>,
    remembered: Option<Mac>,
    unreported: bool,
) -> Pass {
    let attached = match discover_ports() {
        Ok(attached) => attached,
        Err(e) => return Pass::Failed { reason: e.to_string(), opened: 0 },
    };
    // A run that has found its bridge wants that board and no other, so the list
    // narrows to it. Not finding it costs no open at all, which is what keeps a
    // node plugged in mid-capture from being transmitted into.
    let candidates = match settled {
        Some(mac) => select(&attached, Some(&BridgeSpec::Mac(mac)), None),
        None => select(&attached, transport.spec.as_ref(), remembered),
    };
    if candidates.is_empty() {
        let reason = match (settled, &transport.spec) {
            (Some(mac), _) => format!("the bridge {} is no longer attached", ports::mac_text(&mac)),
            (None, Some(spec)) => {
                TransportError::NoSuchBridge { spec: spec.to_string() }.to_string()
            }
            (None, None) => TransportError::NoBridgeFound.to_string(),
        };
        return Pass::Failed { reason, opened: 0 };
    }

    // Whether this pass has anywhere else to go. It decides both how long each board
    // is worth and whether a board's silence is worth concluding anything from.
    let alone = candidates.len() == 1 || transport.spec.is_some() || settled.is_some();
    if !alone {
        // Said once per sweep, so that the announcement below reads as a choice
        // rather than as the only board there was. With two bridges attached it is
        // the only record of which one this run is driving and which it passed over.
        tracing::info!(boards = candidates.len(), "no bridge is known yet; asking each in turn");
    }

    let mut reasons = Vec::new();
    let mut opened = 0;
    // Set only by the remembered board being opened and then saying nothing, which
    // is the one thing that means it has stopped being the bridge.
    let mut remembered_went_quiet = false;
    for candidate in &candidates {
        let state = Arc::new(Attempt::default());
        let attempt = connect(
            &candidate.path,
            if alone { SETTLE_TICKS } else { PROBE_TICKS },
            alone,
            &state,
            &transport.memory,
            plumbing,
            unreported,
        );
        match attempt.await {
            Ok(()) => return Pass::HandleDropped,
            Err(reason) => {
                let was_opened = state.opened.load(Ordering::Relaxed);
                if was_opened {
                    opened += 1;
                }
                if silence_disowns_it(alone, was_opened, remembered, candidate) {
                    remembered_went_quiet = true;
                }
                // Copied out rather than read in place: the guard would otherwise
                // be held across the send below, which is not `Send`.
                let announced = *state.mac.lock().unwrap_or_else(|e| e.into_inner());
                // A board that announced itself and then went away is not a failed
                // candidate: the sweep is over and what is left is a reconnection.
                if let Some(mac) = announced {
                    tracing::warn!(reason = %reason, mac = %ports::mac_text(&mac), "link down");
                    if plumbing.events.send(LinkEvent::Disconnected { reason }).await.is_err() {
                        return Pass::HandleDropped;
                    }
                    return Pass::Connected(mac);
                }
                reasons.push(reason);
            }
        }
    }

    // A board remembered as the bridge, opened, heard out in full, and silent has
    // been reflashed or replaced. Kept, it would cost every later run that patience
    // before the sweep it needed anyway — and it is the one entry that can come to
    // point at a node, since a node is what a bridge becomes when it is reflashed.
    //
    // It is not the only way the file is corrected, and not the common one: whatever
    // board does answer writes its own address over it. This is for the case where
    // nothing answers at all, which is the only case that leaves a wrong entry
    // standing.
    //
    // **Opened is the whole of it.** A port that would not open said nothing because
    // nothing was asked of it: ModemManager holds a fresh CDC-ACM device for a few
    // seconds on some distributions, and a missing `dialout` group holds it for
    // ever. Forgetting the bridge over either would throw away the right answer for
    // a reason that has nothing to do with the board.
    if settled.is_none() && remembered_went_quiet {
        transport.memory.forget();
    }

    // One reason for the whole pass. Reporting each would put a line per board per
    // retry into the log, and the view has one line to say any of it on.
    let reason = match reasons.as_slice() {
        [only] => only.clone(),
        several => format!(
            "swept {} boards and none answered the link protocol: {}",
            several.len(),
            several.join("; ")
        ),
    };
    Pass::Failed { reason, opened }
}

/// What the reader thread and the connection loop share about one attempt.
///
/// One value rather than five `Arc`s because they are read together and mean
/// something only together: `announced` without `decoded` is a bridge whose
/// `Ready` was lost, and `garbled` without either is a board running node
/// firmware. The sweep keeps it after [`connect`] returns, to read `mac`.
#[derive(Debug, Default)]
struct Attempt {
    /// Set to abandon the port: both threads check it between blocking calls.
    stop: AtomicBool,
    /// The port was opened, as against refused or absent. A candidate that never
    /// opened cost nothing, which is what the sweep's backoff turns on.
    opened: AtomicBool,
    /// A `Ready` has arrived, so this board is the bridge.
    announced: AtomicBool,
    /// Any frame at all has decoded.
    ///
    /// Distinct from `announced`, and load-bearing: a bridge whose fleet is busy
    /// can lose every `Ready` to its own oldest-first transmit rings while its
    /// observations arrive perfectly well, so giving up on `announced` alone
    /// would tear down a working link every few seconds.
    decoded: AtomicBool,
    /// Frames that would not decode. A board running node firmware talks
    /// constantly and none of it is a frame, which is what this counts.
    garbled: std::sync::atomic::AtomicU32,
    /// The address, once a board has announced one.
    mac: std::sync::Mutex<Option<Mac>>,
}

/// One candidate's turn. `Err` carries a reason to show the user.
///
/// `patience` is the whole difference between a probe and settling on a board: the
/// open, the guard, the threads and the frame are one code path either way, which
/// is what keeps there from being two places to get [`Shutdown`] wrong. A probe
/// that wins needs no handing over, because the `identify` arm below is already
/// gated off once a board has announced itself.
async fn connect(
    path: &str,
    patience: u32,
    proven: bool,
    state: &Arc<Attempt>,
    memory: &BridgeMemory,
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
    progress!(port = %path, "opening the bridge");

    let port = serialport::new(path, BAUD)
        .timeout(READ_TIMEOUT)
        // Named rather than left to the default, because it is the setting that
        // keeps the driver from moving RTS on its own; `ports` explains why that
        // matters on a board whose reset line is a pair of modem signals.
        .flow_control(serialport::FlowControl::None)
        .open()
        .map_err(|e| open_failure(path, &e))?;
    // Distinct from being connected: the port is ours, and whether anything is
    // listening is the next question. Which line is last in the log is the diagnosis.
    state.opened.store(true, Ordering::Relaxed);
    progress!(port = %path, "port open; asking the bridge to identify itself");
    let writer = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;
    // A third handle, given to the guard below so that the cleanup it does is
    // owned by a value rather than by a code path.
    let flush = port.try_clone().map_err(|e| format!("could not split {path}: {e}"))?;

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
            let state = Arc::clone(state);
            let memory = memory.clone();
            move || {
                let reason = read_loop(port, &events, &state, &memory);
                let _ = dead_tx.blocking_send(reason);
            }
        })
        .map_err(|e| format!("could not start reader thread: {e}"))?;

    // Before the second spawn, not after both: a `?` there would otherwise
    // return with the reader thread still running on a port nobody will ever
    // stop, while `supervise` reopens the same path every `reconnect_delay`.
    let shutdown = Shutdown { state: Arc::clone(state), port: flush };

    let writer_thread = std::thread::Builder::new()
        .name(format!("wartui-serial-tx {path}"))
        .spawn({
            let state = Arc::clone(state);
            move || write_loop(writer, &write_rx, &state.stop)
        })
        .map_err(|e| format!("could not start writer thread: {e}"))?;

    // A bridge announces itself at boot, and the host is rarely watching at that
    // moment: unplugging the dongle is not part of restarting the TUI. So we ask —
    // **exactly once, and then only wait.**
    //
    // Once, because a board that is not reading its USB endpoint absorbs exactly
    // one packet: the ESP32's USB Serial/JTAG takes one into its OUT FIFO and NACKs
    // every one after it until firmware reads that FIFO, which node firmware never
    // does. The second frame is therefore still in flight when the port is closed,
    // and `close` waits for the tty's output queue — measured at 30 s against a
    // node on this bench, which is the kernel's own timer giving up rather than the
    // board relenting. Nothing can interrupt it: the thread stays alive in the
    // kernel, and because a process cannot exit while one of its threads is in
    // there, `wartui status` pointed at a node hung for half a minute after
    // printing its answer. One frame never reaches that.
    //
    // Once is also enough. A frame written to an enumerated board is delivered:
    // if its main loop has not started draining the link yet, our `Identify` waits
    // in that same FIFO and is read when it does. The patience below is for the
    // *answer* to take its time, not for the asking to be repeated.
    let mut identify = tokio::time::interval(IDENTIFY_INTERVAL);
    let mut waited = 0_u32;

    // Forward commands, preserving the urgent-first bias, until either the
    // reader dies or the engine drops its handle.
    let outcome = loop {
        tokio::select! {
            biased;
            reason = dead_rx.recv() => {
                break Err(reason.unwrap_or_else(|| "reader stopped".to_owned()));
            }
            _ = identify.tick(), if !state.announced.load(Ordering::Relaxed) => {
                // Never applied to a proven board: a bridge forwarding a busy fleet
                // down a bad cable produces undecodable frames too, and giving up
                // on it would tear down a working link every few seconds.
                if !proven && state.garbled.load(Ordering::Relaxed) >= PROBE_GARBLE_LIMIT {
                    break Err(format!(
                        "{path} is talking, and none of it is the link protocol"
                    ));
                }
                if waited >= patience && !state.decoded.load(Ordering::Relaxed) {
                    // Says what was observed, and stops short of concluding
                    // "it is not a bridge", which is false in the case an
                    // operator actually hits: it *is* the bridge, its transmit
                    // endpoint has stopped draining, and it is still reading
                    // every frame sent to it — which is why the remedy named
                    // here is a command rather than a shrug.
                    let millis = IDENTIFY_INTERVAL.as_millis() * u128::from(patience);
                    // A board that was named or remembered is the one the advice is
                    // for; one passed over in a sweep is very likely a node, and
                    // telling an operator to reset it would be telling them to
                    // reboot the wrong board.
                    break Err(if proven {
                        format!(
                            "nothing on {path} answered the link protocol in {}s; \
                             if it is the bridge, `wartui reset` reboots one that has \
                             stopped answering",
                            millis / 1000
                        )
                    } else {
                        format!("nothing on {path} answered the link protocol in {millis}ms")
                    });
                }
                waited += 1;
                // The first tick of a tokio interval fires immediately, so this is
                // the one ask, sent as soon as the port is open. See above for why
                // there is never a second.
                if waited == 1 && write_tx.send(HostToBridge::Identify).is_err() {
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
///
/// It does **not** make that close safe, and cannot: a `tcflush` empties the tty's
/// own queue, and the packet the USB layer has already accepted is past it. Not
/// writing a second packet is what keeps the close short, which is [`connect`]'s
/// single `Identify`.
struct Shutdown {
    state: Arc<Attempt>,
    port: Box<dyn serialport::SerialPort>,
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Relaxed);
        let _ = self.port.clear(serialport::ClearBuffer::Output);
    }
}

/// The OS's reason a port would not open, with the remedy for the one that has one.
///
/// Permission is the failure worth separating: on Linux a serial port belongs to
/// `dialout`, and "no bridge found; name one with --bridge" is exactly the wrong
/// advice for a board that was found and refused.
fn open_failure(path: &str, error: &serialport::Error) -> String {
    if matches!(error.kind(), serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)) {
        return TransportError::Forbidden { port: path.to_owned() }.to_string();
    }
    TransportError::Open { port: path.to_owned(), source: error.clone() }.to_string()
}

/// Blocking read loop. Returns the reason it stopped.
fn read_loop(
    mut port: Box<dyn serialport::SerialPort>,
    events: &mpsc::Sender<LinkEvent>,
    state: &Attempt,
    memory: &BridgeMemory,
) -> String {
    let mut acc = FrameAccumulator::<MAX_FRAME>::new();
    let mut buf = [0u8; 1024];
    // Local rather than shared: a property of one connection, not of the link.
    let mut last_uptime: Option<u32> = None;

    while !state.stop.load(Ordering::Relaxed) {
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
                    panel,
                    ..
                }) => {
                    state.decoded.store(true, Ordering::Relaxed);
                    state.announced.store(true, Ordering::Relaxed);
                    // Read by the sweep, which stops here and holds this board for
                    // the rest of the run rather than reaching for another. The
                    // address is written down here rather than when the connection
                    // ends, because a command as short as `wartui status` exits with
                    // its connection still open — and remembering the board it just
                    // used is most of the point of keeping the file at all.
                    let first =
                        state.mac.lock().unwrap_or_else(|e| e.into_inner()).replace(mac).is_none();
                    if first {
                        memory.remember(mac);
                    }
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
                        panel = ?panel,
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
                        panel,
                    })
                }
                Ok(msg) => {
                    state.decoded.store(true, Ordering::Relaxed);
                    LinkEvent::Message(msg)
                }
                // Reset banners and half-frames land here; the framing has already
                // resynchronised, so this is a counter rather than a fault. At
                // `debug`, or a bad cable writes the log as fast as the bridge talks.
                Err(e) => {
                    state.garbled.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!(error = %e, "undecodable frame");
                    LinkEvent::Garbled(e)
                }
            };
            if events.blocking_send(event).is_err() {
                return "engine stopped listening".to_owned();
            }
        }
    }
    if state.announced.load(Ordering::Relaxed) {
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

/// Whether this board's silence says it has stopped being the bridge.
///
/// Silence alone says nothing, and each of the three guards here is a way of being
/// silent for a reason that is not the board's:
///
/// - **It has to be the board the file names.** Any other board's silence is just a
///   board that was never the bridge.
/// - **The port has to have opened.** One that would not said nothing because
///   nothing was asked of it: ModemManager holds a fresh CDC-ACM device for a few
///   seconds on some distributions, and a missing `dialout` group holds it for ever.
/// - **There has to have been nothing else to try.** A board with another waiting
///   gets [`PROBE_TICKS`], and a bridge still bringing its radio up takes longer
///   than that. Concluding from a probe is concluding from impatience.
///
/// This is not the usual way the file is corrected, and not the important one:
/// whatever board does answer writes its own address over it. This is for the case
/// where nothing answers at all, which is the only one that leaves a wrong entry
/// standing.
fn silence_disowns_it(
    alone: bool,
    opened: bool,
    remembered: Option<Mac>,
    candidate: &PortCandidate,
) -> bool {
    alone && opened && remembered.is_some() && candidate.mac() == remembered
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
    use super::{is_a_new_life, silence_disowns_it};
    use crate::ports::{BRIDGE_PID, ESPRESSIF_VID, PortCandidate, candidate, parse_mac};

    const BRIDGE_MAC: &str = "10:BD:A3:EC:44:C0";
    const NODE_MAC: &str = "02:00:5E:10:9D:24";

    fn board(mac: &str) -> PortCandidate {
        candidate("/dev/ttyACM0", Some(ESPRESSIF_VID), Some(BRIDGE_PID), Some(mac))
    }

    #[test]
    fn a_remembered_board_that_was_heard_out_and_said_nothing_is_disowned() {
        assert!(silence_disowns_it(true, true, parse_mac(BRIDGE_MAC), &board(BRIDGE_MAC)));
    }

    #[test]
    fn a_board_that_would_not_open_is_not_disowned() {
        // It said nothing because nothing was asked of it — ModemManager holding a
        // fresh device, or a missing `dialout` group. Forgetting the bridge over
        // either throws away the right answer for a reason that is not the board's.
        assert!(!silence_disowns_it(true, false, parse_mac(BRIDGE_MAC), &board(BRIDGE_MAC)));
    }

    #[test]
    fn a_board_given_only_a_probe_is_not_disowned() {
        // A bridge power-cycled alongside a node takes longer than `PROBE_TICKS` to
        // bring its radio up, and would otherwise be forgotten for being slow.
        assert!(!silence_disowns_it(false, true, parse_mac(BRIDGE_MAC), &board(BRIDGE_MAC)));
    }

    #[test]
    fn another_boards_silence_says_nothing_about_the_one_remembered() {
        assert!(!silence_disowns_it(true, true, parse_mac(BRIDGE_MAC), &board(NODE_MAC)));
    }

    #[test]
    fn with_nothing_remembered_there_is_nothing_to_disown() {
        assert!(!silence_disowns_it(true, true, None, &board(BRIDGE_MAC)));
    }

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
