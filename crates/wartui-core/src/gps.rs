//! A GPS on the host, read as NMEA over a serial port.
//!
//! The receiver runs on its own thread and publishes the last fix it saw into a
//! shared cell. Nothing above it blocks on a satellite: [`Gps::latest`] is a
//! mutex and a copy, so [`crate::PositionChain::resolve`] stays the cheap
//! synchronous call the engine needs it to be.
//!
//! **The reader stamps arrival time itself**, being the only thing that knows when a
//! byte turned up. Whether that age is too much is [`crate::PositionChain`]'s.

use std::io::ErrorKind;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::discover::{self, BAUD_LADDER, PROBE_WINDOW};
use crate::nmea::{Nmea, NmeaError, Report};
use crate::position::{Fix, PositionSource};

/// Longest line worth assembling. NMEA 0183 caps a sentence at 82 characters;
/// this leaves room for the proprietary sentences that ignore that, and is
/// still short enough that a receiver spewing binary cannot grow the buffer.
const MAX_LINE: usize = 256;

/// How long to block in a read before looking at the stop flag again.
const READ_TIMEOUT: Duration = Duration::from_millis(200);

/// First retry delay after the port fails, doubling to [`MAX_BACKOFF`].
const MIN_BACKOFF: Duration = Duration::from_millis(250);

/// Slowest retry: an unplugged puck is worth checking for every few seconds.
const MAX_BACKOFF: Duration = Duration::from_secs(5);

/// How long a port has to stay readable before the backoff is forgiven.
///
/// Opening is not the same as working: a device node left behind after an unplug
/// opens and then fails on the first read, and resetting on the open alone would
/// retry that four times a second for the rest of the capture.
const HEALTHY: Duration = Duration::from_secs(2);

/// How many times a port that has worked is retried before the search reopens.
///
/// A receiver put back into the same socket keeps its `by-id` name, so the common
/// unplug costs a reconnection rather than another walk of the ladder. The search
/// resumes when the path is gone from the enumeration, or when these are spent.
const RETRIES_BEFORE_SEARCHING: u32 = 3;

