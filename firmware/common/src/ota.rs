//! Over-the-air updates on flash: the slot under [`wartui_proto::ota::Receiver`], the switch
//! to a new image, and the trial that image boots on. A node and the P4 bridge's C6 share it.
//! Each feeds its receiver its own way, a node from the air and the C6 from the SPI link.
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
//! naming the slot on trial, then one cleared bit per boot of it, in the fixed layout
//! [`wartui_proto::ota::trial`] defines and tests. The firmware writes the `otadata` states
//! too (`New`, `PendingVerify`, `Valid`, `Aborted`), for whoever reads them, but never decides
//! from them.
//!
//! **Every step is ordered so a power cut leaves a state the next boot reads correctly.**
//!
//! - Install ([`Ota::install`]): erase `trial`; arm it for the target; pin the booted slot,
//!   then select the target; `New`; the caller reboots.
//! - Trial boot ([`Ota::boot`]): `PendingVerify`; clear one count bit.
//! - Rollback: `Aborted`; select the other slot; erase `trial`; the caller reboots.
//! - Confirm ([`Ota::confirm`]): `Valid`; erase `trial`.
//!
//! A boot armed for the booted slot is a trial. One armed for the other slot, or holding
//! anything but an erased or armed record, is a leftover from an install or rollback a power
//! cut interrupted, and is erased. With `otadata` erased or unreadable, the image was flashed
//! over USB and nothing is on trial. A trial boot whose count cannot be read or written rolls
//! back rather than running uncounted, and a table without a `trial` sector refuses every
//! update.
//!
//! **Trial boots are counted in flash** because these boards lose power routinely, and RTC
//! memory does not survive that. There is no watchdog, so a hung image is caught the same
//! way: each power cycle counts a boot, given by the operator on a node and by the P4 on its
//! C6.
//!
//! **Nothing here logs.** Each step returns what happened, as [`Note`]s or a [`Fault`], and
//! the caller says it its own way and reboots where a step says to: a node prints to its USB
//! port, and the C6 sends lines up the link.

use core::fmt;

use embedded_storage::nor_flash::{NorFlash, ReadNorFlash};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    self, AppPartitionSubType, DataPartitionSubType, PARTITION_TABLE_MAX_LEN, PartitionType,
};
use esp_hal::peripherals::FLASH;
use esp_storage::{FlashStorage, FlashStorageError};
use static_cell::ConstStaticCell;
use wartui_proto::heapless::Vec;
use wartui_proto::ota::image::{DIGEST_LEN, appended_digest};
use wartui_proto::ota::trial::{self, Record};
use wartui_proto::ota::{SECTOR, SectorSink, TRIAL_BOOTS};

const SECTOR_U32: u32 = SECTOR as u32;

#[allow(clippy::cast_possible_truncation)]
const COUNT_OFFSET: u32 = trial::COUNT_OFFSET as u32;

static TABLE: ConstStaticCell<[u8; PARTITION_TABLE_MAX_LEN]> =
    ConstStaticCell::new([0; PARTITION_TABLE_MAX_LEN]);

/// Why a flash step failed.
#[derive(Debug)]
pub enum Fault {
    /// Reading the partition table or `otadata`.
    Table(partitions::Error),
    /// Erasing or writing flash directly.
    Flash(FlashStorageError),
    /// The table has no second app slot or no `trial` sector, or the booted slot is unknown.
    Layout,
    /// Every bit of the `trial` sector's count is clear.
    CountFull,
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

/// Something a step did or failed to do, for the caller to say.
#[derive(Debug)]
pub enum Note {
    /// `otadata` holds no update state: the image was flashed over USB.
    NoUpdateState,
    /// This boot is trial boot `count` of [`TRIAL_BOOTS`].
    TrialBoot(u32),
    /// The step named failed.
    Failed(&'static str, Fault),
    /// `otadata` now selects this slot, and `trial` is clear: the caller reboots.
    RolledBack(AppPartitionSubType),
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoUpdateState => f.write_str("image confirmed (no update state)"),
            Self::TrialBoot(count) => write!(f, "trial boot {count} of {TRIAL_BOOTS}"),
            Self::Failed(step, fault) => write!(f, "{step}: {fault}"),
            Self::RolledBack(kind) => write!(f, "rolled back to {kind:?}"),
        }
    }
}

/// What a step had to say, in order.
pub type Notes = Vec<Note, 6>;

/// What [`Ota::boot`] found.
#[derive(Debug)]
pub struct Booted {
    /// This boot is a trial, unconfirmed until [`Ota::confirm`].
    pub trial: bool,
    /// The trial is over and `otadata` selects the other slot: reboot after the notes.
    pub reboot: bool,
    /// What happened, for the caller to say.
    pub notes: Notes,
}

/// An app slot: its subtype, absolute offset and size.
#[derive(Debug, Clone, Copy)]
pub struct AppSlot {
    /// `ota_0` or `ota_1`.
    pub kind: AppPartitionSubType,
    /// Absolute offset on flash.
    pub base: u32,
    /// Bytes.
    pub size: u32,
}

