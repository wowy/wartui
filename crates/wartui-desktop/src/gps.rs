//! The puck reader: a thread that finds an NMEA receiver among the host's serial ports, reads it,
//! and feeds what it says into a [`Gps`], whose state and parser are `wartui-core`'s.
//!
//! The reader runs on its own thread and publishes into the [`Gps`] it was given, so nothing
//! above waits on it ([`wartui_core::gps`] says why that matters).
//!
//! **It stands down while the bridge's receiver is sending sentences** ([`Gps::set_suspended`],
//! which [`wartui_core::PositionChain::stand_down`] calls): it lets go of its port within
//! `READ_TIMEOUT` and opens nothing, so one capture reads one receiver. A receiver that stops
//! sending lets the search resume after the staleness limit. [`wartui_core::gps`] has the one port
//! that never stands down.
//!
//! **The search, the backoff and the retries are the reader's** (`read_forever`). Each rule is
//! stated where it is applied: a port must stay readable for `HEALTHY` before the backoff is
//! forgiven, a port that has worked is retried `RETRIES_BEFORE_SEARCHING` times before the
//! search runs again, and a named port is never searched past. Which ports are listened to, and
//! what counts as a receiver, is [`discover`]'s.
//!
//! **What the ports owe the reader** (`ReceiverPorts`, implemented by
//! [`SerialPorts`] and by a fake in the tests):
//!
//! - `open` opens at the baud asked for with no flow control, and **never sets DTR or RTS**:
//!   moving one modem line without the other resets a board ([`wartui_bridge::ports`] has why).
//!   Nothing is ever written to the port.
//! - A read returns within about `READ_TIMEOUT`, failing with [`ErrorKind::TimedOut`] when
//!   nothing arrived. The reader polls its stop and stand-down flags between reads, so a read that
//!   blocks longer delays both by as much.
//! - A read returning `Ok(0)` means the device closed the port.
//! - A port that is gone fails its reads with an error.

use std::io::{ErrorKind, Read};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wartui_bridge::ports::PortCandidate;
use wartui_core::gps::{Gps, GpsStatus, Lines};
use wartui_core::position::DEFAULT_MAX_AGE;

use crate::SerialPorts;
use crate::discover::{self, BAUD_LADDER, PROBE_WINDOW};

/// How long a read blocks before the reader looks at its stop and stand-down flags again.
pub(crate) const READ_TIMEOUT: Duration = Duration::from_millis(200);

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

/// The host's serial ports, as the receiver search listens to them.
pub(crate) trait ReceiverPorts: Send + Sync {
    /// Every USB serial port attached, named by the most stable path the OS offers.
    fn list(&self) -> std::io::Result<Vec<PortCandidate>>;

    /// Open `path` at `baud` to listen. Nothing is ever written to it.
    fn open(&self, path: &str, baud: u32) -> std::io::Result<Box<dyn Read + Send>>;
}

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
        Self { port: None, baud: None, reserved: Vec::new(), max_age: DEFAULT_MAX_AGE }
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

/// Open `config.port` on a background thread and keep it open, reconnecting if the receiver is
/// unplugged and put back. Returns at once: observations with no position are still worth having.
#[must_use]
pub fn spawn(config: GpsConfig) -> Gps {
    spawn_on(config, Arc::new(SerialPorts))
}

/// [`spawn`], listing and opening ports through `ports`.
fn spawn_on(config: GpsConfig, ports: Arc<dyn ReceiverPorts>) -> Gps {
    let gps = Gps::for_reader(config.port.is_some(), config.baud.is_some(), config.max_age);
    let worker = gps.clone();
    let started = std::thread::Builder::new()
        .name("wartui-gps".to_owned())
        .spawn(move || Reader { gps: &worker, ports: ports.as_ref() }.read_forever(&config));
    if let Err(e) = started {
        gps.set_status(GpsStatus::Failed(format!("starting the reader thread: {e}")));
    }
    gps
}

/// The reader thread's view: the receiver it feeds, and the ports it reads.
struct Reader<'a> {
    gps: &'a Gps,
    ports: &'a dyn ReceiverPorts,
}

