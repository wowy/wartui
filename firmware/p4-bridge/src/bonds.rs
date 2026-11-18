//! The Bluetooth bonds the P4 keeps for its C6, in the second sector of its `nvs` partition, and
//! the pairing window. `wartui_proto::bonds` has the records' layout.
//!
//! **The P4 is the bonds' system of record.** It reads the sector once at boot ([`boot`]), and
//! sends the C6 every bond after each hello answer, so the C6, which keeps no flash state, holds
//! them across a restart of either chip and an update of the C6. A bond the C6 reports is applied
//! at once, stored, and sent back down, so the C6 drops one the store evicted. The C6 repeats a
//! new bond until it hears it back, so a repeat of one already held only sends the bonds down
//! again. Each write parks core 1, as a settings write does (`src/settings.rs`). A write that
//! fails leaves the change applied until reboot and says so to the host.
//!
//! **`Down::Bonds` goes on every change and is repeated every [`REPEAT_EVERY_MS`]** while the
//! pair bridges ([`repeat`]). The link never resends, so a lost one would leave the C6 serving
//! bonds the P4 dropped until the next hello. The C6 takes a repeat that changes nothing as
//! changing nothing. Only a change that finds the queue full says so; a repeat stays silent.
//!
//! **The pairing window** is open while no bond is stored, for the first [`WINDOW_MS`] after the
//! P4 boots, and for [`WINDOW_MS`] after a host's `OpenPairing`. The host link reaches that
//! command only from USB or a bonded central. Its time left goes down in each `Down::Bonds`, and
//! the C6 times its close.
//!
//! **Clearing the bonds strands every host that held one.** A host whose bond the bridge dropped
//! fails every connect until it forgets the bridge and pairs again
//! (`docs/c6-ble-pairing-findings.md`, "What a host has to know"), so `ClearBonds` says so.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use wartui_proto::bonds::{self, BondAddr, BondRecord, MAX_BONDS, Record, write_for};
use wartui_proto::link::{LogStr, PairWindow};
use wartui_proto::mac;
use wartui_proto::ota::SECTOR;
use wartui_proto::radio_link::{Down, WINDOW_UNTIL_BONDED};

use crate::settings::Unsaved;
use crate::update::Updates;
use crate::{Shared, now_ms};

/// How long the pairing window stays open after the P4 boots, or after a host opens it.
pub const WINDOW_MS: u64 = wartui_proto::link::PAIR_WINDOW_SECS as u64 * 1000;

/// How often the P4 sends its bonds to a bridging C6 again, changed or not.
pub const REPEAT_EVERY_MS: u64 = 5_000;

/// The bonds, when a host last opened the window, and when they last went to the C6.
pub struct Bonds {
    held: RefCell<bonds::Bonds>,
    /// P4 milliseconds at the latest `OpenPairing`.
    opened_at: Cell<Option<u64>>,
    /// P4 milliseconds at the latest `Down::Bonds` the down queue took.
    sent_at: Cell<u64>,
}

impl Bonds {
    /// How many bonds are stored.
    pub fn count(&self) -> u8 {
        self.held.borrow().len() as u8
    }

    /// Milliseconds the window stays open after `now`: [`WINDOW_UNTIL_BONDED`] while no bond is
    /// stored, else whichever of the boot's and the host's windows ends later, or 0 once both
    /// have.
    pub fn window_ms(&self, now: u64) -> u32 {
        if self.held.borrow().is_empty() {
            return WINDOW_UNTIL_BONDED;
        }
        let opened = self.opened_at.get().map_or(0, |at| at + WINDOW_MS);
        let left = WINDOW_MS.max(opened).saturating_sub(now);
        u32::try_from(left).unwrap_or(WINDOW_UNTIL_BONDED - 1)
    }

    /// The window at `now`, as `Status` reports it.
    pub fn window(&self, now: u64) -> PairWindow {
        match self.window_ms(now) {
            0 => PairWindow::Closed,
            WINDOW_UNTIL_BONDED => PairWindow::UntilBonded,
            ms => PairWindow::Open { secs: u16::try_from(ms.div_ceil(1000)).unwrap_or(u16::MAX) },
        }
    }
}

/// The bonds the sector holds, read into `image`: none when the partition does not read.
///
/// Out of line, as `settings::boot` is.
#[inline(never)]
pub fn boot(updates: &mut Updates, image: &mut [u8; SECTOR]) -> Bonds {
    let held = updates
        .bonds()
        .and_then(|mut sector| sector.read(image).ok())
        .map(|()| bonds::read(image))
        .unwrap_or_default();
    Bonds { held: RefCell::new(held), opened_at: Cell::new(None), sent_at: Cell::new(0) }
}

