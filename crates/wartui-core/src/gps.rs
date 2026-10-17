//! A GPS receiver, read as NMEA: a puck on a host serial port, or the bridge's own receiver.
//!
//! The puck reader runs on its own thread and publishes its last fix into a shared cell. Nothing
//! above blocks on a satellite: [`Gps::latest`] is a mutex and a copy, so
//! [`crate::PositionChain::resolve`] stays cheap and synchronous for the engine.
//!
//! **A bridge's receiver has no thread.** Its bytes arrive on the link, and the runtime hands them
//! to [`Gps::feed_bytes`], which splits UBX from NMEA ([`crate::ubx::Demux`]) and feeds the same
//! parser a puck does. While the connected bridge's receiver has a fresh fix
//! ([`Gps::has_fresh_fix`]), the puck reader stands down ([`Gps::set_suspended`]): it lets go of
//! its port and opens nothing, so one capture reads one receiver. The rule keys on a fix rather
//! than on sentences: a receiver with no sky still sends no-fix sentences every epoch, and standing
//! a puck with a fix down for it would leave every row unpositioned. A port the operator named with
//! `--gps` never stands down: reading it is their instruction.
//!
//! The bridge's stream restarts with each connection ([`Gps::reset_stream`]): a sentence cut off by
//! a disconnect must not join the first bytes after the reconnect. Only a fix from the new stream
//! counts toward standing down, so a reconnect does not stand the puck down on what the bridge said
//! before it. The retained fix still serves the chain until it ages out.
//!
//! **The reader stamps arrival time itself**, as the only thing that knows when a byte turned up.
//! [`crate::PositionChain`] decides whether that age is too much.
//! Diagnostics use the same age limit, including on read timeouts, without changing the retained
//! fix or the screen status. Logging belongs here so resolving a position stays free of effects.
//!
//! The sky starts over whenever the reader settles on a port or gives one up, so a lost or replaced
//! receiver shows none of the old one's satellites. The retained fix is not reset: the chain ages it.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::discover::{self, BAUD_LADDER, PROBE_WINDOW};
use crate::nmea::{Nmea, NmeaError, Report, Sky};
use crate::position::{Fix, PositionSource, is_fresh};
use crate::ubx::{Demux, ValsetReply};

/// Longest line worth assembling. NMEA 0183 caps a sentence at 82 characters. This leaves room for
/// proprietary sentences that ignore that, yet stops a receiver spewing binary from growing the
/// buffer.
const MAX_LINE: usize = 256;

/// How long to block in a read before looking at the stop flag again.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// First retry delay after the port fails, doubling to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(250);

/// Slowest retry: an unplugged puck is worth checking for every few seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How long a port must stay readable before the backoff is forgiven. A device node left behind
/// after an unplug opens and then fails its first read, so forgiving on open would retry it four
/// times a second all capture.
const HEALTHY: Duration = Duration::from_secs(2);

/// How many times a port that has worked is retried before searching again. A receiver put back in
/// the same socket keeps its `by-id` name, so the common unplug costs a reconnection, not a walk of
/// the ladder. The search resumes when the path leaves the enumeration or these are spent.
const RETRIES_BEFORE_SEARCHING: u32 = 3;

/// Which port to read, and how fast, or neither, and go and find out.
#[derive(Debug, Clone)]
pub struct GpsConfig {
    /// A device path the operator named. `None` searches for one.
    pub port: Option<String>,
    /// A line rate the operator named. `None` walks [`BAUD_LADDER`].
    pub baud: Option<u32>,
    /// Ports something else has claimed, which the search must not open.
    pub reserved: Vec<String>,
    /// How old a fix may be before the reader reports it stale. Match the position chain's limit.
    pub max_age: Duration,
}

impl Default for GpsConfig {
    fn default() -> Self {
        Self { port: None, baud: None, reserved: Vec::new(), max_age: Duration::from_secs(5) }
    }
}

impl GpsConfig {
    /// Go and find a receiver.
    #[must_use]
    pub fn search() -> Self {
        Self::default()
    }

    /// Read this port and no other.
    #[must_use]
    pub fn pinned(port: impl Into<String>) -> Self {
        Self { port: Some(port.into()), ..Self::default() }
    }

    /// Use this rate rather than walking the ladder.
    #[must_use]
    pub fn at_baud(mut self, baud: u32) -> Self {
        self.baud = Some(baud);
        self
    }

    /// Leave these ports alone, whatever they turn out to be.
    #[must_use]
    pub fn reserving(mut self, paths: Vec<String>) -> Self {
        self.reserved = paths;
        self
    }

    /// The rates to try, in order.
    fn ladder(&self) -> Vec<u32> {
        self.baud.map_or_else(|| BAUD_LADDER.to_vec(), |baud| vec![baud])
    }
}

/// What the receiver is doing, in terms fit to put on screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GpsStatus {
    /// The port has not been opened yet.
    Connecting,
    /// Listening to a port at a rate, to find out whether it is a receiver.
    Scanning {
        /// The port being listened to.
        port: String,
        /// The rate being tried.
        baud: u32,
    },
    /// Every port was listened to and none was a receiver. Distinct from [`GpsStatus::Failed`]
    /// because it is not a fault and shows nothing (`nothing_found` says why).
    NoReceiver,
    /// Sentences are arriving and the receiver says it has no fix. Normal for
    /// the first half-minute after a cold start, and for indoors forever.
    Searching,
    /// A position arrived.
    Fixed {
        /// Satellites in the solution, when the receiver said.
        satellites: Option<u8>,
    },
    /// The port could not be opened or stopped reading. The thread keeps
    /// retrying; this is what it will say in the meantime.
    Failed(String),
}

/// Totals worth showing, so a receiver talking a dialect this parser does not
/// understand looks different from one that is not talking at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GpsCounters {
    /// Sentences that parsed, whether or not they carried a position.
    pub sentences: u64,
    /// Positions taken from them.
    pub fixes: u64,
    /// Lines that failed a checksum or a field. One or two at start-up is the reader
    /// joining a stream mid-sentence; a steady flow is the wrong baud rate.
    pub rejected: u64,
}