impl Reader<'_> {
    /// Whether the reader should let go of what it is doing: stopping, or standing down.
    fn halted(&self) -> bool {
        self.gps.is_stopped() || self.gps.is_suspended()
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
        self.gps.hold_no_port();
        while self.gps.is_suspended() && !self.gps.is_stopped() {
            std::thread::sleep(READ_TIMEOUT);
        }
    }

    /// Find a receiver, read it, and look again when it goes away.
    ///
    /// The search runs here because nothing blocks on it: each row takes whatever position exists
    /// as it is written, and a run waiting for a fix checks [`Gps::fix_run`] on its tick, so
    /// seconds spent listening stall nothing. It also lets a receiver survive moving to another
    /// socket, which a reader given one literal path cannot.
    fn read_forever(&self, config: &GpsConfig) {
        let mut backoff = MIN_BACKOFF;
        // What the search settled on, kept across reconnections: a puck put back is the same port
        // at the same rate.
        let mut settled: Option<(String, u32)> = None;
        // Why reading stopped. A working receiver that stopped is news, either way. One that was
        // never there is not.
        let mut lost: Option<String> = None;
        let mut retries = 0_u32;

        while !self.gps.is_stopped() {
            if self.gps.is_suspended() {
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
                let why = |path: &str| why_not(self.ports, path);
                self.gps.set_status(nothing_found(config.port.as_deref(), lost.as_deref(), why));
                self.pause(backoff);
                backoff = (backoff * 2).min(MAX_BACKOFF);
                continue;
            };
            if settled.is_none() {
                settled = Some((port.clone(), baud));
                lost = None;
                retries = 0;
                self.gps.settle(&port, baud);
                tracing::info!(port = %port, baud, "reading a receiver");
            }

            match self.ports.open(&port, baud) {
                Ok(handle) => {
                    if !matches!(self.gps.view().status, GpsStatus::Fixed { .. }) {
                        self.gps.set_status(GpsStatus::Searching);
                    }
                    let opened = Instant::now();
                    let Some(reason) = self.read_port(handle) else {
                        // Halted, not failed: nothing is retried or backed off. A stand-down is
                        // handled at the top of the loop, and one already lifted reopens the
                        // settled port.
                        if self.gps.is_stopped() {
                            return;
                        }
                        continue;
                    };
                    if self.gps.is_stopped() {
                        return;
                    }
                    if self.gps.is_suspended() {
                        continue;
                    }
                    if opened.elapsed() >= HEALTHY {
                        backoff = MIN_BACKOFF;
                        retries = 0;
                    }
                    self.gps.set_status(GpsStatus::Failed(reason));
                }
                Err(_) if self.halted() => continue,
                Err(e) => self.gps.set_status(GpsStatus::Failed(format!("{port}: {e}"))),
            }

            // Look past a port that keeps failing or has gone, unless the operator named it:
            // searching would answer a question they did not ask.
            retries += 1;
            let gone = !still_there(self.ports, &port);
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
                self.gps.unsettle();
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
        let (candidates, needed) = match &config.port {
            // A named path says nothing about rate, so the ladder still runs. One sentence settles
            // each rate, since the operator has said what the device is.
            Some(path) => (
                vec![wartui_bridge::ports::candidate(path, None, None, None)],
                discover::SENTENCES_ON_A_NAMED_PORT,
            ),
            None => match self.ports.list() {
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
        discover::settle(&candidates, &ladder, needed, |path, baud| {
            if announce {
                self.gps.set_status(GpsStatus::Scanning { port: path.to_owned(), baud });
            }
            self.listen(path, baud)
        })
        .map(|(path, baud)| (path.to_owned(), baud))
    }

    /// Read whatever a port has to say for [`PROBE_WINDOW`], and say nothing back.
    fn listen(&self, path: &str, baud: u32) -> Vec<u8> {
        // Standing down opens nothing, so the rest of the search falls through at once.
        if self.halted() {
            return Vec::new();
        }
        let Ok(mut port) = self.ports.open(path, baud) else { return Vec::new() };
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

    /// Read until the port fails, returning why in terms fit to show a user, or `None` when the
    /// reader was halted (stopped or standing down) rather than the port failing.
    fn read_port(&self, mut port: Box<dyn Read + Send>) -> Option<String> {
        let mut lines = Lines::default();
        let mut buf = [0u8; 512];
        while !self.halted() {
            match port.read(&mut buf) {
                Ok(0) => return Some("the receiver closed the port".to_owned()),
                Ok(n) => {
                    let now_ms = chrono::Utc::now().timestamp_millis();
                    lines.push(&buf[..n], |line| self.gps.feed(line, now_ms));
                }
                // Silence does not break the port, but it still ages the retained fix.
                Err(e) if e.kind() == ErrorKind::TimedOut => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Some(e.to_string()),
            }
            self.gps.tick(chrono::Utc::now().timestamp_millis());
        }
        None
    }
}

/// What to say when a search came back with nothing.
///
/// This is what makes searching by default bearable. A receiver nobody asked for or attached is not
/// a fault, so the view names it in a word rather than a fault line, and the search runs again in
/// case one is plugged in mid-capture.
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
fn why_not(ports: &dyn ReceiverPorts, path: &str) -> String {
    match ports.open(path, BAUD_LADDER[0]) {
        Err(e) => format!("{path}: {e}"),
        // It opened, so it is there and it is not a receiver — or not one talking
        // NMEA, which a u-blox configured to emit only UBX binary is not.
        Ok(_) => format!("{path} answered no NMEA at any rate tried"),
    }
}

/// Whether the search's path is still among the attached ports. A receiver moved to another socket
/// returns under another name, and waiting for the old one is what a reader given one literal path
/// does. A failed enumeration counts as present: failing to list is no evidence a working port has
/// gone.
fn still_there(ports: &dyn ReceiverPorts, path: &str) -> bool {
    ports.list().map_or(true, |attached| {
        attached.iter().any(|candidate| candidate.path == path || candidate.device == path)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::sync::Mutex;

    use super::*;

    const GGA: &[u8] = b"$GPGGA,123519.00,4807.038,N,01131.000,E,1,08,0.9,545.4,M,46.9,M,,*69";

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

    /// A path the fake has no script for, which it refuses to open as the OS would.
    const NO_SUCH_PORT: &str = "/dev/wartui-test-no-such-port";

    /// What a scripted port does once opened.
    #[derive(Debug, Clone)]
    enum Script {
        /// Refuse to open, for this reason.
        Refuse(ErrorKind),
        /// Yield these bytes, then report the port closed (`Ok(0)`).
        Ends(Vec<u8>),
        /// Yield these bytes, then time out on every read.
        Quiet(Vec<u8>),
    }

    /// What the fake lists, what each path does, and what the reader opened.
    #[derive(Debug, Default)]
    struct Bench {
        listed: Vec<PortCandidate>,
        scripts: HashMap<String, Script>,
        opened: Vec<(String, u32)>,
        unplugged: HashSet<String>,
    }

    /// The host's ports as a test scripts them. Clones share one [`Bench`].
    #[derive(Debug, Clone, Default)]
    struct FakePorts(Arc<Mutex<Bench>>);

    impl FakePorts {
        /// List `candidate`, and script its path.
        fn attach(self, candidate: PortCandidate, script: Script) -> Self {
            self.script(&candidate.path.clone(), script);
            self.bench().listed.push(candidate);
            self
        }

        /// Script `path` without listing it, as a port only `--gps` names.
        fn script(&self, path: &str, script: Script) {
            self.bench().scripts.insert(path.to_owned(), script);
        }

        /// Take `path` out of the listing and fail its reads.
        fn unplug(&self, path: &str) {
            let mut bench = self.bench();
            bench.listed.retain(|candidate| candidate.path != path);
            bench.unplugged.insert(path.to_owned());
        }

        /// Every `(path, baud)` opened so far, in order.
        fn opened(&self) -> Vec<(String, u32)> {
            self.bench().opened.clone()
        }

        fn shared(&self) -> Arc<dyn ReceiverPorts> {
            Arc::new(self.clone())
        }

        fn bench(&self) -> std::sync::MutexGuard<'_, Bench> {
            self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }

    impl ReceiverPorts for FakePorts {
        fn list(&self) -> std::io::Result<Vec<PortCandidate>> {
            Ok(self.bench().listed.clone())
        }

        fn open(&self, path: &str, baud: u32) -> std::io::Result<Box<dyn Read + Send>> {
            let mut bench = self.bench();
            bench.opened.push((path.to_owned(), baud));
            if bench.unplugged.contains(path) {
                return Err(ErrorKind::NotFound.into());
            }
            let (bytes, ends) = match bench.scripts.get(path) {
                None => return Err(ErrorKind::NotFound.into()),
                Some(Script::Refuse(kind)) => return Err((*kind).into()),
                Some(Script::Ends(bytes)) => (bytes.clone(), true),
                Some(Script::Quiet(bytes)) => (bytes.clone(), false),
            };
            let ports = self.clone();
            Ok(Box::new(FakePort { ports, path: path.to_owned(), bytes, at: 0, ends }))
        }
    }

    /// One open scripted port.
    struct FakePort {
        ports: FakePorts,
        path: String,
        bytes: Vec<u8>,
        at: usize,
        ends: bool,
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.ports.bench().unplugged.contains(&self.path) {
                return Err(std::io::Error::other("the device is gone"));
            }
            let left = &self.bytes[self.at..];
            if !left.is_empty() {
                let n = left.len().min(buf.len());
                buf[..n].copy_from_slice(&left[..n]);
                self.at += n;
                return Ok(n);
            }
            if self.ends {
                return Ok(0);
            }
            std::thread::sleep(Duration::from_millis(1));
            Err(ErrorKind::TimedOut.into())
        }
    }

    /// Two whole sentences, enough for the search to believe an unknown port.
    fn talk() -> Vec<u8> {
        [GGA, b"\r\n", GGA, b"\r\n"].concat()
    }

    /// A receiver on `path` that says nothing about itself.
    fn puck(path: &str) -> PortCandidate {
        wartui_bridge::ports::candidate(path, Some(0x1546), Some(0x01A7), None)
    }

    #[test]
    fn gps_reader_reads_fix_when_named_port_and_rate_talk_nmea() {
        let ports = FakePorts::default();
        ports.script("/dev/ttyACM0", Script::Quiet(talk()));
        let gps = spawn_on(GpsConfig::pinned("/dev/ttyACM0").at_baud(4_800), ports.shared());
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Fixed { .. })));
        assert!(gps.latest().is_some());
        // Port and rate both named leave nothing to probe: the port is opened once, at that rate.
        assert_eq!(ports.opened(), [("/dev/ttyACM0".to_owned(), 4_800)]);
        gps.stop();
    }

    #[test]
    fn gps_reader_settles_on_talking_port_when_search_meets_a_silent_one() {
        // The silent port sorts first and closes at once at every rate, so the search moves on.
        let ports = FakePorts::default()
            .attach(puck("/dev/ttyACM0"), Script::Ends(Vec::new()))
            .attach(puck("/dev/ttyACM1"), Script::Quiet(talk()));
        let gps = spawn_on(GpsConfig::search(), ports.shared());
        let want = Some(("/dev/ttyACM1".to_owned(), BAUD_LADDER[0]));
        assert!(eventually(|| gps.view().settled == want), "{:?}", gps.view());
        let opened = ports.opened();
        for baud in BAUD_LADDER {
            assert!(opened.contains(&("/dev/ttyACM0".to_owned(), baud)), "{opened:?}");
        }
        gps.stop();
    }

    #[test]
    fn gps_reader_opens_no_fleet_board_when_searching() {
        // Both boards would pass for receivers if read, so only the listing keeps them shut.
        use wartui_bridge::ports::{
            CH343_PID, CH343_VID, ESPRESSIF_VID, SERIAL_JTAG_PID, candidate,
        };
        let bridge = candidate("/dev/ttyACM0", Some(CH343_VID), Some(CH343_PID), None);
        let node = candidate("/dev/ttyACM1", Some(ESPRESSIF_VID), Some(SERIAL_JTAG_PID), None);
        let ports = FakePorts::default()
            .attach(bridge, Script::Ends(talk()))
            .attach(node, Script::Ends(talk()))
            .attach(puck("/dev/ttyACM2"), Script::Quiet(talk()));
        let gps = spawn_on(GpsConfig::search(), ports.shared());
        let want = Some(("/dev/ttyACM2".to_owned(), BAUD_LADDER[0]));
        assert!(eventually(|| gps.view().settled == want), "{:?}", gps.view());
        let opened = ports.opened();
        assert!(opened.iter().all(|(path, _)| path == "/dev/ttyACM2"), "{opened:?}");
        gps.stop();
    }

    #[test]
    fn gps_reader_opens_no_reserved_port_when_searching() {
        let ports = FakePorts::default()
            .attach(puck("/dev/ttyACM0"), Script::Ends(talk()))
            .attach(puck("/dev/ttyACM1"), Script::Quiet(talk()));
        let config = GpsConfig::search().reserving(vec!["/dev/ttyACM0".to_owned()]);
        let gps = spawn_on(config, ports.shared());
        let want = Some(("/dev/ttyACM1".to_owned(), BAUD_LADDER[0]));
        assert!(eventually(|| gps.view().settled == want), "{:?}", gps.view());
        let opened = ports.opened();
        assert!(opened.iter().all(|(path, _)| path == "/dev/ttyACM1"), "{opened:?}");
        gps.stop();
    }

    #[test]
    fn gps_reader_reports_no_receiver_when_nothing_is_attached() {
        let ports = FakePorts::default();
        let gps = spawn_on(GpsConfig::search(), ports.shared());
        assert!(eventually(|| gps.view().status == GpsStatus::NoReceiver), "{:?}", gps.view());
        assert_eq!(ports.opened(), []);
        gps.stop();
    }

    #[test]
    fn gps_reader_fails_with_os_reason_when_named_port_will_not_open() {
        let ports = FakePorts::default();
        ports.script("/dev/ttyACM0", Script::Refuse(ErrorKind::PermissionDenied));
        let gps = spawn_on(GpsConfig::pinned("/dev/ttyACM0"), ports.shared());
        let reason = std::io::Error::from(ErrorKind::PermissionDenied);
        let want = GpsStatus::Failed(format!("/dev/ttyACM0: {reason}"));
        assert!(eventually(|| gps.view().status == want), "{:?}", gps.view());
        gps.stop();
    }

    #[test]
    fn gps_reader_says_port_is_no_longer_attached_when_it_leaves_the_listing() {
        let ports = FakePorts::default().attach(puck("/dev/ttyACM0"), Script::Quiet(talk()));
        let gps = spawn_on(GpsConfig::search(), ports.shared());
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Fixed { .. })));
        ports.unplug("/dev/ttyACM0");
        let want = GpsStatus::Failed("/dev/ttyACM0 is no longer attached".to_owned());
        assert!(eventually(|| gps.view().status == want), "{:?}", gps.view());
        gps.stop();
    }

    #[test]
    fn gps_reader_stands_down_without_a_fault_when_suspended() {
        // A receiver read and then lost is a fault. Standing down clears it, holds no port, and
        // resumes as a fresh search, which has nothing to report as lost.
        let ports = FakePorts::default().attach(puck("/dev/ttyACM0"), Script::Quiet(talk()));
        let gps = spawn_on(GpsConfig::search(), ports.shared());
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Fixed { .. })));
        ports.unplug("/dev/ttyACM0");
        assert!(eventually(|| matches!(gps.view().status, GpsStatus::Failed(_))));
        gps.set_suspended(true);
        assert!(eventually(|| gps.view().status == GpsStatus::Connecting));
        // Held there, not retried into another fault.
        std::thread::sleep(MIN_BACKOFF * 2);
        assert_eq!(gps.view().status, GpsStatus::Connecting);
        assert_eq!(gps.view().settled, None);
        gps.set_suspended(false);
        assert!(eventually(|| gps.view().status == GpsStatus::NoReceiver), "{:?}", gps.view());
        gps.stop();
    }

    #[test]
    fn gps_reader_ignores_suspension_when_port_is_pinned() {
        let gps =
            spawn_on(GpsConfig::pinned(NO_SUCH_PORT).at_baud(9_600), FakePorts::default().shared());
        gps.set_suspended(true);
        assert!(!gps.is_suspended());
        gps.stop();
    }

    #[test]
    fn gps_reader_reports_pinned_when_operator_names_the_port() {
        let gps =
            spawn_on(GpsConfig::pinned(NO_SUCH_PORT).at_baud(9_600), FakePorts::default().shared());
        assert!(gps.is_pinned());
        gps.stop();
        assert!(!Gps::detached().is_pinned());
        assert!(!Gps::bridged(DEFAULT_MAX_AGE).is_pinned());
    }

    #[test]
    fn gps_reader_reports_no_failure_when_read_ends_on_a_halt() {
        // A port that keeps talking ends its read only on a halt, which is not the port failing.
        let ports = FakePorts::default();
        ports.script("/dev/ttyACM0", Script::Quiet(talk()));
        let suspended = Gps::for_reader(false, false, DEFAULT_MAX_AGE);
        suspended.set_suspended(true);
        let stopped = Gps::for_reader(false, false, DEFAULT_MAX_AGE);
        stopped.stop();
        for gps in [&suspended, &stopped] {
            let reader = Reader { gps, ports: &ports };
            let port = ports.open("/dev/ttyACM0", BAUD_LADDER[0]).expect("a scripted port");
            assert_eq!(reader.read_port(port), None);
        }
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
}
