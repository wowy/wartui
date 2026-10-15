//! Over-the-air updates: the flash under [`wartui_proto::ota::Receiver`], the switch to a
//! new image, and the trial that image boots on.
//!
//! The receiver decides what to write; this module only says where. An image goes into the
//! app slot that is not booted, through [`Slot`], at that slot's offset on flash.
//!
//! **The switch writes `otadata` through [`Ota`](esp_bootloader_esp_idf::ota::Ota), never
//! through `OtaUpdater::activate_next_partition`.** With `otadata` erased, as every USB
//! flash leaves it, `OtaUpdater` reads the selection as the factory slot this table does not
//! have. Its next slot is then the booted one, and it numbers `ota_1` with sequence 0, which
//! names the right slot only through `u32` wraparound. So [`Ota::install`] first pins
//! `otadata` to the running slot, a no-op once it names it, and then selects the other. The
//! entry left behind names the running image, which is the one a rollback returns to.
//!
//! **Rollback is this firmware's, because the bootloader has none.** The prebuilt espflash
//! bootloader selects a slot by `otadata` sequence alone and reads no image state. So the
//! `trial` sector, not `otadata`, decides whether a boot is a trial. It holds an armed record
//! naming the slot on trial, then one cleared bit per boot of it. The node writes the
//! `otadata` states too (`New`, `PendingVerify`, `Valid`, `Aborted`), for whoever reads them,
//! but never decides from them.
//!
//! **Every step is ordered so a power cut leaves a state the next boot reads correctly.**
//!
//! - Install ([`Ota::install`]): erase `trial`; arm it for the target; pin the booted slot,
//!   then select the target; `New`; reboot.
//! - Trial boot ([`Ota::boot`]): `PendingVerify`; clear one count bit.
//! - Rollback: `Aborted`; select the other slot; erase `trial`; reboot.
//! - Confirm ([`Ota::confirm`]): `Valid`; erase `trial`.
//!
//! A boot armed for the booted slot is a trial. One armed for the other slot, or holding
//! anything but an erased or armed record, is a leftover from an install or rollback a power
//! cut interrupted, and is erased. With `otadata` erased or unreadable, the node was flashed
//! over USB and nothing is on trial. A trial boot whose count cannot be read or written rolls
//! back rather than running uncounted, and a table without a `trial` sector refuses every
//! update.
//!
//! **Trial boots are counted in flash, one bit each.** These nodes lose power routinely,
//! and RTC memory does not survive that. Clearing a bit on NOR flash needs no erase, so
//! each boot clears one more bit, and a power cut can lose at most the bit being written.
//! There is no watchdog, so a hung image is caught the same way: each power cycle the
//! operator gives it counts a boot.
//!
//! **Any update frame from the host confirms a trial image first**, as an admin, clear or
//! update-confirm frame does. Hearing the host is the test, and an update accepted on trial
//! would write over the only slot known to work. So a `begin` the node still on trial would
//! take is refused with [`OtaState::FlashError`].

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType,
};
use esp_hal::peripherals::FLASH;
use esp_hal::time::{Duration, Instant};
use esp_storage::{FlashStorage, FlashStorageError};
use static_cell::ConstStaticCell;
use wartui_firmware::reboot;
use wartui_proto::air::{
    OtaAbortMsg, OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg,
};
use wartui_proto::ota::{Receiver, SECTOR, SectorSink};

/// Unconfirmed boots of a new image before it is rolled back.
///
/// Three rather than one: a node runs on a USB-C PD supply, which can reset it once while
/// it negotiates, so a single boot that never heard the host is not evidence of a bad image.
/// A hung image reaches three too, since there is no watchdog and each power cycle the
/// operator gives it counts.
const TRIAL_BOOTS: u32 = 3;

/// How long update mode waits for the next update frame before giving up on the session.
///
/// Long enough to cover a host that re-sends after a lost status, short enough that a host
/// that went away mid-update costs the fleet half a minute of this node, not the drive.
const SILENCE: Duration = Duration::from_millis(30_000);

const SECTOR_U32: u32 = SECTOR as u32;

/// The first three bytes of an armed `trial` sector. The fourth names the slot on trial.
const ARMED_MAGIC: [u8; 3] = *b"WTR";

/// Where the boot count starts in the `trial` sector, past the armed record.
const COUNT_OFFSET: u32 = 4;