/// Where the update's pieces are on flash, from the partition table.
#[derive(Debug, Clone, Copy, Default)]
struct Layout {
    /// The running slot.
    booted: Option<AppSlot>,
    /// The other app slot: where an image is written, and where a rollback returns to.
    target: Option<AppSlot>,
    /// Absolute offset of the `trial` sector.
    trial: Option<u32>,
}

/// The slot an image is written into, as the receiver sees it.
pub struct Slot<'a> {
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

/// The OTA app number of `kind`: 0 for `ota_0`, 1 for `ota_1`, `None` for any other.
const fn app_number(kind: AppPartitionSubType) -> Option<u8> {
    match kind {
        AppPartitionSubType::Ota0 => Some(0),
        AppPartitionSubType::Ota1 => Some(1),
        _ => None,
    }
}

/// The flash, and where the slots are.
pub struct Ota {
    flash: FlashStorage<'static>,
    table: &'static mut [u8; PARTITION_TABLE_MAX_LEN],
    layout: Layout,
}

impl Ota {
    /// Open the flash and read the partition table, with the fault if the table could not
    /// be read. Without it, every update is refused.
    ///
    /// # Panics
    /// If called twice: the table buffer is a static taken once.
    pub fn new(flash: FLASH<'static>) -> (Self, Option<Fault>) {
        let mut flash = FlashStorage::new(flash);
        let table = TABLE.take();
        let (layout, fault) = match locate(&mut flash, table) {
            Ok(layout) => (layout, None),
            Err(err) => (Layout::default(), Some(err)),
        };
        (Self { flash, table, layout }, fault)
    }

    /// The slot an update is written into, if the table has one.
    #[must_use]
    pub fn target(&self) -> Option<AppSlot> {
        self.layout.target
    }

    /// Whether the table has a `trial` sector. Without one, every update is refused.
    #[must_use]
    pub fn has_trial(&self) -> bool {
        self.layout.trial.is_some()
    }

