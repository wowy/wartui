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
//! bootloader selects a slot by `otadata` and never changes an image's state. An image the
//! node switched to boots in state `New`, and every boot in `New` or `PendingVerify` is a
//! trial boot. Hearing the host over the air confirms it ([`Ota::confirm`]). Reaching
//! [`TRIAL_BOOTS`] unconfirmed rolls it back ([`Ota::boot`]).
//!
//! **Trial boots are counted in flash, one bit each.** These nodes lose power routinely,
//! and RTC memory does not survive that. Clearing a bit on NOR flash needs no erase, so
//! each boot clears one more bit of the `trial` sector, and a power cut can lose at most the
//! bit being written. There is no watchdog, so a hung image is caught the same way: each
//! power cycle the operator gives it counts a boot.

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
use wartui_proto::air::{Frame, OtaState, OtaStatusMsg};
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
}

impl core::fmt::Display for Fault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Table(err) => write!(f, "partition table or otadata: {err:?}"),
            Self::Flash(err) => write!(f, "flash: {err:?}"),
            Self::Layout => f.write_str("no update slot or trial sector in the partition table"),
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

/// The node's flash, its update session, and where the slots are.
pub struct Ota {
    flash: FlashStorage<'static>,
    table: &'static mut [u8; PARTITION_TABLE_MAX_LEN],
    receiver: &'static mut Receiver,
    layout: Layout,
    /// When the last update frame arrived.
    heard: Instant,
    /// The verified image could not be switched to. Every status for it says
    /// [`OtaState::FlashError`] until the session ends.
    failed: bool,
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
        Self {
            flash,
            table,
            receiver: RECEIVER.take(),
            layout,
            heard: Instant::now(),
            failed: false,
        }
    }

    /// Check whether this boot is a trial, counting it if so. Returns whether it is.
    ///
    /// Rolls back and reboots, rather than returning, on the boot that reaches
    /// [`TRIAL_BOOTS`]. A slot `otadata` does not select, or one in any state but `New` or
    /// `PendingVerify`, is confirmed. That covers a USB flash, whose erased `otadata` has
    /// no state at all, and a slot the bootloader fell back to.
    pub fn boot(&mut self) -> bool {
        let Some(booted) = self.layout.booted else { return false };
        let state = match self.updater().and_then(|mut updater| {
            let selected = updater.selected_partition()?;
            Ok((selected, updater.current_ota_state()?))
        }) {
            Ok((selected, state @ (OtaImageState::New | OtaImageState::PendingVerify)))
                if selected == booted =>
            {
                state
            }
            Ok((_, state)) => {
                note!("image confirmed ({:?})", state);
                return false;
            }
            Err(_) => {
                note!("image confirmed (no update state)");
                return false;
            }
        };
        if state == OtaImageState::New
            && let Err(err) = self
                .updater()
                .and_then(|mut u| Ok(u.set_current_ota_state(OtaImageState::PendingVerify)?))
        {
            note!("could not mark the image pending: {}", err);
        }
        let count = match self.count_trial_boot() {
            Ok(count) => count,
            Err(err) => {
                note!("trial boot, uncounted: {}", err);
                return true;
            }
        };
        note!("trial boot {} of {}", count, TRIAL_BOOTS);
        if count >= TRIAL_BOOTS {
            match self.roll_back() {
                Ok(()) => {
                    note!("rolled back after {} unconfirmed boots", count);
                    reboot()
                }
                Err(err) => note!("could not roll back: {}", err),
            }
        }
        true
    }

    /// End a trial: mark the image valid and clear the boot count.
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

    /// Whether an update session is open: the node is in update mode.
    pub fn updating(&self) -> bool {
        self.receiver.is_active()
    }

    /// Drop the session if no update frame has arrived for [`SILENCE`].
    pub fn expire(&mut self) {
        if self.receiver.is_active() && self.heard.elapsed() >= SILENCE {
            note!("update abandoned: no update frame for {} s", SILENCE.as_secs());
            self.receiver.reset();
            self.failed = false;
        }
    }

    /// Hand an update frame to the receiver, returning the status to send its sender.
    ///
    /// `None` for a frame that is not an update frame, and for a chunk the receiver takes
    /// without comment.
    pub fn on_frame(&mut self, frame: &Frame<'_>) -> Option<OtaStatusMsg> {
        let (base, size) = self.layout.target.map_or((0, 0), |t| (t.base, t.size));
        // A zero-sized slot makes the receiver refuse every `begin` as too large, which is
        // the answer for a table this node could not read.
        let mut slot = Slot { flash: &mut self.flash, base, size };
        let status = match frame {
            Frame::OtaBegin(msg) => Some(self.receiver.begin(msg, &slot)),
            Frame::OtaChunk(msg) => self.receiver.chunk(msg, &mut slot),
            Frame::OtaCommit(msg) => Some(self.receiver.commit(msg, &mut slot)),
            Frame::OtaAbort(msg) => Some(self.receiver.abort(msg)),
            _ => return None,
        };
        self.heard = Instant::now();
        if !self.receiver.is_verified() {
            self.failed = false;
        }
        status.map(|mut status| {
            if self.failed && status.state == OtaState::Verified {
                status.state = OtaState::FlashError;
            }
            status
        })
    }

    /// Switch to the verified image and reboot into it.
    ///
    /// Returns only if a flash step failed, leaving the node on the running image and
    /// every later status for the session saying [`OtaState::FlashError`].
    pub fn install(&mut self) {
        match self.switch() {
            Ok(()) => {
                note!("rebooting into the new image");
                reboot()
            }
            Err(err) => {
                note!("could not switch to the new image: {}", err);
                self.failed = true;
            }
        }
    }

    /// Select the target slot in `otadata`, in state `New`, and clear the trial count.
    ///
    /// The order matters. Selecting a slot rewrites the next `otadata` entry and keeps
    /// whatever state that entry held, so the state is set after, on the entry just
    /// selected.
    fn switch(&mut self) -> Result<(), Fault> {
        let (Some(booted), Some(target)) = (self.layout.booted, self.layout.target) else {
            return Err(Fault::Layout);
        };
        {
            let mut updater = self.updater()?;
            let mut otadata = updater.ota_data()?;
            otadata.set_current_app_partition(booted)?;
            otadata.set_current_app_partition(target.kind)?;
            otadata.set_current_ota_state(OtaImageState::New)?;
        }
        self.erase_trial()
    }

    /// Return `otadata` to the slot this image replaced.
    ///
    /// Marks this image's entry `Aborted` first, so the bootloader skips it even if power
    /// fails before the other entry is written, then selects the other slot and clears the
    /// trial count.
    fn roll_back(&mut self) -> Result<(), Fault> {
        let Some(target) = self.layout.target else { return Err(Fault::Layout) };
        {
            let mut updater = self.updater()?;
            updater.set_current_ota_state(OtaImageState::Aborted)?;
            updater.ota_data()?.set_current_app_partition(target.kind)?;
        }
        self.erase_trial()
    }

    /// Count this boot in the `trial` sector and return the count including it.
    ///
    /// The count is the number of cleared bits. Bytes clear in order, each from its lowest
    /// set bit, so the first byte that is not zero is the one to clear next. The write is
    /// the word holding that byte, with every other bit as it was; programming NOR flash
    /// only clears bits, so the word needs no erase.
    fn count_trial_boot(&mut self) -> Result<u32, Fault> {
        let base = self.layout.trial.ok_or(Fault::Layout)?;
        let mut cleared = 0;
        for at in (base..base + SECTOR_U32).step_by(4) {
            let mut word = [0u8; 4];
            ReadNorFlash::read(&mut self.flash, at, &mut word)?;
            let Some(i) = word.iter().position(|&byte| byte != 0) else {
                cleared += 32;
                continue;
            };
            // `i` is at most 3.
            #[allow(clippy::cast_possible_truncation)]
            {
                cleared += 8 * i as u32 + word[i].count_zeros();
            }
            word[i] &= word[i] - 1;
            NorFlash::write(&mut self.flash, at, &word)?;
            return Ok(cleared + 1);
        }
        // Every bit is clear: more boots than a trial ever reaches.
        Ok(cleared)
    }

    /// Erase the `trial` sector, setting the count to zero.
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