static TABLE: ConstStaticCell<[u8; PARTITION_TABLE_MAX_LEN]> =
    ConstStaticCell::new([0; PARTITION_TABLE_MAX_LEN]);

static RECEIVER: ConstStaticCell<Receiver> = ConstStaticCell::new(Receiver::new(crate::BOARD));

/// Why a flash step failed.
enum Fault {
    /// Reading the partition table or `otadata`.
    Table(partitions::Error),
    /// Erasing or writing flash directly.
    Flash(FlashStorageError),
    /// The table has no second app slot or no `trial` sector, or the booted slot is unknown.
    Layout,
    /// Every bit of the `trial` sector's count is clear.
    CountFull,
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Table(err) => write!(f, "partition table or otadata: {err:?}"),
            Self::Flash(err) => write!(f, "flash: {err:?}"),
            Self::Layout => f.write_str("no update slot or trial sector in the partition table"),
            Self::CountFull => f.write_str("trial count full"),
        }
    }
}

impl From<partitions::Error> for Fault {
    fn from(err: partitions::Error) -> Self {
        Self::Table(err)
    }
}

impl From<FlashStorageError> for Fault {
    fn from(err: FlashStorageError) -> Self {
        Self::Flash(err)
    }
}

/// An app slot: its subtype, absolute offset and size.
#[derive(Debug, Clone, Copy)]
struct AppSlot {
    kind: AppPartitionSubType,
    base: u32,
    size: u32,
}

/// Where the update's pieces are on flash, from the partition table.
#[derive(Debug, Clone, Copy, Default)]
struct Layout {
    /// The running slot.
    booted: Option<AppPartitionSubType>,
    /// The other app slot: where an image is written, and where a rollback returns to.
    target: Option<AppSlot>,
    /// Absolute offset of the `trial` sector.
    trial: Option<u32>,
}

/// The slot an image is written into, as the receiver sees it.
struct Slot<'a> {
    flash: &'a mut FlashStorage<'static>,
    base: u32,
    size: u32,
}

impl SectorSink for Slot<'_> {
    type Error = FlashStorageError;

    fn capacity(&self) -> u32 {
        self.size
    }

    fn erase_sector(&mut self, offset: u32) -> Result<(), Self::Error> {
        let at = self.base + offset;
        NorFlash::erase(self.flash, at, at + SECTOR_U32)
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), Self::Error> {
        NorFlash::write(self.flash, self.base + offset, data)
    }
}

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

/// What the first word of the `trial` sector says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Record {
    /// Erased: nothing on trial.
    Erased,
    /// Armed for the slot with this OTA app number.
    Armed(u8),
    /// Anything else.
    Garbage,
}

/// The first word of a `trial` sector armed for OTA app slot `slot`.
const fn armed_word(slot: u8) -> [u8; 4] {
    [ARMED_MAGIC[0], ARMED_MAGIC[1], ARMED_MAGIC[2], slot]
}

/// Read the first word of a `trial` sector.
fn decode_record(word: [u8; 4]) -> Record {
    match word {
        [0xFF, 0xFF, 0xFF, 0xFF] => Record::Erased,
        [a, b, c, slot @ (0 | 1)] if [a, b, c] == ARMED_MAGIC => Record::Armed(slot),
        _ => Record::Garbage,
    }
}

/// The OTA app number of `kind`: 0 for `ota_0`, 1 for `ota_1`, `None` for any other.
const fn app_number(kind: AppPartitionSubType) -> Option<u8> {
    match kind {
        AppPartitionSubType::Ota0 => Some(0),
        AppPartitionSubType::Ota1 => Some(1),
        _ => None,
    }
}

/// Count one more boot in a word of the count: the bits already clear in it, and the word
/// with the next one cleared. `None` when the word is full.
///
/// Bytes clear in order, each from its lowest set bit, so the first byte that is not zero is
/// the one to clear next and the count is the number of clear bits.
fn count_word(word: [u8; 4]) -> Option<(u32, [u8; 4])> {
    let i = word.iter().position(|&byte| byte != 0)?;
    let mut next = word;
    next[i] &= next[i] - 1;
    // `i` is at most 3.
    #[allow(clippy::cast_possible_truncation)]
    let cleared = 8 * i as u32 + word[i].count_zeros();
    Some((cleared, next))
}

