//! A [`Ports`] over boards a test describes, so the driver's sweep, probe, cadence and tally run
//! against something that answers as a bridge, a node or nothing does.
//!
//! Reads return what is queued, or wait a millisecond and fail with `TimedOut`, as the `pipe`
//! contract asks. Every board decodes the host frames written to it, so a test reads back what
//! the driver said.

use std::io::{self, ErrorKind, Read, Write};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use wartui_proto::bus::{NodeBoard, Version};
use wartui_proto::link::{
    BRIDGE_VERSION, BridgeToHost, FrameAccumulator, HostToBridge, LoopPhase, MAX_FRAME, Ready,
    ReadyCore, ReadyRest, ShortStr, Trial, decode_host_frame, encode_frame,
};
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;

use crate::TransportError;
use crate::pipe::{Abort, Ports, SerialParts, SerialPipe};
use crate::ports::PortCandidate;

/// How long a read with nothing to return waits before it fails with `TimedOut`.
const IDLE_READ: Duration = Duration::from_millis(1);

/// Undecodable chunks a talking board sends per read, enough to pass the garble limit within a
/// tick or two of a test's cadence.
const GARBAGE_CHUNKS: usize = 8;

/// What a fake board does with what it is sent.
#[derive(Debug, Clone)]
pub(super) enum Behavior {
    /// A bridge at `mac`, answering each `Identify` with a `Ready` after the first `ignore`.
    Bridge { mac: Mac, ignore: u32 },
    /// A bridge at `mac` one major past this host's, answering each `Identify` with a `Ready`
    /// whose rest the host does not read.
    OtherMajor { mac: Mac },
    /// Reads nothing back, ever.
    Silent,
    /// Talks constantly, and none of it decodes.
    Garbage,
    /// Will not open, for this reason.
    Refused(ErrorKind),
}

/// One board, as the fake lists it and as it has been spoken to.
#[derive(Debug)]
struct Board {
    candidate: PortCandidate,
    behavior: Behavior,
    /// Still attached: listed, and readable through any pipe open on it.
    plugged: bool,
    /// What the host has said to it, across every open.
    decoded: Vec<HostToBridge>,
    /// `Identify`s it has heard, against `Behavior::Bridge`'s `ignore`.
    identifies: u32,
    /// When it booted, which its `Ready` uptime counts from.
    booted: Instant,
}

/// One open of a board.
#[derive(Debug)]
struct Line {
    board: usize,
    /// Bytes for the host to read.
    inbound: Vec<u8>,
    /// The host's bytes, framed.
    acc: FrameAccumulator<MAX_FRAME>,
}

#[derive(Debug, Default)]
struct Shared {
    boards: Vec<Board>,
    lines: Vec<Line>,
    /// Each path opened, in order.
    opened: Vec<String>,
    list_fails: bool,
}

/// Fake boards, shared by every clone, so a test keeps one to watch and steer.
#[derive(Debug, Clone, Default)]
pub(super) struct FakePorts {
    shared: Arc<Mutex<Shared>>,
}

impl FakePorts {
    /// Attach a board.
    pub(super) fn attach(&self, candidate: PortCandidate, behavior: Behavior) -> &Self {
        self.lock().boards.push(Board {
            candidate,
            behavior,
            plugged: true,
            decoded: Vec::new(),
            identifies: 0,
            booted: Instant::now(),
        });
        self
    }

    /// Every path opened so far, in order.
    pub(super) fn opened(&self) -> Vec<String> {
        self.lock().opened.clone()
    }

    /// What the board at `path` has decoded from the host.
    pub(super) fn decoded(&self, path: &str) -> Vec<HostToBridge> {
        self.lock()
            .boards
            .iter()
            .find(|board| board.candidate.path == path)
            .map(|board| board.decoded.clone())
            .unwrap_or_default()
    }

    /// Pull the board at `path`: it leaves the list, and every read and write on it fails.
    pub(super) fn unplug(&self, path: &str) {
        for board in &mut self.lock().boards {
            if board.candidate.path == path {
                board.plugged = false;
            }
        }
    }

    /// Make every later `list` fail.
    pub(super) fn fail_list(&self) {
        self.lock().list_fails = true;
    }