    /// The slot to hand the receiver, or `None` when the table lacks a second app slot or a
    /// `trial` sector, and every update is refused.
    pub fn slot(&mut self) -> Option<Slot<'_>> {
        let target = self.layout.target?;
        self.layout.trial?;
        Some(Slot { flash: &mut self.flash, base: target.base, size: target.size })
    }

    /// The digest appended to the booted image, which names the build
    /// ([`wartui_proto::ota::image`]). `None` when the booted slot is unknown or its image has
    /// none.
    pub fn booted_digest(&mut self) -> Option<[u8; DIGEST_LEN]> {
        let booted = self.layout.booted?;
        let flash = &mut self.flash;
        appended_digest(|at, buf| {
            let fits = at.checked_add(buf.len() as u32).is_some_and(|end| end <= booted.size);
            fits && flash.read(booted.base + at, buf).is_ok()
        })
    }

    /// Check whether this boot is a trial, counting it if so.
    ///
    /// The `trial` sector decides, as the module documentation lays out. On the boot that
    /// reaches [`TRIAL_BOOTS`], and on a trial boot that cannot be counted, it rolls back and
    /// says to reboot.
    pub fn boot(&mut self) -> Booted {
        let mut booted = Booted { trial: false, reboot: false, notes: Notes::new() };
        let (Some(slot), Some(base)) =
            (self.layout.booted.and_then(|slot| app_number(slot.kind)), self.layout.trial)
        else {
            return booted;
        };
        let otadata = self.updater().and_then(|mut updater| Ok(updater.current_ota_state()?));
        let record = self.read_word(base).map(trial::decode_record);
        match (otadata, record) {
            (Ok(_), Ok(Record::Armed(armed))) if armed == slot => {}
            (Ok(state), Err(err)) => {
                note(&mut booted.notes, Note::Failed("could not read the trial sector", err));
                // Unknown whether this boot is armed. `otadata` is the only other witness.
                if matches!(state, OtaImageState::New | OtaImageState::PendingVerify) {
                    booted.reboot = self.roll_back(&mut booted.notes);
                }
                return booted;
            }
            (otadata, record) => {
                if otadata.is_err() {
                    note(&mut booted.notes, Note::NoUpdateState);
                }
                if !matches!(record, Ok(Record::Erased))
                    && let Err(err) = self.erase_trial()
                {
                    let failed = Note::Failed("could not clear a stale trial sector", err);
                    note(&mut booted.notes, failed);
                }
                return booted;
            }
        }
        booted.trial = true;
        if let Err(err) = self
            .updater()
            .and_then(|mut u| Ok(u.set_current_ota_state(OtaImageState::PendingVerify)?))
        {
            note(&mut booted.notes, Note::Failed("could not mark the image pending", err));
        }
        match self.count_trial_boot(base) {
            Ok(count) => {
                note(&mut booted.notes, Note::TrialBoot(count));
                if count >= TRIAL_BOOTS {
                    booted.reboot = self.roll_back(&mut booted.notes);
                }
            }
            Err(err) => {
                note(&mut booted.notes, Note::Failed("could not count the trial boot", err));
                booted.reboot = self.roll_back(&mut booted.notes);
            }
        }
        booted
    }

    /// End a trial: mark the image valid, then clear the `trial` sector.
    ///
    /// A failure leaves the image on trial, so the caller tries again on the next sign of
    /// its peer.
    ///
    /// # Errors
    /// The step that failed.
    pub fn confirm(&mut self) -> Result<(), Fault> {
        self.updater()
            .and_then(|mut updater| Ok(updater.set_current_ota_state(OtaImageState::Valid)?))
            .and_then(|()| self.erase_trial())
    }

    /// Switch to the image the receiver verified in the slot. On success the caller says
    /// the notes and reboots into it.
    ///
    /// The order is the module's install row. Once the target is selected, `New` is best
    /// effort: the `trial` sector already makes the next boot a trial.
    ///
    /// # Errors
    /// The step that failed before `otadata` selected the new slot. The running image stays.
    pub fn install(&mut self) -> Result<Notes, Fault> {
        let (Some(booted), Some(target), Some(base)) =
            (self.layout.booted, self.layout.target, self.layout.trial)
        else {
            return Err(Fault::Layout);
        };
        let armed = app_number(target.kind).and_then(trial::armed_word).ok_or(Fault::Layout)?;
        self.erase_trial()?;
        NorFlash::write(&mut self.flash, base, &armed)?;
        let mut updater = self.updater()?;
        let mut otadata = updater.ota_data()?;
        otadata.set_current_app_partition(booted.kind)?;
        otadata.set_current_app_partition(target.kind)?;
        let mut notes = Notes::new();
        if let Err(err) = otadata.set_current_ota_state(OtaImageState::New) {
            note(&mut notes, Note::Failed("could not mark the new image new", err.into()));
        }
        Ok(notes)
    }

    /// Return `otadata` to the slot this image replaced and clear the `trial` sector.
    /// Returns whether it did, and the caller should reboot.
    ///
    /// `Aborted` is written first, for whoever reads it. The `trial` sector is cleared only
    /// once the other slot is selected, so a power cut before then leaves the next boot armed
    /// and over the count, and it rolls back again. Returns `false`, leaving this image
    /// running, only if the other slot could not be selected.
    fn roll_back(&mut self, notes: &mut Notes) -> bool {
        let Some(target) = self.layout.target else {
            note(notes, Note::Failed("could not roll back", Fault::Layout));
            return false;
        };
        if let Err(err) =
            self.updater().and_then(|mut u| Ok(u.set_current_ota_state(OtaImageState::Aborted)?))
        {
            note(notes, Note::Failed("could not mark the image aborted", err));
        }
        if let Err(err) = self
            .updater()
            .and_then(|mut u| Ok(u.ota_data()?.set_current_app_partition(target.kind)?))
        {
            note(notes, Note::Failed("could not roll back", err));
            return false;
        }
        // The next boot finds the record armed for the other slot and clears it anyway.
        if let Err(err) = self.erase_trial() {
            note(notes, Note::Failed("could not clear the trial sector", err));
        }
        note(notes, Note::RolledBack(target.kind));
        true
    }

    /// Count this boot in the `trial` sector at `base` and return the count including it.
    ///
    /// [`trial::Count`] says which word to write; this only reads and writes it.
    fn count_trial_boot(&mut self, base: u32) -> Result<u32, Fault> {
        let mut count = trial::Count::new();
        for at in (base + COUNT_OFFSET..base + SECTOR_U32).step_by(trial::WORD) {
            if let Some((next, boots)) = count.step(self.read_word(at)?) {
                NorFlash::write(&mut self.flash, at, &next)?;
                return Ok(boots);
            }
        }
        Err(Fault::CountFull)
    }

    /// Read the flash word at `at`.
    fn read_word(&mut self, at: u32) -> Result<[u8; trial::WORD], Fault> {
        let mut word = [0u8; trial::WORD];
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
    /// It borrows the flash and the table buffer for as long as it lives, so the firmware
    /// opens one when it needs `otadata` rather than holding one across its main loop.
    fn updater(&mut self) -> Result<OtaUpdater<'_, 'static>, Fault> {
        Ok(OtaUpdater::new(&mut self.flash, self.table)?)
    }
}

/// Add `item` to `notes`. The list holds the most one step makes, so nothing is lost.
fn note(notes: &mut Notes, item: Note) {
    let _ = notes.push(item);
}

/// Find the booted slot, the other app slot and the `trial` sector.
fn locate(
    flash: &mut FlashStorage<'static>,
    table: &mut [u8; PARTITION_TABLE_MAX_LEN],
) -> Result<Layout, Fault> {
    let pt = partitions::read_partition_table(flash, table)?;
    let booted = pt.booted_partition()?;
    let booted_slot = booted.and_then(|entry| match entry.partition_type() {
        PartitionType::App(kind) => Some(AppSlot { kind, base: entry.offset(), size: entry.len() }),
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
    Ok(Layout { booted: booted_slot, target, trial })
}