/// The node's flash, its update session, and where the slots are.
pub struct Ota {
    flash: FlashStorage<'static>,
    table: &'static mut [u8; PARTITION_TABLE_MAX_LEN],
    receiver: &'static mut Receiver,
    layout: Layout,
    /// When the last update frame arrived.
    heard: Instant,
    /// Update mode outlasts a session dropped after a failed install, until [`SILENCE`].
    lingering: bool,
}

impl Ota {
    /// Open the flash and read the partition table.
    pub fn new(flash: FLASH<'static>) -> Self {
        let mut flash = FlashStorage::new(flash);
        let table = TABLE.take();
        let layout = match locate(&mut flash, table) {
            Ok(layout) => layout,
            Err(err) => {
                note!("could not read the partition table: {}; updates refused", err);
                Layout::default()
            }
        };
        if let Some(target) = layout.target {
            note!("update slot {:?} at {:#x}, {:#x} bytes", target.kind, target.base, target.size);
        }
        if layout.trial.is_none() {
            note!("no trial sector; updates refused");
        }
        Self {
            flash,
            table,
            receiver: RECEIVER.take(),
            layout,
            heard: Instant::now(),
            lingering: false,
        }
    }

    /// Check whether this boot is a trial, counting it if so. Returns whether it is.
    ///
    /// The `trial` sector decides, as the module documentation lays out. Rolls back and
    /// reboots, rather than returning, on the boot that reaches [`TRIAL_BOOTS`] and on a
    /// trial boot that cannot be counted.
    pub fn boot(&mut self) -> bool {
        let (Some(booted), Some(base)) =
            (self.layout.booted.and_then(app_number), self.layout.trial)
        else {
            return false;
        };
        let otadata = self.updater().and_then(|mut updater| Ok(updater.current_ota_state()?));
        let record = self.read_word(base).map(decode_record);
        match (otadata, record) {
            (Ok(_), Ok(Record::Armed(slot))) if slot == booted => {}
            (Ok(state), Err(err)) => {
                note!("could not read the trial sector: {}", err);
                // Unknown whether this boot is armed. `otadata` is the only other witness.
                if matches!(state, OtaImageState::New | OtaImageState::PendingVerify) {
                    self.roll_back();
                }
                return false;
            }
            (otadata, record) => {
                if otadata.is_err() {
                    note!("image confirmed (no update state)");
                }
                if !matches!(record, Ok(Record::Erased))
                    && let Err(err) = self.erase_trial()
                {
                    note!("could not clear a stale trial sector: {}", err);
                }
                return false;
            }
        }
        if let Err(err) = self
            .updater()
            .and_then(|mut u| Ok(u.set_current_ota_state(OtaImageState::PendingVerify)?))
        {
            note!("could not mark the image pending: {}", err);
        }
        match self.count_trial_boot(base) {
            Ok(count) => {
                note!("trial boot {} of {}", count, TRIAL_BOOTS);
                if count >= TRIAL_BOOTS {
                    self.roll_back();
                }
            }
            Err(err) => {
                note!("could not count the trial boot: {}", err);
                self.roll_back();
            }
        }
        true
    }