    /// As a trait object, the way the transports take it.
    pub(super) fn ports(&self) -> Arc<dyn Ports> {
        Arc::new(self.clone())
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Ports for FakePorts {
    fn list(&self) -> Result<Vec<PortCandidate>, TransportError> {
        let shared = self.lock();
        if shared.list_fails {
            return Err(TransportError::Enumerate(io::Error::other("the fake will not list")));
        }
        Ok(shared
            .boards
            .iter()
            .filter(|board| board.plugged)
            .map(|board| board.candidate.clone())
            .collect())
    }

    fn open(&self, candidate: &PortCandidate) -> Result<Box<dyn SerialPipe>, TransportError> {
        let mut shared = self.lock();
        let port = candidate.path.clone();
        let Some(board) = shared
            .boards
            .iter()
            .position(|board| board.plugged && board.candidate.path == candidate.path)
        else {
            return Err(TransportError::Open { port, source: ErrorKind::NotFound.into() });
        };
        // As the desktop adapter words a refusal.
        match shared.boards[board].behavior {
            Behavior::Refused(ErrorKind::PermissionDenied) => {
                return Err(TransportError::Forbidden { port });
            }
            Behavior::Refused(kind) => {
                return Err(TransportError::Open { port, source: kind.into() });
            }
            _ => {}
        }
        shared.opened.push(port);
        shared.lines.push(Line { board, inbound: Vec::new(), acc: FrameAccumulator::new() });
        let line = shared.lines.len() - 1;
        Ok(Box::new(Pipe { shared: Arc::clone(&self.shared), line }))
    }
}

/// A handle on one [`Line`]. Every part [`SerialPipe::split`] makes is another.
#[derive(Debug, Clone)]
struct Pipe {
    shared: Arc<Mutex<Shared>>,
    line: usize,
}

impl Pipe {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn unplugged() -> io::Error {
    io::Error::new(ErrorKind::BrokenPipe, "the board was unplugged")
}

impl Read for Pipe {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (n, garbage) = {
            let mut shared = self.lock();
            let Shared { boards, lines, .. } = &mut *shared;
            let line = &mut lines[self.line];
            let board = &boards[line.board];
            if !board.plugged {
                return Err(unplugged());
            }
            let garbage = matches!(board.behavior, Behavior::Garbage);
            if garbage && line.inbound.is_empty() {
                // A bootloader's log at the wrong rate: no chunk of it is a frame.
                line.inbound.extend(b"ESP-ROM:esp32p4\0".repeat(GARBAGE_CHUNKS));
            }
            let n = buf.len().min(line.inbound.len());
            buf[..n].copy_from_slice(&line.inbound[..n]);
            line.inbound.drain(..n);
            (n, garbage)
        };
        // Paced like a port, so a board that talks constantly does not spin its reader.
        if n == 0 || garbage {
            std::thread::sleep(IDLE_READ);
        }
        if n == 0 { Err(ErrorKind::TimedOut.into()) } else { Ok(n) }
    }
}

impl Write for Pipe {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut shared = self.lock();
        let Shared { boards, lines, .. } = &mut *shared;
        let line = &mut lines[self.line];
        let board = &mut boards[line.board];
        if !board.plugged {
            return Err(unplugged());
        }
        for &byte in buf {
            let Some(cmd) =
                line.acc.push(byte).and_then(|frame| decode_host_frame(frame).ok().flatten())
            else {
                continue;
            };
            match (&cmd, &board.behavior) {
                (HostToBridge::Identify, Behavior::Bridge { mac, ignore }) => {
                    board.identifies += 1;
                    if board.identifies > *ignore {
                        line.inbound.extend(ready(*mac, BRIDGE_VERSION, board.booted));
                    }
                }
                (HostToBridge::Identify, Behavior::OtherMajor { mac }) => {
                    let version = Version::new(BRIDGE_VERSION.major + 1, 0, 0);
                    line.inbound.extend(ready(*mac, version, board.booted));
                }
                _ => {}
            }
            board.decoded.push(cmd);
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SerialPipe for Pipe {
    fn split(self: Box<Self>) -> io::Result<SerialParts> {
        Ok(SerialParts {
            reader: Box::new((*self).clone()),
            writer: Box::new((*self).clone()),
            abort: Box::new(*self),
        })
    }
}

impl Abort for Pipe {
    /// A fake write never blocks, so there is nothing to release.
    fn clear_output(&mut self) {}
}

/// A `Ready` from a bridge on `version` at `mac` that booted at `booted`. Its uptime advances with
/// real time, so the answers to two `Identify`s read as one life.
fn ready(mac: Mac, version: Version, booted: Instant) -> Vec<u8> {
    let msg = BridgeToHost::Ready(Ready {
        core: ReadyCore {
            mac,
            image: [0; 32],
            version,
            trial: Trial::Settled,
            uptime_ms: u32::try_from(booted.elapsed().as_millis()).unwrap_or(u32::MAX),
        },
        rest: Some(ReadyRest {
            fw_version: ShortStr::new(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            node_board: NodeBoard::NoCarrier,
            radio: None,
        }),
    });
    let mut buf = [0; MAX_FRAME];
    let len = encode_frame(&msg, &mut buf).expect("a Ready fits a frame");
    buf[..len].to_vec()
}