/// The receiver's state as of a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct GpsView {
    /// What it is doing.
    pub status: GpsStatus,
    /// What it has done.
    pub counters: GpsCounters,
    /// Unix milliseconds the last fix arrived, if one ever has.
    pub last_fix_ms: Option<i64>,
    /// The port and rate being read, once the search has settled on one.
    pub settled: Option<(String, u32)>,
    /// Whether the operator chose the rate rather than the ladder. Only then is `--gps-baud` worth
    /// mentioning: a rate the ladder chose already produced valid sentences, so later unreadable
    /// lines mean something else.
    pub pinned_baud: bool,
    /// What the receiver says about its fix and its sky, for the view's GPS modal.
    pub sky: Sky,
    /// Unix milliseconds the last sentence parsed, if one ever has. The sky is as of then.
    pub heard_ms: Option<i64>,
    /// How long the receiver may go quiet before its sentences count as stale (`--gps-max-age`).
    pub max_age: Duration,
    /// Whether this is the bridge's receiver, read over the link rather than a host port.
    pub via_bridge: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Diagnostic {
    Status(GpsStatus),
    Fresh,
    Stale,
}

#[derive(Debug)]
struct Inner {
    nmea: Nmea,
    fix: Option<Fix>,
    received_at_ms: Option<i64>,
    heard_ms: Option<i64>,
    status: GpsStatus,
    counters: GpsCounters,
    settled: Option<(String, u32)>,
    pinned_baud: bool,
    diagnostic: Option<Diagnostic>,
    last_failure: Option<String>,
    max_age: Duration,
    via_bridge: bool,
    /// The bridge's stream, split and reassembled. Unused by the puck reader, which keeps its own
    /// [`Lines`] on its thread.
    demux: Demux,
    lines: Lines,
    /// Whether a fix has arrived since the stream last restarted ([`Gps::reset_stream`]).
    fix_in_stream: bool,
    /// The receiver's last answer to its rate, so a repeated answer is logged once.
    valset: Option<ValsetReply>,
    /// Whether the receiver has said its transmit buffer is full, so that is logged once.
    txbuf_full: bool,
}

impl Inner {
    fn set_status(&mut self, status: GpsStatus) {
        self.status = status.clone();
        if matches!(status, GpsStatus::Scanning { .. }) {
            tracing::debug!(status = ?status, "gps scanning");
            return;
        }
        // Reopening a port is not recovery: it can fail its first read on every retry.
        if status == GpsStatus::Searching && self.last_failure.is_some() {
            return;
        }
        self.note_status(status);
    }

    fn note_status(&mut self, status: GpsStatus) {
        let diagnostic = Diagnostic::Status(status.clone());
        if self.diagnostic.as_ref() == Some(&diagnostic) {
            return;
        }
        match &status {
            GpsStatus::Failed(reason) => {
                if self.last_failure.as_ref() != Some(reason) {
                    tracing::warn!(reason = %reason, "gps unreadable");
                    self.last_failure = Some(reason.clone());
                }
            }
            other => tracing::info!(status = ?other, "gps"),
        }
        self.diagnostic = Some(diagnostic);
    }

    fn tick(&mut self, now_ms: i64) {
        if self.diagnostic == Some(Diagnostic::Fresh)
            && let Some(at_ms) = self.received_at_ms
            && !is_fresh(at_ms, now_ms, self.max_age)
        {
            tracing::warn!(last_fix_ms = at_ms, max_age_ms = ?self.max_age.as_millis(), "gps fix stale");
            self.diagnostic = Some(Diagnostic::Stale);
        }
    }

    /// Drop what the last receiver said about its sky, so a lost or replaced one shows no
    /// satellites it is not reporting. The retained fix and status belong to the position chain.
    fn forget_receiver(&mut self) {
        self.nmea = Nmea::new();
        self.heard_ms = None;
    }

    fn note_fix(&mut self) {
        self.last_failure = None;
        if self.diagnostic != Some(Diagnostic::Fresh) {
            if self.received_at_ms.is_none() {
                tracing::info!("gps fix acquired");
            } else {
                tracing::info!("gps fix recovered");
            }
            self.diagnostic = Some(Diagnostic::Fresh);
        }
    }

    /// The host writes the rate with every bridge status poll, so the receiver answers every few
    /// seconds. Only a change is worth a line. Takes the field rather than `self`, so the demux's
    /// packet callback can hold it while the line callback holds the parser.
    fn note_valset(seen: &mut Option<ValsetReply>, reply: ValsetReply) {
        if *seen == Some(reply) {
            return;
        }
        match reply {
            ValsetReply::Accepted => tracing::debug!("the GPS receiver took its rate"),
            ValsetReply::Refused => tracing::warn!("the GPS receiver refused its rate"),
        }
        *seen = Some(reply);
    }

    /// A `TXT` sentence saying `txbuf` is how a u-blox receiver reports dropping its own output.
    fn note_txt(&mut self, line: &[u8]) {
        let txt = line.get(3..6) == Some(b"TXT");
        if txt && !self.txbuf_full && line.windows(5).any(|w| w == b"txbuf") {
            tracing::warn!("the GPS receiver is dropping output: its transmit buffer is full");
            self.txbuf_full = true;
        }
    }

    /// Offer one line to the parser, as of `now_ms`.
    fn feed(&mut self, line: &[u8], now_ms: i64) {
        self.tick(now_ms);
        self.feed_line(line, now_ms);
    }

    /// [`Self::feed`] without the tick, for a batch of lines that has already ticked once.
    fn feed_line(&mut self, line: &[u8], now_ms: i64) {
        let parsed = self.nmea.parse(line);
        if parsed.is_ok() {
            self.heard_ms = Some(now_ms);
        }
        match parsed {
            Ok(Report::Fix(new)) => {
                self.note_fix();
                self.counters.sentences += 1;
                self.counters.fixes += 1;
                // A cycle is two sentences about one instant, and only GGA carries altitude,
                // satellites and dilution. So the position is replaced and the rest carried: what a
                // sentence omits, it does not contradict.
                let previous = self.fix;
                let satellites = new.satellites.or(match self.status {
                    GpsStatus::Fixed { satellites } => satellites,
                    _ => None,
                });
                self.status = GpsStatus::Fixed { satellites };
                self.fix = Some(Fix {
                    lat: Some(new.lat),
                    lon: Some(new.lon),
                    alt: new.alt.or_else(|| previous.and_then(|fix| fix.alt)),
                    accuracy: new.accuracy.or_else(|| previous.and_then(|fix| fix.accuracy)),
                    source: PositionSource::Gps,
                    at_ms: new.at_ms,
                });
                self.received_at_ms = Some(now_ms);
                self.fix_in_stream = true;
            }
            Ok(Report::NoFix) => {
                self.last_failure = None;
                self.counters.sentences += 1;
                // Keep the last fix: losing lock under a bridge has not moved the host, and the
                // chain's staleness rule decides when to stop believing it.
                if !matches!(self.status, GpsStatus::Fixed { .. }) {
                    self.status = GpsStatus::Searching;
                    self.note_status(GpsStatus::Searching);
                }
            }
            Ok(Report::Other) => {
                self.counters.sentences += 1;
                self.note_txt(line);
            }
            Err(NmeaError::NotASentence | NmeaError::Checksum | NmeaError::Malformed) => {
                self.counters.rejected += 1;
            }
        }
    }
}

