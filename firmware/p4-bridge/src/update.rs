//! The P4's own image: its trial, and the update `wartui flash-bridge` sends over USB.
//!
//! The host sends [`HostToBridge::Update`](wartui_proto::link::HostToBridge::Update) frames
//! carrying the `wartui_proto::spi::update` ops the C6 takes over SPI, and the P4 drives
//! [`wartui_proto::ota::Receiver`] over [`wartui_firmware::ota`] with them, as the C6 does
//! (`firmware/p4-radio/src/link.rs`). Each status goes back as a priority
//! [`BridgeToHost::Update`]. `Install` on a verified session switches slots and reboots.
//!
//! **The first host frame confirms an image on trial**, as a node confirms on hearing the
//! host. The transport sends `Identify` on connect, so that is normally the frame. A C6 bundle
//! that does not take still shows its fault, and the P4 keeps its new image.
//!
//! **A trial boot arms the RTC watchdog**, which resets the system after [`TRIAL_WATCHDOG`]
//! unless a host confirms first. The P4 has no operator power cycling it, so this is what counts
//! a hung image's boots: the third unconfirmed boot rolls back. A trial P4 left powered with no
//! host rolls back the same way. Confirming disarms it.
//!
//! **Flash work holds the executor.** Erasing and writing a sector takes tens of
//! milliseconds, well inside the host's reply wait, and the SPI link and USB writer wait behind
//! it. The receiver is reset when DTR falls, so a cable pulled mid-transfer leaves no session
//! for the next host to trip over.

use core::fmt::{self, Write};

use esp_hal::peripherals::{FLASH, RTC_TIMER};
use esp_hal::rtc_cntl::{Rtc, RwdtStage};
use esp_hal::time::Duration;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_firmware::ota::{Fault, Note, Ota};
use wartui_firmware::reboot;
use wartui_proto::air::{
    Board, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg, Version,
};
use wartui_proto::heapless::Vec;
use wartui_proto::link::{BridgeToHost, LogLevel, LogStr, UPDATE_STATUS_MAX, UpdateStatus};
use wartui_proto::ota::Receiver;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::spi::update::Op;

use crate::Shared;

/// How long a trial boot runs before the watchdog resets it, unless a host confirms it.
///
/// Long enough for the P4 to boot, hear its C6 and enumerate, and for the host to reopen the
/// port and send `Identify`. Three of these, about 90 s, roll back an image no host confirms.
pub const TRIAL_WATCHDOG: Duration = Duration::from_secs(30);

/// Lines for the host held until the next announcement, since nothing reaches the host
/// before its `Ready`: what the boot found, and the confirm.
pub type Lines = Vec<(Option<LogLevel>, LogStr), 8>;

/// The update receiver, with its sector of buffer.
static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(Board::Esp32p4));

/// Static, to keep the lines' kilobyte out of `main`'s future.
static UPDATES: StaticCell<Updates> = StaticCell::new();

/// Check whether this boot is a trial, and roll back or arm the watchdog as it says. Returns
/// the update side and the booted image's digest, for `Ready`.
///
/// Called before anything else is set up, and after `esp_hal::init`, which disarms every
/// watchdog. Out of line, so what it reads off flash stays out of `main`'s future.
#[inline(never)]
pub fn boot(
    flash: FLASH<'static>,
    rtc_timer: RTC_TIMER<'static>,
) -> (&'static mut Updates, Option<[u8; DIGEST_LEN]>) {
    let (mut ota, table_fault) = Ota::new(flash);
    let booted = ota.boot();
    if booted.reboot {
        reboot()
    }
    let image = ota.booted_digest();
    let rtc = Rtc::new(rtc_timer);
    let updates =
        UPDATES.init_with(|| Updates::new(ota, table_fault, booted.trial, &booted.notes, rtc));
    (updates, image)
}

/// The flash, the receiver an image goes through, and where this image stands.
pub struct Updates {
    ota: Ota,
    /// Static, since it holds a sector of buffer.
    receiver: &'static mut Receiver,
    /// The booted image is on trial until a host frame confirms it.
    trial: bool,
    /// Holds the RTC watchdog, armed while on trial.
    rtc: Rtc<'static>,
    /// Lines for the host's next announcement.
    lines: Lines,
    /// Why updates are refused, said with every announcement.
    fault: Option<LogStr>,
}

impl Updates {
    /// Take over the flash and the RTC, after [`Ota::boot`] ran with this `trial` and these
    /// `notes`, and arm the watchdog on a trial boot.
    fn new(
        ota: Ota,
        table_fault: Option<Fault>,
        trial: bool,
        notes: &[Note],
        mut rtc: Rtc<'static>,
    ) -> Self {
        if trial {
            rtc.rwdt.set_timeout(RwdtStage::Stage0, TRIAL_WATCHDOG);
            // Stage 0 resets the system; the others are off.
            rtc.rwdt.enable();
        }
        let mut lines = Lines::new();
        for note in notes {
            let level = if matches!(note, Note::Failed(..)) { None } else { Some(LogLevel::Info) };
            let _ = lines.push((level, text(format_args!("{note}"))));
        }
        let fault = match table_fault {
            Some(err) => Some(text(format_args!(
                "could not read the partition table: {err}; updates refused"
            ))),
            None if !ota.has_trial() => {
                Some(text(format_args!("no trial sector; updates refused")))
            }
            None => None,
        };
        Self { ota, receiver: RECEIVER.take(), trial, rtc, lines, fault }
    }