    /// End a trial: mark the image valid, then clear the `trial` sector.
    ///
    /// Returns whether it took. A failure leaves the image on trial, so the next frame
    /// from the host tries again.
    pub fn confirm(&mut self) -> bool {
        let result = self
            .updater()
            .and_then(|mut updater| Ok(updater.set_current_ota_state(OtaImageState::Valid)?))
            .and_then(|()| self.erase_trial());
        match result {
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
        let (Some(target), Some(_), false) = (self.layout.target, self.layout.trial, on_trial)
        else {
            return Self::refuse(update);
        };
        let mut slot = Slot { flash: &mut self.flash, base: target.base, size: target.size };
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
        match self.switch() {
            Ok(()) => {
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

    /// Arm the `trial` sector for the target and select it in `otadata`.
    ///
    /// The order is the module's install row. Once the target is selected, `New` is best
    /// effort: the `trial` sector already makes the next boot a trial.
    fn switch(&mut self) -> Result<(), Fault> {
        let (Some(booted), Some(target), Some(base)) =
            (self.layout.booted, self.layout.target, self.layout.trial)
        else {
            return Err(Fault::Layout);
        };
        let slot = app_number(target.kind).ok_or(Fault::Layout)?;
        self.erase_trial()?;
        NorFlash::write(&mut self.flash, base, &armed_word(slot))?;
        let mut updater = self.updater()?;
        let mut otadata = updater.ota_data()?;
        otadata.set_current_app_partition(booted)?;
        otadata.set_current_app_partition(target.kind)?;
        if let Err(err) = otadata.set_current_ota_state(OtaImageState::New) {
            note!("could not mark the new image new: {:?}", err);
        }
        Ok(())
    }

    /// Return `otadata` to the slot this image replaced, clear the `trial` sector and
    /// reboot.
    ///
    /// `Aborted` is written first, for whoever reads it. The `trial` sector is cleared only
    /// once the other slot is selected, so a power cut before then leaves the next boot armed
    /// and over the count, and it rolls back again. Returns, leaving this image running, only
    /// if the other slot could not be selected.
    fn roll_back(&mut self) {
        let Some(target) = self.layout.target else {
            note!("could not roll back: {}", Fault::Layout);
            return;
        };
        if let Err(err) =
            self.updater().and_then(|mut u| Ok(u.set_current_ota_state(OtaImageState::Aborted)?))
        {
            note!("could not mark the image aborted: {}", err);
        }
        if let Err(err) = self
            .updater()
            .and_then(|mut u| Ok(u.ota_data()?.set_current_app_partition(target.kind)?))
        {
            note!("could not roll back: {}", err);
            return;
        }
        // The next boot finds the record armed for the other slot and clears it anyway.
        if let Err(err) = self.erase_trial() {
            note!("could not clear the trial sector: {}", err);
        }
        note!("rolled back to {:?}", target.kind);
        reboot()
    }

    /// Count this boot in the `trial` sector at `base` and return the count including it.
    fn count_trial_boot(&mut self, base: u32) -> Result<u32, Fault> {
        let mut cleared = 0;
        for at in (base + COUNT_OFFSET..base + SECTOR_U32).step_by(4) {
            let word = self.read_word(at)?;
            match count_word(word) {
                Some((in_word, next)) => {
                    NorFlash::write(&mut self.flash, at, &next)?;
                    return Ok(cleared + in_word + 1);
                }
                None => cleared += 32,
            }
        }
        Err(Fault::CountFull)
    }

    /// Read the flash word at `at`.
    fn read_word(&mut self, at: u32) -> Result<[u8; 4], Fault> {
        let mut word = [0u8; 4];
        ReadNorFlash::read(&mut self.flash, at, &mut word)?;
        Ok(word)
    }

    /// Erase the `trial` sector: nothing on trial.
    fn erase_trial(&mut self) -> Result<(), Fault> {
        let base = self.layout.trial.ok_or(Fault::Layout)?;
        NorFlash::erase(&mut self.flash, base, base + SECTOR_U32)?;
        Ok(())
    }

    /// An `OtaUpdater` over the flash, opened for one step and dropped after it.
    ///
    /// It borrows the flash and the table buffer for as long as it lives, so the node
    /// opens one when it needs `otadata` rather than holding one across the main loop.
    fn updater(&mut self) -> Result<OtaUpdater<'_, FlashStorage<'static>>, Fault> {
        Ok(OtaUpdater::new(&mut self.flash, self.table)?)
    }
}

/// Find the booted slot, the other app slot and the `trial` sector.
fn locate(
    flash: &mut FlashStorage<'static>,
    table: &mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Result<Layout, Fault> {
    let pt = partitions::read_partition_table(flash, table)?;
    let booted = pt.booted_partition()?;
    let booted_kind = booted.and_then(|entry| match entry.partition_type() {
        PartitionType::App(kind) => Some(kind),
        _ => None,
    });
    let target = booted.and_then(|booted| {
        pt.iter().find_map(|entry| match entry.partition_type() {
            PartitionType::App(AppPartitionSubType::Factory | AppPartitionSubType::Test) => None,
            PartitionType::App(kind) if entry.offset() != booted.offset() => {
                Some(AppSlot { kind, base: entry.offset(), size: entry.len() })
            }
            _ => None,
        })
    });
    let trial = pt
        .find_partition(PartitionType::Data(DataPartitionSubType::Undefined))?
        .filter(|entry| entry.len() >= SECTOR_U32)
        .map(|entry| entry.offset());
    Ok(Layout { booted: booted_kind, target, trial })
}