/// Which port to read, and how fast — or neither, and go and find out.
#[derive(Debug, Clone, Default)]
pub struct GpsConfig {
    /// A device path the operator named. `None` searches for one.
    pub port: Option<String>,
    /// A line rate the operator named. `None` walks [`BAUD_LADDER`].
    pub baud: Option<u32>,
    /// Ports something else has claimed, which the search must not open.
    pub reserved: Vec<String>,
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
    /// Every port was listened to and none of them was a receiver.
    ///
    /// Distinct from [`GpsStatus::Failed`], and the distinction is the whole of
    /// what makes searching by default bearable: a receiver nobody asked for and
    /// nobody attached is not a fault, and must not put a line on a view that has
    /// one line to say anything on.
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
    /// Whether the rate was the operator's choice rather than the ladder's.
    ///
    /// The view needs it to know whether `--gps-baud` is worth mentioning: a rate
    /// the ladder chose is a rate that already produced valid sentences, so
    /// unreadable lines afterwards mean something else entirely.
    pub pinned_baud: bool,
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
            })),
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Open `config.port` on a background thread and keep it open, reconnecting
    /// on its own if the receiver is unplugged and put back.
    ///
    /// Returns immediately: observations with no position are still worth having.
    #[must_use]
    pub fn spawn(config: GpsConfig) -> Self {
        let gps = Self::detached();
        gps.lock().pinned_baud = config.baud.is_some();
        let worker = gps.clone();
        let started = std::thread::Builder::new()
            .name("wartui-gps".to_owned())
            .spawn(move || worker.read_forever(&config));
        if let Err(e) = started {
            gps.set_status(GpsStatus::Failed(format!("starting the reader thread: {e}")));
        }
        gps
    }

    /// Ask the reader thread to finish. Idempotent, and harmless on a detached
    /// receiver.
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

    /// Offer one line to the parser, as of `now_ms`.
    ///
    /// Public because it is the seam the reader thread and the tests share.
    pub fn feed(&self, line: &[u8], now_ms: i64) {
        let mut inner = self.lock();
        match inner.nmea.parse(line) {
            Ok(Report::Fix(new)) => {
                inner.counters.sentences += 1;
                inner.counters.fixes += 1;
                // A cycle is two sentences describing the same instant, and only
                // GGA carries altitude, satellite count and dilution. So the
                // position is replaced and the rest carried: what a sentence does
                // not mention, it is not contradicting.
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
                inner.counters.sentences += 1;
                // Deliberately leaves the last fix in place: a receiver that loses
                // lock under a bridge has not moved the host, and the chain's
                // staleness rule decides when to stop believing it.
                if !matches!(inner.status, GpsStatus::Fixed { .. }) {
                    inner.status = GpsStatus::Searching;
                }
            }
            Ok(Report::Other) => inner.counters.sentences += 1,
            Err(NmeaError::NotASentence | NmeaError::Checksum | NmeaError::Malformed) => {
                inner.counters.rejected += 1;
            }
        }
    }

    fn set_status(&self, status: GpsStatus) {
        // A reader thread nobody is watching is the most invisible thing in the
        // program: the rows keep being written, they just stop saying where.
        match &status {
            GpsStatus::Failed(reason) => tracing::warn!(reason = %reason, "gps unreadable"),
            other => tracing::info!(status = ?other, "gps"),
        }
        self.lock().status = status;
    }

    /// A poisoned mutex means a previous holder panicked mid-update; what it left is
    /// still readable and better than taking the capture down over a position.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Find a receiver, read it, and go back to looking when it goes away.
    ///
    /// The search lives on this thread because nothing above waits for it: a
    /// capture starts the moment it is asked to and takes whatever position is
    /// available when each row is written, so a few seconds spent listening to
    /// ports costs the capture nothing. It is also what makes a receiver survive
    /// being unplugged and put back into a different socket, which a reader given
    /// one literal path cannot do.
    fn read_forever(&self, config: &GpsConfig) {
        let mut backoff = MIN_BACKOFF;
        // What the search settled on. Kept across reconnections, because a puck put
        // back where it came from is the same port at the same rate.
        let mut settled: Option<(String, u32)> = None;
        let mut retries = 0_u32;

        while !self.stop.load(Ordering::Relaxed) {
            let Some((port, baud)) = settled.clone().or_else(|| self.search(config)) else {
                self.set_status(nothing_found(config.port.as_deref(), why_not));
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            };
            if settled.is_none() {
                settled = Some((port.clone(), baud));
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

            // A port that keeps failing, or that is no longer there at all, is worth
            // looking past. A named one never is: the operator said which, and
            // searching would answer a question they did not ask.
            retries += 1;
            if config.port.is_none() && (retries > RETRIES_BEFORE_SEARCHING || !still_there(&port))
            {
                settled = None;
                self.lock().settled = None;
            }
            std::thread::sleep(backoff);
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    /// Listen to each candidate port at each rate until one of them is a receiver.
    fn search(&self, config: &GpsConfig) -> Option<(String, u32)> {
        let ladder = config.ladder();
        let ports = match &config.port {
            // Named: the ladder still runs, because a path says nothing about a rate.
            Some(path) => vec![wartui_bridge::ports::candidate(path, None, None, None)],
            None => match wartui_bridge::ports::list() {
                Ok(attached) => discover::candidates(attached, &config.reserved),
                Err(e) => {
                    tracing::debug!(error = %e, "could not list serial ports");
                    return None;
                }
            },
        };
        discover::settle(&ports, &ladder, |path, baud| {
            self.set_status(GpsStatus::Scanning { port: path.to_owned(), baud });
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
        // Accumulated across short reads rather than taken in one long one, so that
        // `stop` is noticed within the read timeout and a capture being shut down
        // does not wait out the window.
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
                // The read timeout exists to get back here and check the stop
                // flag; a silent receiver is not a broken one.
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return e.to_string(),
            }
        }
        "stopped".to_owned()
    }
}

/// What to say when a search came back with nothing.
///
/// The whole of what makes searching by default bearable. A receiver nobody asked
/// for and nobody attached is not a fault, and a view with one line to say anything
/// on must not spend it saying that most captures have no GPS — so that case is
/// silent, and the search quietly runs again in case one is plugged in mid-capture.
///
/// A receiver the operator *named* is the opposite: they asked for that port, and
/// silence about it would leave a capture recording no position for a reason nobody
/// mentioned.
fn nothing_found(named: Option<&str>, why: impl FnOnce(&str) -> String) -> GpsStatus {
    match named {
        Some(path) => GpsStatus::Failed(why(path)),
        None => GpsStatus::NoReceiver,
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

/// Whether a path the search settled on is still among the ports attached.
///
/// A receiver unplugged and put back into another socket comes back under another
/// name, and looking for the old one for ever is what a reader given one literal
/// path does. Enumeration failing counts as "still there": a failure to list is not
/// evidence that a working port has gone.
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

    #[test]
    fn a_receiver_nobody_asked_for_and_nobody_attached_is_not_a_fault() {
        // Searching is the default, so this is most captures. A fault here would be
        // a fault on the view of every run made without a GPS.
        assert_eq!(nothing_found(None, |_| unreachable!()), GpsStatus::NoReceiver);
    }

    #[test]
    fn a_receiver_that_was_named_and_not_found_says_so() {
        let status = nothing_found(Some("/dev/ttyNOPE"), |path| format!("{path}: no such device"));
        assert_eq!(status, GpsStatus::Failed("/dev/ttyNOPE: no such device".to_owned()));
    }

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";
    const GGA_NO_FIX: &[u8] = b"$GPGGA,123520.00,4807.038,N,01131.000,E,0,00,,,M,,M,,*76";

    #[test]
    fn a_receiver_that_has_never_had_a_fix_offers_nothing() {
        let gps = Gps::detached();
        assert_eq!(gps.latest(), None);
        gps.feed(GGA_NO_FIX, 1_000);
        assert_eq!(gps.latest(), None);
        assert_eq!(gps.view().status, GpsStatus::Searching);
    }

    #[test]
    fn a_fix_is_kept_with_the_time_it_arrived_not_the_time_it_was_taken() {
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
    fn the_altitude_and_accuracy_survive_the_sentence_that_does_not_carry_them() {
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
    fn losing_lock_does_not_throw_away_the_last_known_position() {
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
    fn garbage_is_counted_rather_than_mistaken_for_a_place() {
        let gps = Gps::detached();
        gps.feed(b"\xff\xfe binary junk", 1_000);
        gps.feed(GGA, 2_000);
        let view = gps.view();
        assert_eq!(view.counters.rejected, 1);
        assert_eq!(view.counters.fixes, 1);
        assert_eq!(view.last_fix_ms, Some(2_000));
    }

    #[test]
    fn sentences_split_across_reads_are_reassembled() {
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
    fn a_receiver_talking_something_other_than_nmea_cannot_grow_the_buffer() {
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
