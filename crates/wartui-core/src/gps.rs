//! A GPS on the host, read as NMEA over a serial port.
//!
//! The receiver runs on its own thread and publishes its last fix into a shared cell. Nothing above
//! blocks on a satellite: [`Gps::latest`] is a mutex and a copy, so
//! [`crate::PositionChain::resolve`] stays cheap and synchronous for the engine.
//!
//! **The reader stamps arrival time itself**, as the only thing that knows when a byte turned up.
//! [`crate::PositionChain`] decides whether that age is too much.
//! Diagnostics use the same age limit, including on read timeouts, without changing the retained
//! fix or the screen status. Logging belongs here so resolving a position stays free of effects.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::discover::{self, BAUD_LADDER, PROBE_WINDOW};
use crate::nmea::{Nmea, NmeaError, Report};
use crate::position::{Fix, PositionSource};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    status: GpsStatus,
    counters: GpsCounters,
    settled: Option<(String, u32)>,
    pinned_baud: bool,
    diagnostic: Option<Diagnostic>,
    last_failure: Option<String>,
    max_age: Duration,
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
            && (now_ms.saturating_sub(at_ms).max(0) as u128) > self.max_age.as_millis()
        {
            tracing::warn!(last_fix_ms = at_ms, max_age_ms = ?self.max_age.as_millis(), "gps fix stale");
            self.diagnostic = Some(Diagnostic::Stale);
        }
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
}

/// A handle on the receiver, cheap to clone and safe to share.
#[derive(Debug, Clone)]
pub struct Gps {
    inner: Arc<Mutex<Inner>>,
    stop: Arc<AtomicBool>,
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
                status: GpsStatus::Connecting,
                counters: GpsCounters::default(),
                settled: None,
                pinned_baud: false,
                diagnostic: None,
                last_failure: None,
                max_age: GpsConfig::default().max_age,
            })),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open `config.port` on a background thread and keep it open, reconnecting if the receiver is
    /// unplugged and put back. Returns at once: observations with no position are still worth
    /// having.
    #[must_use]
    pub fn spawn(config: GpsConfig) -> Self {
        let gps = Self::detached();
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
        }
    }

    /// Offer one line to the parser, as of `now_ms`. Public as the seam the reader thread and the
    /// tests share.
    pub fn feed(&self, line: &[u8], now_ms: i64) {
        let mut inner = self.lock();
        inner.tick(now_ms);
        match inner.nmea.parse(line) {
            Ok(Report::Fix(new)) => {
                inner.note_fix();
                inner.counters.sentences += 1;
                inner.counters.fixes += 1;
                // A cycle is two sentences about one instant, and only GGA carries altitude,
                // satellites and dilution. So the position is replaced and the rest carried: what a
                // sentence omits, it does not contradict.
                let previous = inner.fix;
                let satellites = new.satellites.or(match inner.status {
                    GpsStatus::Fixed { satellites } => satellites,
                    _ => None,
                });
                inner.status = GpsStatus::Fixed { satellites };
                inner.fix = Some(Fix {
                    lat: Some(new.lat),
                    lon: Some(new.lon),
                    alt: new.alt.or_else(|| previous.and_then(|fix| fix.alt)),
                    accuracy: new.accuracy.or_else(|| previous.and_then(|fix| fix.accuracy)),
                    source: PositionSource::Gps,
                    at_ms: new.at_ms,
                });
                inner.received_at_ms = Some(now_ms);
            }
            Ok(Report::NoFix) => {
                inner.last_failure = None;
                inner.counters.sentences += 1;
                // Keep the last fix: losing lock under a bridge has not moved the host, and the
                // chain's staleness rule decides when to stop believing it.
                if !matches!(inner.status, GpsStatus::Fixed { .. }) {
                    inner.status = GpsStatus::Searching;
                    inner.note_status(GpsStatus::Searching);
                }
            }
            Ok(Report::Other) => inner.counters.sentences += 1,
            Err(NmeaError::NotASentence | NmeaError::Checksum | NmeaError::Malformed) => {
                inner.counters.rejected += 1;
            }
        }
    }

    fn set_status(&self, status: GpsStatus) {
        self.lock().set_status(status);
    }

    fn tick(&self, now_ms: i64) {
        self.lock().tick(now_ms);
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
            let searching = settled.is_none();
            let Some((port, baud)) =
                settled.clone().or_else(|| self.search(config, lost.is_none() && searching))
            else {
                self.set_status(nothing_found(config.port.as_deref(), lost.as_deref(), why_not));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            };
            if settled.is_none() {
                settled = Some((port.clone(), baud));
                lost = None;
                retries = 0;
                self.lock().settled = Some((port.clone(), baud));
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
                    if opened.elapsed() >= HEALTHY {
                        backoff = MIN_BACKOFF;
                        retries = 0;
                    }
                    self.set_status(GpsStatus::Failed(reason));
                }
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
                self.lock().settled = None;
            }
            std::thread::sleep(backoff);
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

        let Ok(mut port) = open_port(path, baud) else { return Vec::new() };
        let mut sample = Vec::new();
        let mut buf = [0u8; 512];
        let until = Instant::now() + PROBE_WINDOW;
        // Short reads accumulated, so `stop` is noticed within the read timeout and shutdown does
        // not wait out the window.
        while Instant::now() < until && !self.stop.load(Ordering::Relaxed) {
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
        while !self.stop.load(Ordering::Relaxed) {
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
