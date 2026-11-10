//! Updates over the node bus: the host's update records, handed to
//! [`wartui_proto::ota::Receiver`] over the flash [`wartui_firmware::ota`] owns.
//!
//! The slot, the switch to a new image and the trial it boots on are shared with the P4
//! bridge's C6, and their reasoning lives in `firmware/common/src/ota.rs`. This module is the
//! node's conversation: which records reach the receiver, when update mode ends, and what each
//! step prints.
//!
//! **Any update record confirms a trial image first**, as an assign, stop or update-confirm
//! record does. Hearing the bridge is the test, and an update accepted on trial
//! would write over the only slot known to work. So a `begin` the node still on trial would
//! take is refused with [`UpdateState::FlashError`].

use esp_hal::peripherals::FLASH;
use esp_hal::time::{Duration, Instant};
use static_cell::ConstStaticCell;
use wartui_firmware::ota::Ota as Flash;
use wartui_firmware::reboot;
use wartui_proto::bus::{
    UpdateAbort, UpdateBegin, UpdateChunk, UpdateCommit, UpdateState, UpdateStatus,
};
use wartui_proto::ota::Receiver;

/// How long update mode waits for the next update record before giving up on the session.
///
/// Long enough to cover a host that re-sends after a lost status, short enough that a host
/// that went away mid-update costs the fleet half a minute of this node, not the drive.
const SILENCE: Duration = Duration::from_millis(30_000);

static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(crate::BOARD));

/// An update record, as [`Ota::on_update`] takes it.
pub enum Update<'a> {
    Begin(UpdateBegin),
    Chunk(UpdateChunk<'a>),
    Commit(UpdateCommit),
    Abort(UpdateAbort),
}

/// The node's flash and its update session.
pub struct Ota {
    flash: Flash,
    receiver: &'static mut Receiver,
    /// When the last update record arrived.
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
    /// next record from the host tries again.
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
    /// failed install has not yet gone [`SILENCE`] without an update record.
    pub fn updating(&self) -> bool {
        self.receiver.is_active() || self.lingering
    }

    /// Leave update mode if no update record has arrived for [`SILENCE`].
    pub fn expire(&mut self) {
        if self.updating() && self.heard.elapsed() >= SILENCE {
            note!("update abandoned: no update record for {} s", SILENCE.as_secs());
            self.receiver.reset();
            self.lingering = false;
        }
    }

    /// Hand an update record to the receiver.
    ///
    /// `on_trial` is whether the running image is still unconfirmed. A `begin` is refused
    /// with [`UpdateState::FlashError`] then, and on a table without a `trial` sector or a
    /// second app slot. Returns the status to send back, if any.
    pub fn on_update(&mut self, update: &Update<'_>, on_trial: bool) -> Option<UpdateStatus> {
        self.heard = Instant::now();
        let (Some(mut slot), false) = (self.flash.slot(), on_trial) else {
            return Some(Self::refuse(update));
        };
        let status = match update {
            Update::Begin(msg) => Some(self.receiver.begin(msg, &slot)),
            Update::Chunk(msg) => self.receiver.chunk(msg, &mut slot),
            Update::Commit(msg) => Some(self.receiver.commit(msg, &mut slot)),
            Update::Abort(msg) => Some(self.receiver.abort(msg)),
        };
        if self.receiver.is_active() {
            self.lingering = false;
        }
        status
    }

    /// Refuse a record without the receiver: a `begin` with [`UpdateState::FlashError`], and
    /// anything else with [`UpdateState::Idle`], since no session can be open.
    fn refuse(update: &Update<'_>) -> UpdateStatus {
        let (session, state) = match update {
            Update::Begin(msg) => (msg.session, UpdateState::FlashError),
            Update::Chunk(msg) => (msg.session, UpdateState::Idle),
            Update::Commit(UpdateCommit { session }) | Update::Abort(UpdateAbort { session }) => {
                (*session, UpdateState::Idle)
            }
        };
        UpdateStatus { session, state, next_offset: 0 }
    }

    /// Switch to the verified image and reboot into it.
    ///
    /// Returns only if the switch failed before `otadata` selected the new slot, with the
    /// [`UpdateState::FlashError`] status to send. The node stays on the running image, and
    /// the session is dropped so an identical `begin` starts afresh. Update mode lasts
    /// until [`SILENCE`] passes without an update record.
    pub fn install(&mut self) -> UpdateStatus {
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
                UpdateStatus { session, state: UpdateState::FlashError, next_offset: 0 }
            }
        }
    }
}