    /// End this image's trial, if it is on one, and disarm the watchdog. A failure leaves it
    /// on trial, and the next host frame tries again.
    fn confirm(&mut self) {
        if !self.trial {
            return;
        }
        let line = match self.ota.confirm() {
            Ok(()) => {
                self.trial = false;
                self.rtc.rwdt.disable();
                (Some(LogLevel::Info), text(format_args!("image confirmed by the host")))
            }
            Err(err) => (None, text(format_args!("could not confirm the image: {err}"))),
        };
        let _ = self.lines.push(line);
    }

    /// Hand an update op to the receiver, returning the status to send.
    ///
    /// A `Begin` is refused with [`OtaState::FlashError`] while this image is on trial, and on
    /// a table without a `trial` sector or a second app slot, as the C6 refuses one.
    fn take(&mut self, op: Op<'_>) -> Option<OtaStatusMsg> {
        let receiver = &mut *self.receiver;
        match op {
            Op::Begin { session, len, sha256 } => {
                // The link's `Begin` names no version, and the receiver checks none.
                let version = Version::new(0, 0, 0);
                let begin = OtaBeginMsg { session, len, sha256, version, board: receiver.board() };
                match (self.ota.slot(), self.trial) {
                    (Some(slot), false) => Some(receiver.begin(&begin, &slot)),
                    _ => {
                        Some(OtaStatusMsg { session, state: OtaState::FlashError, next_offset: 0 })
                    }
                }
            }
            Op::Chunk { session, offset, data } => match self.ota.slot() {
                Some(mut slot) => receiver.chunk(&OtaChunkMsg { session, offset, data }, &mut slot),
                None => Some(OtaStatusMsg { session, state: OtaState::Idle, next_offset: 0 }),
            },
            Op::Commit { session } => match self.ota.slot() {
                Some(mut slot) => Some(receiver.commit(&OtaCommitMsg { session }, &mut slot)),
                None => Some(OtaStatusMsg { session, state: OtaState::Idle, next_offset: 0 }),
            },
            Op::Install { session } => self.install(session),
            // Only the P4 sends a status.
            Op::Status(_) => None,
        }
    }

    /// Switch to the verified image and reboot into it. Returns only when the session is not
    /// verified, or the switch failed, with the status to send.
    fn install(&mut self, session: u8) -> Option<OtaStatusMsg> {
        let receiver = &mut *self.receiver;
        if !receiver.is_verified() || receiver.status().session != session {
            return Some(receiver.status());
        }
        match self.ota.install() {
            // The notes cannot reach the host before the reboot; the new image's `Ready` says
            // the rest.
            Ok(_) => reboot(),
            Err(err) => {
                let _ = self
                    .lines
                    .push((None, text(format_args!("could not switch to the new image: {err}"))));
                receiver.reset();
                Some(OtaStatusMsg { session, state: OtaState::FlashError, next_offset: 0 })
            }
        }
    }
}

/// A host frame decoded: confirm the image if it is on trial.
pub fn note_host_frame(shared: &Shared) {
    let trial = shared.updates.borrow().trial;
    if trial {
        shared.updates.borrow_mut().confirm();
    }
}

/// Carry out one `HostToBridge::Update` and answer it.
///
/// Out of line: a decoded op borrows the 1 KB frame, and the flash work below is deep.
#[inline(never)]
pub fn take(bytes: &[u8], shared: &Shared) {
    let Some(op) = Op::decode(bytes) else {
        shared.push_line(None, text(format_args!("an update op did not decode")));
        return;
    };
    let status = shared.updates.borrow_mut().take(op);
    if let Some(status) = status {
        answer(status, shared);
    }
    // An install that failed has a line to say.
    say_lines(shared);
}

/// Send the host `status`. Out of line, as each message is, so the two never share a frame.
#[inline(never)]
fn answer(status: OtaStatusMsg, shared: &Shared) {
    let mut buf = [0u8; UPDATE_STATUS_MAX];
    let len = Op::Status(status).encode(&mut buf);
    let status = UpdateStatus::from_slice(&buf[..len]).unwrap_or_default();
    shared.push_host(&BridgeToHost::Update(status));
}

/// Send the host the lines held for it.
#[inline(never)]
pub fn say_lines(shared: &Shared) {
    for (level, message) in take_lines(shared) {
        shared.push_line(level, message);
    }
}

/// The host let go of DTR: drop any session, so a transfer cut short leaves nothing behind.
pub fn host_gone(shared: &Shared) {
    shared.updates.borrow_mut().receiver.reset();
}

/// The lines held for the host, emptied.
pub fn take_lines(shared: &Shared) -> Lines {
    core::mem::take(&mut shared.updates.borrow_mut().lines)
}

/// Why updates are refused, if they are.
pub fn fault(shared: &Shared) -> Option<LogStr> {
    shared.updates.borrow().fault.clone()
}

fn text(args: fmt::Arguments<'_>) -> LogStr {
    let mut message = LogStr::new();
    let _ = message.write_fmt(args);
    message
}
