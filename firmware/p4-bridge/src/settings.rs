//! The settings the P4 keeps on flash: the lid fan's duty and the channel pool, in the first
//! sector of its `nvs` partition. `wartui_proto::settings` has the records'
//! layout.
//!
//! **The P4 is the settings' system of record.** It reads the sector once at boot, on every
//! board, before the futures start ([`boot`]), and runs what it holds, or the defaults when
//! nothing is stored or the partition does not read. A host's change applies at once and is
//! written only when the sector does not hold it, so a repeat costs a sector read and no flash
//! write. Each write parks core 1, as every P4 flash write does (`wartui_firmware::ota`), and
//! one change in 1024 erases the sector first. A write that fails leaves the change applied until
//! reboot and says so to the host, and the same change again retries it.

use core::fmt::{self, Write as _};

use static_cell::ConstStaticCell;
use wartui_firmware::ota::Fault;
use wartui_proto::ChannelPool;
use wartui_proto::link::LogStr;
use wartui_proto::ota::SECTOR;
use wartui_proto::settings::{self, Setting, Stored, Write, write_for};

use crate::Shared;
use crate::update::Updates;

/// The sector's image, read before each write. Static, to keep 4 KB off the stack.
static SECTOR_IMAGE: ConstStaticCell<[u8; SECTOR]> = ConstStaticCell::new([0; SECTOR]);

/// The sector's image, and what it holds: nothing when the partition does not read.
///
/// Out of line, as `update::boot` is.
///
/// # Panics
/// If called twice: the image is a static taken once.
#[inline(never)]
pub fn boot(updates: &mut Updates) -> (&'static mut [u8; SECTOR], Stored) {
    let image = SECTOR_IMAGE.take();
    let stored = updates
        .settings()
        .and_then(|mut sector| sector.read(image).ok())
        .map(|()| settings::read(image))
        .unwrap_or_default();
    (image, stored)
}

/// A `SetPool`: run `pool` now, and store it when the sector does not hold it, so a repeat
/// retries a save that failed.
#[inline(never)]
pub fn set_pool(shared: &Shared, pool: ChannelPool) {
    crate::fleet::set_pool(shared, pool);
    if let Err(why) = save(shared, Setting::Pool(pool)) {
        let mut message = LogStr::new();
        let _ = write!(message, "pool {pool} not saved, so it holds until reboot: {why}");
        shared.push_error(&message);
    }
}

/// Why a setting was not saved.
pub enum Unsaved {
    /// The table has no `nvs` partition.
    NoPartition,
    /// Reading, erasing or writing the sector.
    Flash(Fault),
}

impl fmt::Display for Unsaved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoPartition => f.write_str("no nvs partition"),
            Self::Flash(err) => write!(f, "{err}"),
        }
    }
}

impl From<Fault> for Unsaved {
    fn from(err: Fault) -> Self {
        Self::Flash(err)
    }
}

/// Store `setting`, unless the sector already holds it: at its next erased word, or, when it is
/// full, by erasing it and writing every setting it holds.
pub fn save(shared: &Shared, setting: Setting) -> Result<(), Unsaved> {
    let mut updates = shared.updates.borrow_mut();
    let mut sector = updates.settings().ok_or(Unsaved::NoPartition)?;
    let mut image = shared.sector.borrow_mut();
    sector.read(&mut image)?;
    match write_for(&image, setting) {
        None => {}
        // `offset` is inside one sector.
        #[allow(clippy::cast_possible_truncation)]
        Some(Write::At { offset, record }) => sector.write(offset as u32, &record)?,
        Some(Write::Erase { records, len }) => {
            sector.erase()?;
            sector.write(0, &records[..len])?;
        }
    }
    Ok(())
}