/// A handle on the receiver, cheap to clone and safe to share.
#[derive(Debug, Clone)]
pub struct Gps {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
    /// Set while the connected bridge's receiver has a fresh fix: the puck reader holds no port and
    /// searches for none.
    suspended: Arc<AtomicBool>,
    /// Whether the operator named the port, which [`Gps::set_suspended`] then leaves alone.
    pinned: bool,
}

impl Gps {
    /// A receiver with nothing behind it: it holds state and accepts [`Gps::feed`]
    /// with no thread reading a port, which is what makes the chain testable.
    #[must_use]
    pub fn detached() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                nmea: Nmea::new(),
                fix: None,
                received_at_ms: None,
                heard_ms: None,
                status: GpsStatus::Connecting,
                counters: GpsCounters::default(),
                settled: None,
                pinned_baud: false,
                diagnostic: None,
                last_failure: None,
                max_age: GpsConfig::default().max_age,
                via_bridge: false,
                demux: Demux::new(),
                lines: Lines::default(),
                fix_in_stream: false,
                valset: None,
                txbuf_full: false,
            })),
            stop: Arc::new(AtomicBool::new(false)),
            suspended: Arc::new(AtomicBool::new(false)),
            pinned: false,
        }
    }

    /// The bridge's receiver: detached, and fed by the runtime through [`Gps::feed_bytes`].
    /// `max_age` is the staleness limit its diagnostics and [`Gps::has_fresh_fix`] use, as
    /// [`GpsConfig::max_age`] is for a puck.
    #[must_use]
    pub fn bridged(max_age: Duration) -> Self {
        let gps = Self::detached();
        {
            let mut inner = gps.lock();
            inner.via_bridge = true;
            inner.max_age = max_age;
        }
        gps
    }

    /// Open `config.port` on a background thread and keep it open, reconnecting if the receiver is
    /// unplugged and put back. Returns at once: observations with no position are still worth
    /// having.
    #[must_use]
    pub fn spawn(config: GpsConfig) -> Self {
        let gps = Self { pinned: config.port.is_some(), ..Self::detached() };
        {
            let mut inner = gps.lock();
            inner.pinned_baud = config.baud.is_some();
            inner.max_age = config.max_age;
        }
        let worker = gps.clone();
        let started = std::thread::Builder::new()
            .name("wartui-gps".to_owned())
            .spawn(move || worker.read_forever(&config));
        if let Err(e) = started {
            gps.set_status(GpsStatus::Failed(format!("starting the reader thread: {e}")));
        }
        gps
    }

    /// Ask the reader thread to finish. Idempotent, and harmless when detached.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    /// Stand the puck reader down, or let it search again. While suspended it lets go of any port
    /// within [`READ_TIMEOUT`], opens none, and forgets the sky. Cheap and idempotent, so the
    /// runtime calls it on every tick; only a change is logged. A no-op for a port the operator
    /// named with `--gps`, and harmless when detached.
    pub fn set_suspended(&self, suspended: bool) {
        if self.pinned || self.suspended.swap(suspended, Ordering::Relaxed) == suspended {
            return;
        }
        if suspended {
            tracing::info!("gps search stands down: the bridge's receiver has a fix");
        } else {
            tracing::info!("gps search resumes");
        }
    }

    /// Whether a fix arrived within the staleness limit of `now_ms`, and since the stream last
    /// restarted ([`Self::reset_stream`]). Sentences without a fix do not count.
    #[must_use]
    pub fn has_fresh_fix(&self, now_ms: i64) -> bool {
        let inner = self.lock();
        inner.fix_in_stream
            && inner.received_at_ms.is_some_and(|at_ms| is_fresh(at_ms, now_ms, inner.max_age))
    }

    /// Whether the operator named this receiver's port with `--gps`.
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// [`Self::detached`], as if the operator had named its port.
    #[cfg(test)]
    pub(crate) fn detached_pinned() -> Self {
        Self { pinned: true, ..Self::detached() }
    }

    /// Start the bridge's stream over: drop a partial line or UBX packet, so bytes from before a
    /// reconnect never join the next stream, and require a new fix before
    /// [`Self::has_fresh_fix`] holds. The retained fix and the sky stay, as the chain and the view
    /// age them.
    pub fn reset_stream(&self) {
        let mut inner = self.lock();
        inner.demux = Demux::new();
        inner.lines = Lines::default();
        inner.fix_in_stream = false;
    }

    /// The last fix and the Unix milliseconds it arrived.
    #[must_use]
    pub fn latest(&self) -> Option<(Fix, i64)> {
        let inner = self.lock();
        Some((inner.fix?, inner.received_at_ms?))
    }

    /// What to say about the receiver on screen.
    #[must_use]
    pub fn view(&self) -> GpsView {
        let inner = self.lock();
        GpsView {
            status: inner.status.clone(),
            counters: inner.counters,
            last_fix_ms: inner.received_at_ms,
            settled: inner.settled.clone(),
            pinned_baud: inner.pinned_baud,
            sky: inner.nmea.sky().clone(),
            heard_ms: inner.heard_ms,
            max_age: inner.max_age,
            via_bridge: inner.via_bridge,
        }
    }

    /// Offer one line to the parser, as of `now_ms`. Public as the seam the reader thread and the
    /// tests share.
    pub fn feed(&self, line: &[u8], now_ms: i64) {
        self.lock().feed(line, now_ms);
    }

    /// Offer a piece of the bridge's receiver stream, as of `now_ms`, in whatever pieces the
    /// bridge read it. Complete NMEA lines reach the parser. The receiver's answer to its rate is
    /// logged, and other UBX is ignored.
    pub fn feed_bytes(&self, bytes: &[u8], now_ms: i64) {
        let mut guard = self.lock();
        let inner = &mut *guard;
        inner.tick(now_ms);
        // Out of `inner` for the call, so the line callback can feed the parser it holds while the
        // packet callback notes the receiver's answer.
        let mut demux = std::mem::take(&mut inner.demux);
        let mut lines = std::mem::take(&mut inner.lines);
        let mut valset = inner.valset.take();
        demux.push(
            bytes,
            |run| lines.push(run, |line| inner.feed_line(line, now_ms)),
            |packet| {
                if let Some(reply) = packet.valset_reply() {
                    Inner::note_valset(&mut valset, reply);
                }
            },
        );
        inner.demux = demux;
        inner.lines = lines;
        inner.valset = valset;
    }

    /// Age the retained fix as of `now_ms`, for the staleness log. The puck reader does this on
    /// every read. A bridge's receiver that has gone quiet sends no bytes to do it, so the runtime
    /// calls this on its tick.
    pub fn tick(&self, now_ms: i64) {
        self.lock().tick(now_ms);
    }

    fn set_status(&self, status: GpsStatus) {
        self.lock().set_status(status);
    }

    pub(crate) fn suspended(&self) -> bool {
        self.suspended.load(Ordering::Relaxed)
    }

    /// Whether the reader should let go of what it is doing: stopping, or standing down.
    fn halted(&self) -> bool {
        self.stop.load(Ordering::Relaxed) || self.suspended()
    }

    /// Sleep for `total`, in steps no longer than [`READ_TIMEOUT`], ending early when halted.
    fn pause(&self, total: Duration) {
        let until = Instant::now() + total;
        while !self.halted() {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            std::thread::sleep(left.min(READ_TIMEOUT));
        }
    }

    /// Hold no port and search for none until resumed or stopped. The sky and the settled port
    /// are forgotten, since the receiver read on resume may be another one. The status is left at
    /// [`GpsStatus::Connecting`]: standing down is not a fault.
    fn stand_down(&self) {
        {
            let mut inner = self.lock();
            inner.settled = None;
            inner.forget_receiver();
            inner.status = GpsStatus::Connecting;
            inner.diagnostic = None;
            inner.last_failure = None;
        }
        while self.suspended() && !self.stop.load(Ordering::Relaxed) {
            std::thread::sleep(READ_TIMEOUT);
        }
    }

    /// A poisoned mutex means a previous holder panicked mid-update; what it left is
    /// still readable and better than taking the capture down over a position.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Find a receiver, read it, and look again when it goes away.
    ///
    /// The search runs here because nothing waits for it: a capture starts at once and takes
    /// whatever position exists as each row is written, so seconds spent listening cost nothing. It
    /// also lets a receiver survive moving to another socket, which a reader given one literal path
    /// cannot.
    fn read_forever(&self, config: &GpsConfig) {
        let mut backoff = MIN_BACKOFF;
        // What the search settled on, kept across reconnections: a puck put back is the same port
        // at the same rate.
        let mut settled: Option<(String, u32)> = None;
        // Why reading stopped. A working receiver that stopped is news, either way. One that was
        // never there is not.
        let mut lost: Option<String> = None;
        let mut retries = 0_u32;

        while !self.stop.load(Ordering::Relaxed) {
            if self.suspended() {
                self.stand_down();
                // Resuming is a fresh search.
                backoff = MIN_BACKOFF;
                settled = None;
                lost = None;
                retries = 0;
                continue;
            }
            let searching = settled.is_none();
            let Some((port, baud)) =
                settled.clone().or_else(|| self.search(config, lost.is_none() && searching))
            else {
                if self.halted() {
                    continue;
                }
                self.set_status(nothing_found(config.port.as_deref(), lost.as_deref(), why_not));
                self.pause(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            };
            if settled.is_none() {
                settled = Some((port.clone(), baud));
                lost = None;
                retries = 0;
                let mut inner = self.lock();
                inner.settled = Some((port.clone(), baud));
                inner.forget_receiver();
                tracing::info!(port = %port, baud, "reading a receiver");
            }

            match open_port(&port, baud) {
                Ok(handle) => {
                    if !matches!(self.view().status, GpsStatus::Fixed { .. }) {
                        self.set_status(GpsStatus::Searching);
                    }
                    let opened = Instant::now();
                    let reason = self.read_port(handle);
                    if self.stop.load(Ordering::Relaxed) {
                        return;
                    }
                    if self.suspended() {
                        continue;
                    }
                    if opened.elapsed() >= HEALTHY {
                        backoff = MIN_BACKOFF;
                        retries = 0;
                    }
                    self.set_status(GpsStatus::Failed(reason));
                }
                Err(_) if self.halted() => continue,
                Err(e) => self.set_status(GpsStatus::Failed(format!("{port}: {e}"))),
            }

            // Look past a port that keeps failing or has gone, unless the operator named it:
            // searching would answer a question they did not ask.
            retries += 1;
            let gone = !still_there(&port);
            if config.port.is_none() && (retries > RETRIES_BEFORE_SEARCHING || gone) {
                // Say what was established. A port that left the enumeration was unplugged. One
                // still there that stopped talking is reconfigured, a failing cable, or held by
                // another program, and sending the operator to check a plugged-in cable wastes the
                // one thing the note buys.
                lost = Some(if gone {
                    format!("{port} is no longer attached")
                } else {
                    format!("{port} stopped sending NMEA")
                });
                settled = None;
                let mut inner = self.lock();
                inner.settled = None;
                inner.forget_receiver();
            }
            self.pause(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Listen to each candidate port at each rate until one is a receiver. `announce` is false
    /// while re-searching after a loss, so the note saying it has gone stays up. The scan is most
    /// of each cycle, and the operator needs the reason, not the activity.
    fn search(&self, config: &GpsConfig, announce: bool) -> Option<(String, u32)> {
        // Port and rate both named leaves detection nothing to decide. Reading is then the
        // operator's instruction, and a probe would refuse a working receiver for saying too little
        // in one window, as one set to a single sentence a second does.
        if let (Some(path), Some(baud)) = (&config.port, config.baud) {
            return Some((path.clone(), baud));
        }
        let ladder = config.ladder();
        let (ports, needed) = match &config.port {
            // A named path says nothing about rate, so the ladder still runs. One sentence settles
            // each rate, since the operator has said what the device is.
            Some(path) => (
                vec![wartui_bridge::ports::candidate(path, None, None, None)],
                discover::SENTENCES_ON_A_NAMED_PORT,
            ),
            None => match wartui_bridge::ports::list() {
                Ok(attached) => (
                    discover::candidates(attached, &config.reserved),
                    discover::SENTENCES_TO_BELIEVE,
                ),
                Err(e) => {
                    tracing::debug!(error = %e, "could not list serial ports");
                    return None;
                }
            },
        };
        discover::settle(&ports, &ladder, needed, |path, baud| {
            if announce {
                self.set_status(GpsStatus::Scanning { port: path.to_owned(), baud });
            }
            self.listen(path, baud)
        })
        .map(|(path, baud)| (path.to_owned(), baud))
    }

    /// Read whatever a port has to say for [`PROBE_WINDOW`], and say nothing back.
    fn listen(&self, path: &str, baud: u32) -> Vec<u8> {
        use std::io::Read;

        // Standing down opens nothing, so the rest of the search falls through at once.
        if self.halted() {
            return Vec::new();
        }
        let Ok(mut port) = open_port(path, baud) else { return Vec::new() };
        let mut sample = Vec::new();
        let mut buf = [0u8; 512];
        let until = Instant::now() + PROBE_WINDOW;
        // Short reads accumulated, so `stop` is noticed within the read timeout and shutdown does
        // not wait out the window.
        while Instant::now() < until && !self.halted() {
            match port.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => sample.extend_from_slice(&buf[..n]),
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::Interrupted) => {}
                Err(_) => break,
            }
        }
        sample
    }

    /// Read until the port fails, returning why in terms fit to show a user.
    fn read_port(&self, mut port: Box<dyn serialport::SerialPort>) -> String {
        use std::io::Read;

        let mut lines = Lines::default();
        let mut buf = [0u8; 512];
        while !self.halted() {
            match port.read(&mut buf) {
                Ok(0) => return "the receiver closed the port".to_owned(),
                Ok(n) => {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    lines.push(&buf[..n], |line| self.feed(line, now_ms));
                }
                // Silence does not break the port, but it still ages the retained fix.
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return e.to_string(),
            }
            self.tick(chrono::Utc::now().timestamp_millis());
        }
        "stopped".to_owned()
    }
}

/// What to say when a search came back with nothing.
///
/// This is what makes searching by default bearable. A receiver nobody asked for or attached is not
/// a fault, and a view with one line to say anything on must not spend it saying most captures have
/// no GPS. So that case is silent, and the search runs again in case one is plugged in mid-capture.
///
/// A receiver the operator *named* is the opposite: silence would leave a capture recording no
/// position for no stated reason. So is one that *was* being read and has gone. A puck out of its
/// socket halfway down a road decides whether a capture uploads, and only the operator can put it
/// back.
fn nothing_found(
    named: Option<&str>,
    lost: Option<&str>,
    why: impl FnOnce(&str) -> String,
) -> GpsStatus {
    match (named, lost) {
        (Some(path), _) => GpsStatus::Failed(why(path)),
        (None, Some(reason)) => GpsStatus::Failed(reason.to_owned()),
        (None, None) => GpsStatus::NoReceiver,
    }
}

/// Why a named port produced no receiver, in the OS's own words where it has any.
fn why_not(path: &str) -> String {
    match open_port(path, BAUD_LADDER[0]) {
        Err(e) => format!("{path}: {e}"),
        // It opened, so it is there and it is not a receiver — or not one talking
        // NMEA, which a u-blox configured to emit only UBX binary is not.
        Ok(_) => format!("{path} answered no NMEA at any rate tried"),
    }
}

/// Open a serial port the one way this crate opens one.
fn open_port(path: &str, baud: u32) -> serialport::Result<Box<dyn serialport::SerialPort>> {
    serialport::new(path, baud)
        .timeout(READ_TIMEOUT)
        // As on the bridge's port: the one setting that keeps the driver from moving
        // RTS by itself. `wartui_bridge::ports` has why that matters.
        .flow_control(serialport::FlowControl::None)
        .open()
}

/// Whether the search's path is still among the attached ports. A receiver moved to another socket
/// returns under another name, and waiting for the old one is what a reader given one literal path
/// does. A failed enumeration counts as present: failing to list is no evidence a working port has
/// gone.
fn still_there(path: &str) -> bool {
    wartui_bridge::ports::list().map_or(true, |attached| {
        attached.iter().any(|candidate| candidate.path == path || candidate.device == path)
    })
}

/// Reassembles lines from however the OS chose to split the stream.
#[derive(Debug, Default)]
pub(crate) struct Lines {
    buf: Vec<u8>,
    /// Set when a line grew past [`MAX_LINE`]: everything up to the next
    /// newline is discarded rather than kept and mis-parsed.
    overrun: bool,
}

impl Lines {
    pub(crate) fn push(&mut self, bytes: &[u8], mut yield_line: impl FnMut(&[u8])) {
        for byte in bytes {
            if *byte == b'\n' {
                if !self.overrun && !self.buf.is_empty() {
                    yield_line(&self.buf);
                }
                self.buf.clear();
                self.overrun = false;
            } else if self.buf.len() < MAX_LINE {
                self.buf.push(*byte);
            } else {
                self.buf.clear();
                self.overrun = true;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::position::DEFAULT_MAX_AGE;
    use crate::ubx::{ACK_VALSET, NAK_VALSET};

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<String>>>);

    impl tracing::Subscriber for Logs {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() > tracing::Level::INFO {
                return;
            }
            struct Fields(String);
            impl tracing::field::Visit for Fields {
                fn record_debug(
                    &mut self,
                    field: &tracing::field::Field,
                    value: &dyn std::fmt::Debug,
                ) {
                    use std::fmt::Write;
                    write!(&mut self.0, "{}={value:?} ", field.name()).unwrap();
                }
            }
            let mut fields = Fields(String::new());
            event.record(&mut fields);
            self.0.lock().unwrap().push(fields.0);
        }

        fn enter(&self, _: &tracing::span::Id) {}

        fn exit(&self, _: &tracing::span::Id) {}
    }

    impl Logs {
        fn entries(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }

        fn assert_messages(&self, messages: &[&str]) {
            let entries = self.entries();
            assert_eq!(entries.len(), messages.len(), "{entries:?}");
            for (entry, message) in entries.iter().zip(messages) {
                assert!(entry.contains(message), "expected {message:?} in {entry:?}");
            }
        }
    }

    #[test]
    fn gps_logs_first_fix_when_successful_sentence_arrives() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.feed(GGA, 1_000);
        });
        let entries = logs.entries();
        assert_eq!(entries.len(), 1, "{entries:?}");
        assert!(entries[0].contains("gps fix acquired"), "{entries:?}");
    }

    #[test]
    fn gps_bounds_logs_when_fixes_and_satellite_counts_repeat() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            for now_ms in 1_000..1_100 {
                gps.feed(GGA, now_ms);
                gps.feed(
                    b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73",
                    now_ms,
                );
                gps.feed(
                    b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,09,0.9,545.4,M,46.9,M,,*68",
                    now_ms,
                );
            }
            assert_eq!(gps.view().counters.fixes, 300);
            assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(9) });
        });
        logs.assert_messages(&["gps fix acquired"]);
    }

    #[test]
    fn gps_logs_stale_and_recovery_when_configured_age_is_exceeded() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.lock().max_age = Duration::from_secs(2);
            gps.feed(GGA, 1_000);
            let latest = gps.latest();
            let status = gps.view().status;
            gps.tick(999);
            gps.tick(3_000);
            logs.assert_messages(&["gps fix acquired"]);
            gps.tick(3_001);
            gps.tick(30_000);
            logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
            assert_eq!(gps.latest(), latest);
            assert_eq!(gps.view().status, status);
            gps.feed(GGA, 30_001);
            gps.feed(GGA, 30_002);
            logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
        });
    }

    #[test]
    fn gps_logs_stale_once_when_receiver_is_silent() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            assert_eq!(GpsConfig::default().max_age, Duration::from_secs(5));
            gps.feed(GGA, 1_000);
            gps.tick(6_000);
            logs.assert_messages(&["gps fix acquired"]);
            for now_ms in 6_001..6_100 {
                gps.tick(now_ms);
            }
            assert_eq!(gps.latest().unwrap().1, 1_000);
            assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
        });
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    #[test]
    fn gps_bounds_searching_logs_when_receiver_reports_no_fix() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            for now_ms in 1_000..1_100 {
                gps.feed(GGA_NO_FIX, now_ms);
            }
            gps.tick(60_000);
            assert_eq!(gps.latest(), None);
            assert_eq!(gps.view().status, GpsStatus::Searching);
            gps.feed(GGA, 60_001);
        });
        logs.assert_messages(&["Searching", "gps fix acquired"]);
    }

    #[test]
    fn gps_logs_stale_when_no_fix_sentences_outlast_retained_fix() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.feed(GGA, 1_000);
            for now_ms in [2_000, 6_000] {
                gps.feed(GGA_NO_FIX, now_ms);
            }
            logs.assert_messages(&["gps fix acquired"]);
            for now_ms in 6_001..6_100 {
                gps.feed(GGA_NO_FIX, now_ms);
            }
            assert_eq!(gps.latest().unwrap().1, 1_000);
            assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
            gps.feed(GGA, 6_100);
        });
        logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
    }

    #[test]
    fn gps_logs_gap_when_fix_returns_without_intervening_tick() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.feed(GGA, 1_000);
            gps.feed(GGA, 6_001);
        });
        logs.assert_messages(&["gps fix acquired", "gps fix stale", "gps fix recovered"]);
    }

    #[test]
    fn gps_bounds_failure_logs_when_failed_ports_are_retried_or_removed() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.feed(GGA, 1_000);
            let latest = gps.latest();
            for _ in 0..100 {
                gps.set_status(GpsStatus::Failed("read failed".to_owned()));
                gps.set_status(GpsStatus::Searching);
            }
            let gone =
                nothing_found(None, Some("receiver is no longer attached"), |_| unreachable!());
            for _ in 0..100 {
                gps.set_status(gone.clone());
                gps.tick(60_000);
            }
            assert_eq!(gps.latest(), latest);
            assert_eq!(gps.view().status, gone);
            gps.feed(GGA, 60_001);
            gps.set_status(GpsStatus::Failed("read failed".to_owned()));
        });
        logs.assert_messages(&[
            "gps fix acquired",
            "read failed",
            "receiver is no longer attached",
            "gps fix recovered",
            "read failed",
        ]);
    }

    #[test]
    fn gps_bounds_failure_logs_when_discovery_retries_the_same_missing_receiver() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            for _ in 0..100 {
                gps.set_status(GpsStatus::Scanning { port: "missing".to_owned(), baud: 9_600 });
                gps.set_status(GpsStatus::Failed("missing receiver".to_owned()));
            }
            gps.set_status(GpsStatus::Searching);
            gps.feed(GGA_NO_FIX, 1_000);
            gps.set_status(GpsStatus::Failed("missing receiver".to_owned()));
        });
        logs.assert_messages(&["missing receiver", "Searching", "missing receiver"]);
    }

    #[test]
    fn gps_bounds_no_receiver_logs_when_discovery_repeats_without_a_receiver() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            for _ in 0..100 {
                gps.set_status(GpsStatus::Scanning { port: "candidate".to_owned(), baud: 9_600 });
                gps.set_status(GpsStatus::NoReceiver);
            }
        });
        logs.assert_messages(&["NoReceiver"]);
    }

    #[test]
    fn gps_logs_stale_when_zero_age_fix_is_followed_by_unusable_sentences() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            gps.lock().max_age = Duration::ZERO;
            gps.feed(GGA, 1_000);
            gps.tick(1_000);
            logs.assert_messages(&["gps fix acquired"]);
            gps.feed(b"not NMEA", 1_001);
            gps.feed(b"$GPGSV,1,1,00*79", 1_002);
            assert_eq!(gps.latest().unwrap().1, 1_000);
        });
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    #[test]
    fn nothing_found_is_no_receiver_when_none_named_or_lost() {
        // Searching is the default, so this is most captures. A fault here would be
        // a fault on the view of every run made without a GPS.
        assert_eq!(nothing_found(None, None, |_| unreachable!()), GpsStatus::NoReceiver);
    }

    #[test]
    fn nothing_found_fails_when_named_port_missing() {
        let status =
            nothing_found(Some("/dev/ttyNOPE"), None, |path| format!("{path}: no such device"));
        assert_eq!(status, GpsStatus::Failed("/dev/ttyNOPE: no such device".to_owned()));
    }

    #[test]
    fn nothing_found_fails_when_read_port_lost() {
        // Measured on the bench: pulling the puck mid-capture left the header with
        // nothing to say about it while the rows quietly stopped carrying a
        // position. The red `pos none` line is the alarm; this is the reason.
        let gone =
            nothing_found(None, Some("/dev/ttyACM0 is no longer attached"), |_| unreachable!());
        assert_eq!(gone, GpsStatus::Failed("/dev/ttyACM0 is no longer attached".to_owned()));
        // And a port still attached that has stopped talking says that instead, so
        // nobody is sent to check a cable that is plugged in.
        let quiet =
            nothing_found(None, Some("/dev/ttyACM0 stopped sending NMEA"), |_| unreachable!());
        assert_eq!(quiet, GpsStatus::Failed("/dev/ttyACM0 stopped sending NMEA".to_owned()));
    }

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76";

    #[test]
    fn gps_has_no_fix_when_receiver_reports_none() {
        let gps = Gps::detached();
        assert_eq!(gps.latest(), None);
        gps.feed(GGA_NO_FIX, 1_000);
        assert_eq!(gps.latest(), None);
        assert_eq!(gps.view().status, GpsStatus::Searching);
    }

    #[test]
    fn gps_stamps_fix_with_arrival_time_when_fix_parsed() {
        // The two differ, and only the arrival time can say how stale a fix
        // is: the receiver's own clock is the thing being reported on.
        let gps = Gps::detached();
        gps.feed(b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73", 5_000);
        let (fix, at) = gps.latest().expect("a fix");
        assert_eq!(at, 5_000);
        assert_eq!(fix.source, PositionSource::Gps);
        assert_eq!(fix.at_ms, Some(1_788_611_719_000));
    }

    #[test]
    fn gps_keeps_gga_altitude_and_accuracy_when_rmc_follows() {
        // Receivers emit GGA and RMC every cycle, RMC normally last, so
        // whatever the RMC does not carry is what the fix spends its life as.
        // Only GGA has an altitude or an HDOP, and both have a column waiting
        // for them in the export.
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(b"$GNRMC,123519.00,A,4807.038,N,01131.000,E,0.06,31.66,050926,,,A*73", 1_100);
        let (fix, _) = gps.latest().expect("a fix");
        assert_eq!(fix.alt, Some(545.4));
        assert_eq!(fix.accuracy, Some(4.5));
        assert_eq!(gps.view().status, GpsStatus::Fixed { satellites: Some(8) });
    }

    #[test]
    fn gps_keeps_last_fix_when_lock_lost() {
        // Driving under a bridge should not blank the position of every
        // observation for the next second. Age is what disqualifies a fix.
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(GGA_NO_FIX, 2_000);
        let (_, at) = gps.latest().expect("the last fix");
        assert_eq!(at, 1_000);
        assert!(matches!(gps.view().status, GpsStatus::Fixed { .. }));
    }

    #[test]
    fn gps_counts_rejected_when_line_corrupt() {
        let gps = Gps::detached();
        gps.feed(b"\xff\xfe binary junk", 1_000);
        gps.feed(GGA, 2_000);
        let view = gps.view();
        assert_eq!(view.counters.rejected, 1);
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.last_fix_ms, Some(2_000));
    }

    #[test]
    fn gps_exposes_sky_in_view_when_sentences_fed() {
        let gps = Gps::detached();
        assert_eq!(gps.view().heard_ms, None);
        gps.feed(GGA, 1_000);
        gps.feed(b"$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69", 1_100);
        gps.feed(b"$GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1*36", 1_200);
        gps.feed(b"not NMEA", 1_300);
        let view = gps.view();
        assert_eq!(view.heard_ms, Some(1_200), "a rejected line is not hearing the receiver");
        assert_eq!(view.sky.used_count, Some(8));
        assert_eq!(view.sky.satellites.len(), 2);
        assert!(view.sky.satellites.iter().all(|s| s.used));
    }

    #[test]
    fn gps_forgets_sky_when_receiver_is_lost() {
        let gps = Gps::detached();
        gps.feed(GGA, 1_000);
        gps.feed(b"$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69", 1_100);
        gps.feed(b"$GNGSA,A,3,10,08,,,,,,,,,,,1.5,0.9,1.2,1*36", 1_200);
        let latest = gps.latest();
        assert!(latest.is_some());
        gps.lock().forget_receiver();
        let view = gps.view();
        assert_eq!(view.sky, Sky::default());
        assert_eq!(view.heard_ms, None);
        assert_eq!(gps.latest(), latest);
    }

    #[test]
    fn gps_reads_bridge_view_when_bridged() {
        let gps = Gps::bridged(Duration::from_secs(3));
        let view = gps.view();
        assert!(view.via_bridge);
        assert_eq!(view.max_age, Duration::from_secs(3));
        assert_eq!(view.status, GpsStatus::Connecting);
        assert!(!Gps::detached().view().via_bridge);
    }

    #[test]
    fn gps_fixes_from_bridge_bytes_when_sentence_spans_chunks_around_an_ack() {
        // The bridge cuts its stream wherever an I2C read ended, and the receiver answers each
        // rate write between sentences. Neither may cost the sentence or count as rejected.
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        let mut stream = GGA.to_vec();
        stream.extend_from_slice(b"\r\n");
        stream.extend_from_slice(&ACK_VALSET);
        stream.extend_from_slice(GGA_NO_FIX);
        stream.extend_from_slice(b"\r\n");
        for chunk in stream.chunks(7) {
            gps.feed_bytes(chunk, 2_000);
        }
        let view = gps.view();
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.counters.sentences, 2);
        assert_eq!(view.counters.rejected, 0);
        assert_eq!(gps.latest().map(|(_, at)| at), Some(2_000));
    }

    #[test]
    fn gps_logs_refused_rate_once_when_receiver_repeats_its_nak() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::bridged(DEFAULT_MAX_AGE);
            for now_ms in 0..10 {
                gps.feed_bytes(&NAK_VALSET, now_ms);
            }
            gps.feed_bytes(&ACK_VALSET, 10);
            gps.feed_bytes(&NAK_VALSET, 11);
            assert_eq!(gps.view().counters.rejected, 0);
        });
        logs.assert_messages(&[
            "the GPS receiver refused its rate",
            "the GPS receiver refused its rate",
        ]);
    }

    #[test]
    fn gps_logs_full_transmit_buffer_once_when_receiver_reports_txbuf() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::bridged(DEFAULT_MAX_AGE);
            for now_ms in 0..3 {
                gps.feed_bytes(b"$GNTXT,01,01,00,txbuf alloc*61\r\n", now_ms);
            }
            gps.feed_bytes(b"$GNTXT,01,01,02,ANTSTATUS=OK*25\r\n", 3);
            assert_eq!(gps.view().counters.rejected, 0);
        });
        logs.assert_messages(&["the GPS receiver is dropping output"]);
    }

    #[test]
    fn gps_logs_stale_when_bridge_receiver_goes_quiet_and_runtime_ticks() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::bridged(DEFAULT_MAX_AGE);
            let mut sentence = GGA.to_vec();
            sentence.extend_from_slice(b"\r\n");
            gps.feed_bytes(&sentence, 1_000);
            gps.tick(7_000);
        });
        logs.assert_messages(&["gps fix acquired", "gps fix stale"]);
    }

    /// Poll `done` until it holds or a few seconds pass.
    fn eventually(mut done: impl FnMut() -> bool) -> bool {
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until {
            if done() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        done()
    }

    #[test]
    fn gps_stands_down_without_a_fault_when_suspended() {
        // Rate and port both named skip the search, so the reader goes straight to opening a
        // port that is not there and reports it. A named port ignores `set_suspended`, so the
        // flag is set directly: this tests the reader standing down, not who may ask it to.
        let gps = Gps::spawn(GpsConfig::pinned("/dev/wartui-test-no-such-port").at_baud(9_600));
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Failed(_))));
        gps.suspended.store(true, Ordering::Relaxed);
        assert!(eventually(|| gps.view().status == GpsStatus::Connecting));
        // Held there, not retried into another fault.
        std::thread::sleep(MIN_BACKOFF * 2);
        assert_eq!(gps.view().status, GpsStatus::Connecting);
        assert_eq!(gps.view().settled, None);
        gps.suspended.store(false, Ordering::Relaxed);
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Failed(_))));
        gps.stop();
    }

    #[test]
    fn gps_ignores_suspension_when_port_is_pinned() {
        let gps = Gps::spawn(GpsConfig::pinned("/dev/wartui-test-no-such-port").at_baud(9_600));
        gps.set_suspended(true);
        assert!(!gps.suspended());
        gps.stop();
    }

    #[test]
    fn gps_logs_suspension_once_when_set_repeatedly() {
        let logs = Logs::default();
        tracing::subscriber::with_default(logs.clone(), || {
            let gps = Gps::detached();
            for _ in 0..3 {
                gps.set_suspended(true);
            }
            assert!(gps.suspended());
            for _ in 0..3 {
                gps.set_suspended(false);
            }
        });
        logs.assert_messages(&["stands down", "resumes"]);
    }

    /// `line` as the bridge forwards it, terminated.
    fn streamed(line: &[u8]) -> Vec<u8> {
        let mut bytes = line.to_vec();
        bytes.extend_from_slice(b"\r\n");
        bytes
    }

    #[test]
    fn gps_has_no_fresh_fix_when_receiver_sends_only_no_fix_sentences() {
        // A receiver with no sky still sends every epoch. That is not a position.
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.feed_bytes(&streamed(GGA_NO_FIX), 1_000);
        gps.feed_bytes(b"$GPGSV,1,1,00*79\r\n", 1_000);
        assert!(!gps.has_fresh_fix(1_000));
    }

    #[test]
    fn gps_has_fresh_fix_when_fix_is_within_max_age() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.feed_bytes(&streamed(GGA), 1_000);
        assert!(gps.has_fresh_fix(6_000));
        assert!(!gps.has_fresh_fix(6_001));
        // Sentences without a fix do not refresh it.
        gps.feed_bytes(&streamed(GGA_NO_FIX), 6_001);
        assert!(!gps.has_fresh_fix(6_001));
    }

    #[test]
    fn gps_has_no_fresh_fix_when_stream_resets_until_a_new_fix_arrives() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        gps.feed_bytes(&streamed(GGA), 1_000);
        gps.reset_stream();
        // The retained fix is within the age limit, but it is from before the reconnect.
        assert!(!gps.has_fresh_fix(2_000));
        assert!(gps.latest().is_some(), "the chain still ages the retained fix");
        gps.feed_bytes(&streamed(GGA), 3_000);
        assert!(gps.has_fresh_fix(3_000));
    }

    #[test]
    fn gps_reports_pinned_when_operator_names_the_port() {
        let gps = Gps::spawn(GpsConfig::pinned("/dev/wartui-test-no-such-port").at_baud(9_600));
        assert!(gps.is_pinned());
        gps.stop();
        assert!(!Gps::detached().is_pinned());
        assert!(!Gps::bridged(DEFAULT_MAX_AGE).is_pinned());
    }

    #[test]
    fn gps_drops_partial_sentence_when_stream_is_reset() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        // Half a sentence, then a reconnect, then the tail of another: joined, the two would parse
        // as nothing and count as rejected, or worse, as a sentence neither sent.
        gps.feed_bytes(&GGA[..30], 1_000);
        gps.reset_stream();
        gps.feed_bytes(&GGA[30..], 2_000);
        gps.feed_bytes(b"\r\n", 2_000);
        assert_eq!(gps.view().counters.fixes, 0);
        let mut sentence = GGA.to_vec();
        sentence.extend_from_slice(b"\r\n");
        gps.feed_bytes(&sentence, 3_000);
        assert_eq!(gps.view().counters.fixes, 1);
    }

    #[test]
    fn gps_keeps_fix_and_sky_when_stream_is_reset() {
        let gps = Gps::bridged(DEFAULT_MAX_AGE);
        let mut stream = GGA.to_vec();
        stream.extend_from_slice(b"\r\n$GPGSV,1,1,02,10,63,137,17,08,49,090,26,1*69\r\n");
        gps.feed_bytes(&stream, 1_000);
        let (latest, sky) = (gps.latest(), gps.view().sky);
        gps.reset_stream();
        assert_eq!(gps.latest(), latest);
        assert_eq!(gps.view().sky, sky);
    }

    #[test]
    fn lines_reassembles_sentence_when_stream_arrives_in_fragments() {
        // A USB serial read returns whatever happened to be in the buffer, so
        // this is the normal case rather than an edge one.
        let gps = Gps::detached();
        let mut lines = Lines::default();
        let stream = [&GGA[..20], &GGA[20..], b"\r\n$GPGGA"];
        for chunk in stream {
            lines.push(chunk, |line| gps.feed(line, 3_000));
        }
        assert_eq!(gps.view().counters.fixes, 1, "one whole sentence, once");
    }

    #[test]
    fn lines_caps_buffer_when_line_exceeds_max_line() {
        let gps = Gps::detached();
        let mut lines = Lines::default();
        lines.push(&vec![b'x'; MAX_LINE * 4], |line| gps.feed(line, 1_000));
        assert!(lines.buf.len() <= MAX_LINE);
        // And the sentence after the flood still parses.
        lines.push(b"\n", |line| gps.feed(line, 1_000));
        lines.push(GGA, |line| gps.feed(line, 2_000));
        lines.push(b"\n", |line| gps.feed(line, 2_000));
        assert_eq!(gps.view().counters.fixes, 1);
    }
}
