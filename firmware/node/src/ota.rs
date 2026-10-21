//! Over-the-air updates from the air: the host's update frames, handed to
//! [`wartui_proto::ota::Receiver`] over the flash [`wartui_firmware::ota`] owns.
//!
//! The slot, the switch to a new image and the trial it boots on are shared with the P4
//! bridge's C6, and their reasoning lives in `firmware/common/src/ota.rs`. This module is the
//! node's conversation: which frames reach the receiver, when update mode ends, and what each
//! step prints.
//!
//! **Any update frame from the host confirms a trial image first**, as an admin, clear or
//! update-confirm frame does. Hearing the host is the test, and an update accepted on trial
//! would write over the only slot known to work. So a `begin` the node still on trial would
//! take is refused with [`OtaState::FlashError`].

use esp_hal::peripherals::FLASH;
use esp_hal::time::{Duration, Instant};
use static_cell::ConstStaticCell;
use wartui_firmware::ota::Ota as Flash;
use wartui_firmware::reboot;
use wartui_proto::air::{
    OtaAbortMsg, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg,
};
use wartui_proto::ota::Receiver;

/// How long update mode waits for the next update frame before giving up on the session.
///
/// Long enough to cover a host that re-sends after a lost status, short enough that a host
/// that went away mid-update costs the fleet half a minute of this node, not the drive.
const SILENCE: Duration = Duration::from_millis(30_000);

static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(crate::BOARD));

/// An update frame, as [`Ota::on_update`] takes it.
pub enum Update<'a> {
    Begin(OtaBeginMsg),
    Chunk(OtaChunkMsg<'a>),
    Commit(OtaCommitMsg),
    Abort(OtaAbortMsg),
}

/// What [`Ota::on_update`] made of a frame.
pub struct Answer {
    /// The status to send its sender, if any.
    pub status: Option<OtaStatusMsg>,
    /// Whether the frame belongs to the session the receiver accepted: a `begin` it took,
    /// or a later frame naming the session it holds.
    pub accepted: bool,
}

/// The node's flash and its update session.
pub struct Ota {
    flash: Flash,
    receiver: &'static mut Receiver,
    /// When the last update frame arrived.
    heard: Instant,
    /// Update mode outlasts a session dropped after a failed install, until [`SILENCE`].
    lingering: bool,
}

impl Ota {
    /// Open the flash and read the partition table.
    pub fn new(flash: FLASH<'static>) -> Self {
        let (flash, fault) = Flash::new(flash);
        if let Some(err) = fault {
            note!("could not read the partition table: {}; updates refused", err);
        }
        if let Some(target) = flash.target() {
            note!("update slot {:?} at {:#x}, {:#x} bytes", target.kind, target.base, target.size);
        }
        if !flash.has_trial() {
            note!("no trial sector; updates refused");
        }
        Self { flash, receiver: RECEIVER.take(), heard: Instant::now(), lingering: false }
    }

    /// Check whether this boot is a trial, counting it if so. Returns whether it is.
    ///
    /// Rolls back and reboots, rather than returning, on the boot that reaches the trial's
    /// limit and on a trial boot that cannot be counted.
    pub fn boot(&mut self) -> bool {
        let booted = self.flash.boot();
        for said in &booted.notes {
            note!("{}", said);
        }
        if booted.reboot {
            reboot()
        }
        booted.trial
    }

    /// End a trial. Returns whether it took; a failure leaves the image on trial, so the
    /// next frame from the host tries again.
    pub fn confirm(&mut self) -> bool {
        match self.flash.confirm() {
            Ok(()) => {
                note!("image confirmed by the host");
                true
            }
            Err(err) => {
                note!("could not confirm the image: {}", err);
                false
            }
        }
    }

    /// Whether the node is in update mode: a session is open, or one dropped after a
    /// failed install has not yet gone [`SILENCE`] without an update frame.
    pub fn updating(&self) -> bool {
        self.receiver.is_active() || self.lingering
    }

    /// Leave update mode if no update frame has arrived for [`SILENCE`].
    pub fn expire(&mut self) {
        if self.updating() && self.heard.elapsed() >= SILENCE {
            note!("update abandoned: no update frame for {} s", SILENCE.as_secs());
            self.receiver.reset();
            self.lingering = false;
        }
    }

    /// Hand an update frame to the receiver.
    ///
    /// `on_trial` is whether the running image is still unconfirmed. A `begin` is refused
    /// with [`OtaState::FlashError`] then, and on a table without a `trial` sector or a
    /// second app slot.
    pub fn on_update(&mut self, update: &Update<'_>, on_trial: bool) -> Answer {
        self.heard = Instant::now();
        let held = self.receiver.is_active().then(|| self.receiver.status().session);
        let (Some(mut slot), false) = (self.flash.slot(), on_trial) else {
            return Self::refuse(update);
        };
        let answer = match update {
            Update::Begin(msg) => {
                let status = self.receiver.begin(msg, &slot);
                let accepted = matches!(status.state, OtaState::Receiving | OtaState::Verified);
                Answer { status: Some(status), accepted }
            }
            Update::Chunk(msg) => Answer {
                status: self.receiver.chunk(msg, &mut slot),
                accepted: held == Some(msg.session),
            },
            Update::Commit(msg) => Answer {
                status: Some(self.receiver.commit(msg, &mut slot)),
                accepted: held == Some(msg.session),
            },
            Update::Abort(msg) => Answer {
                status: Some(self.receiver.abort(msg)),
                accepted: held == Some(msg.session),
            },
        };
        if self.receiver.is_active() {
            self.lingering = false;
        }
        answer
    }

    /// Refuse a frame without the receiver: a `begin` with [`OtaState::FlashError`], and
    /// anything else with [`OtaState::Idle`], since no session can be open.
    fn refuse(update: &Update<'_>) -> Answer {
        let (session, state) = match update {
            Update::Begin(msg) => (msg.session, OtaState::FlashError),
            Update::Chunk(msg) => (msg.session, OtaState::Idle),
            Update::Commit(OtaCommitMsg { session }) | Update::Abort(OtaAbortMsg { session }) => {
                (*session, OtaState::Idle)
            }
        };
        Answer { status: Some(OtaStatusMsg { session, state, next_offset: 0 }), accepted: false }
    }

    /// Switch to the verified image and reboot into it.
    ///
    /// Returns only if the switch failed before `otadata` selected the new slot, with the
    /// [`OtaState::FlashError`] status to send. The node stays on the running image, and
    /// the session is dropped so an identical `begin` starts afresh. Update mode lasts
    /// until [`SILENCE`] passes without an update frame.
    pub fn install(&mut self) -> OtaStatusMsg {
        let session = self.receiver.status().session;
        match self.flash.install() {
            Ok(notes) => {
                for said in &notes {
                    note!("{}", said);
                }
                note!("rebooting into the new image");
                reboot()
            }
            Err(err) => {
                note!("could not switch to the new image: {}", err);
                self.receiver.reset();
                self.lingering = true;
                OtaStatusMsg { session, state: OtaState::FlashError, next_offset: 0 }
            }
        }
    }
}