/// Send the C6 every bond and the window's time left.
pub fn send(shared: &Shared) {
    if !push(shared) {
        shared.push_error("Bluetooth bonds not sent to the radio: its queue is full");
    }
}

/// Send the bonds again once [`REPEAT_EVERY_MS`] has passed since the down queue last took them.
/// Silent when it refuses them: the next repeat tries again.
pub fn repeat(shared: &Shared) {
    if now_ms().saturating_sub(shared.bonds.sent_at.get()) >= REPEAT_EVERY_MS {
        push(shared);
    }
}

/// Queue `Down::Bonds`, and return whether the down queue took it.
fn push(shared: &Shared) -> bool {
    let now = now_ms();
    let bonds = shared.bonds.held.borrow().clone();
    let window_ms = shared.bonds.window_ms(now);
    // The P4 admits no phone to the WardriveESP service yet.
    let pushed = shared.push_down(&Down::Bonds { bonds, window_ms, wardrive_esp: false });
    if pushed {
        shared.bonds.sent_at.set(now);
    }
    pushed
}

/// The C6 bonded a central: store it as the newest, evicting the oldest past [`MAX_BONDS`]. A
/// repeat of a bond already held only sends the bonds down: the C6 missed their echo.
pub fn on_added(shared: &Shared, bond: BondRecord) {
    if shared.bonds.held.borrow().contains(&bond) {
        send(shared);
        return;
    }
    let evicted = {
        let held = shared.bonds.held.borrow();
        let new = !held.iter().any(|kept| kept.addr == bond.addr);
        (new && held.is_full()).then(|| held[0].addr)
    };
    change(shared, &Record::Add(bond));
    let added = bond.addr.mac();
    let mut line = LogStr::new();
    let count = shared.bonds.count();
    let _ = write!(line, "Bluetooth bond {} stored, {count} of {MAX_BONDS}", mac::short(&added));
    if let Some(old) = evicted {
        let old = old.mac();
        let _ = write!(line, "; {} evicted", mac::short(&old));
    }
    shared.push_log(&line);
}

/// The C6 is replacing a bond a central lost: drop the old one.
pub fn on_removed(shared: &Shared, addr: BondAddr) {
    change(shared, &Record::Remove(addr));
}

/// A host's `OpenPairing`: open the window for [`WINDOW_MS`].
pub fn open_window(shared: &Shared) {
    shared.bonds.opened_at.set(Some(now_ms()));
    send(shared);
    let mut line = LogStr::new();
    let _ = write!(line, "Bluetooth pairing window open for {} s", WINDOW_MS / 1000);
    shared.push_log(&line);
}

/// A host's `ClearBonds`: drop every bond, which opens the window until one is stored.
pub fn clear(shared: &Shared) {
    shared.bonds.held.borrow_mut().clear();
    let erased = erase(shared);
    send(shared);
    match erased {
        Ok(()) => {
            shared.push_log("Bluetooth bonds cleared; pairing window open until one is stored");
        }
        Err(why) => {
            let mut message = LogStr::new();
            let _ = write!(message, "Bluetooth bonds cleared until reboot, not on flash: {why}");
            shared.push_error(&message);
        }
    }
    shared.push_log("every host that paired must forget the bridge, then pair again");
}

/// Apply `record`, store it, and send the C6 the result.
fn change(shared: &Shared, record: &Record) {
    let next = bonds::apply(&shared.bonds.held.borrow(), record);
    shared.bonds.held.replace(next);
    if let Err(why) = save(shared, record) {
        let mut message = LogStr::new();
        let _ = write!(message, "Bluetooth bond not saved, so it holds until reboot: {why}");
        shared.push_error(&message);
    }
    send(shared);
}

/// Store `record`, unless it changes nothing the sector holds: at its next erased slot, or, when
/// it is full, by erasing it and writing every bond it holds.
fn save(shared: &Shared, record: &Record) -> Result<(), Unsaved> {
    crate::settings::store(shared, Updates::bonds, |image| write_for(image, record))
}

/// Erase the sector, unless it holds nothing.
fn erase(shared: &Shared) -> Result<(), Unsaved> {
    let mut updates = shared.updates.borrow_mut();
    let mut sector = updates.bonds().ok_or(Unsaved::NoPartition)?;
    let mut image = shared.sector.borrow_mut();
    sector.read(&mut image)?;
    if bonds::is_written(&image) {
        sector.erase()?;
    }
    Ok(())
}
