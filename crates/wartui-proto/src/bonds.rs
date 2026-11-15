//! The Bluetooth bonds the P4 keeps for its C6, and their records in the second sector of its
//! `nvs` partition.
//!
//! **The P4 is the bonds' system of record, and the C6 keeps no flash state.** An update rewrites
//! the C6's flash, and a C6 that held its bonds there would lose every host's pairing to one. The
//! C6 holds them in its Bluetooth host's RAM. The P4 reads the sector at boot and sends the C6
//! every bond after each hello answer ([`Down::Bonds`](crate::radio_link::Down::Bonds)), so a bond
//! survives a C6 restart, a P4 restart and a C6 update alike. The C6 reports each change
//! ([`Up::BondAdded`](crate::radio_link::Up::BondAdded),
//! [`Up::BondRemoved`](crate::radio_link::Up::BondRemoved)). A [`BondRecord`] is what the C6's
//! host stack needs of a bond, in this crate's own types, so this crate takes no dependency on
//! that stack.
//!
//! **At most [`MAX_BONDS`], newest last.** A pairing past them evicts the oldest. A bond paired
//! again moves to newest, so the bond evicted is the one least recently paired.
//!
//! **One [`SLOT`]-byte record a change, written at the first erased slot** ([`sector_log`], as
//! the settings sector is): an add or a remove, each ending in a [`crc32`] over the rest. Reading
//! ([`read`]) walks the slots up to the first erased one and replays each valid record in order,
//! so the last record for an address wins and the eviction replays with it. A slot a power cut
//! tore fails its CRC and is skipped.
//!
//! **A full sector erases and rewrites what it holds** ([`sector_log::Write::Erase`]): the bonds
//! [`read`] gives, oldest first, with the change applied. A power cut between the erase and the
//! rewrite loses every bond, and each host pairs again inside the next boot's window.

use heapless::Vec;
use serde::{Deserialize, Serialize};

use crate::crc::crc32;
use crate::mac::Mac;
use crate::ota::SECTOR;
use crate::sector_log;

/// The most bonds the store holds.
pub const MAX_BONDS: usize = 4;

/// Bytes in a record: a whole number of flash words.
pub const SLOT: usize = 48;

/// Records a sector holds.
pub const SLOTS: usize = SECTOR / SLOT;

/// The tag of an add's record.
pub const ADD: u8 = b'A';

/// The tag of a remove's record.
pub const REMOVE: u8 = b'R';

/// Bytes of a record ahead of its CRC.
const BODY: usize = SLOT - 4;

/// A bonded central's identity address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BondAddr {
    /// The HCI address type: 0 public, 1 random static.
    pub kind: u8,
    /// The address in HCI order, least significant octet first.
    pub addr: [u8; 6],
}

impl BondAddr {
    /// The address in display order, most significant octet first, for [`crate::mac`]'s forms.
    #[must_use]
    pub const fn mac(&self) -> Mac {
        let [a, b, c, d, e, f] = self.addr;
        [f, e, d, c, b, a]
    }
}

/// The security level a bond's key reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BondLevel {
    /// Encrypted, with no protection against a man in the middle: Just Works.
    Encrypted,
    /// Encrypted and authenticated.
    Authenticated,
}

/// One bond: what the C6 needs to recognize a central and resume its encryption.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BondRecord {
    /// The central's identity address.
    pub addr: BondAddr,
    /// Its identity resolving key, which recognizes it behind a resolvable private address.
    pub irk: Option<u128>,
    /// The long term key.
    pub ltk: u128,
    /// The level the key reached.
    pub level: BondLevel,
}

/// The bonds a sector holds, oldest first.
pub type Bonds = Vec<BondRecord, MAX_BONDS>;

/// One change, as a record stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// Store this bond, replacing any for its address, as the newest.
    Add(BondRecord),
    /// Drop the bond for this address.
    Remove(BondAddr),
}

/// The flash write a change takes: one slot, or an erase and every bond the sector holds after
/// the change, as adds, oldest first.
pub type Write = sector_log::Write<SLOT, { MAX_BONDS * SLOT }>;

/// `bonds` with `record` applied: an add replaces its address's bond and goes last, evicting
/// the oldest when the store is full; a remove drops its address's bond.
#[must_use]
pub fn apply(bonds: &Bonds, record: &Record) -> Bonds {
    let mut next = bonds.clone();
    match record {
        Record::Add(bond) => {
            next.retain(|kept| kept.addr != bond.addr);
            if next.is_full() {
                next.remove(0);
            }
            // Room was made above.
            let _ = next.push(*bond);
        }
        Record::Remove(addr) => next.retain(|kept| kept.addr != *addr),
    }
    next
}

/// The byte a level's record stores.
const fn level_byte(level: BondLevel) -> u8 {
    match level {
        BondLevel::Encrypted => 1,
        BondLevel::Authenticated => 2,
    }
}

/// The level `byte` stores, or `None` for a byte no level stores.
const fn level_from(byte: u8) -> Option<BondLevel> {
    match byte {
        1 => Some(BondLevel::Encrypted),
        2 => Some(BondLevel::Authenticated),
        _ => None,
    }
}

/// The record for `change`: tag, address type, address, level, whether an IRK follows, two
/// zero bytes, the LTK and the IRK little-endian, then the CRC-32 over all of it, little-endian.
/// A remove leaves everything after the address zero.
#[must_use]
pub fn record(change: &Record) -> [u8; SLOT] {
    let mut out = [0; SLOT];
    let addr = match change {
        Record::Add(bond) => {
            out[0] = ADD;
            out[8] = level_byte(bond.level);
            out[9] = u8::from(bond.irk.is_some());
            out[12..28].copy_from_slice(&bond.ltk.to_le_bytes());
            out[28..44].copy_from_slice(&bond.irk.unwrap_or(0).to_le_bytes());
            bond.addr
        }
        Record::Remove(addr) => {
            out[0] = REMOVE;
            *addr
        }
    };
    out[1] = addr.kind;
    out[2..8].copy_from_slice(&addr.addr);
    let crc = crc32(&out[..BODY]);
    out[BODY..].copy_from_slice(&crc.to_le_bytes());
    out
}

/// The change `slot` records, or `None` for an erased, torn or unknown slot.
fn decode(slot: &[u8; SLOT]) -> Option<Record> {
    let (body, crc) = slot.split_at(BODY);
    if crc32(body).to_le_bytes() != crc {
        return None;
    }
    let mut octets = [0; 6];
    octets.copy_from_slice(&body[2..8]);
    let addr = BondAddr { kind: body[1], addr: octets };
    let u128_at = |at: usize| {
        let mut bytes = [0; 16];
        bytes.copy_from_slice(&body[at..at + 16]);
        u128::from_le_bytes(bytes)
    };
    match body[0] {
        ADD => {
            let level = level_from(body[8])?;
            let irk = match body[9] {
                0 => None,
                1 => Some(u128_at(28)),
                _ => return None,
            };
            Some(Record::Add(BondRecord { addr, irk, ltk: u128_at(12), level }))
        }
        REMOVE => Some(Record::Remove(addr)),
        _ => None,
    }
}

/// The slots up to the first erased one.
fn written(sector: &[u8; SECTOR]) -> impl Iterator<Item = &[u8; SLOT]> {
    sector_log::written(sector)
}

/// What `sector` holds: its valid records replayed in order.
#[must_use]
pub fn read(sector: &[u8; SECTOR]) -> Bonds {
    written(sector).filter_map(decode).fold(Bonds::new(), |bonds, change| apply(&bonds, &change))
}

/// The write that stores `change` in `sector`, or `None` when it changes nothing.
///
/// At the first erased slot while one is left; once the sector is full, an erase and every bond
/// it holds after the change rewritten, oldest first.
#[must_use]
pub fn write_for(sector: &[u8; SECTOR], change: &Record) -> Option<Write> {
    let stored = read(sector);
    let kept = apply(&stored, change);
    if kept == stored {
        return None;
    }
    Some(sector_log::write_for(sector, record(change), |records| {
        for (at, bond) in kept.iter().enumerate() {
            records[at * SLOT..(at + 1) * SLOT].copy_from_slice(&record(&Record::Add(*bond)));
        }
        kept.len() * SLOT
    }))
}

/// Whether `sector` holds any record, so that clearing it takes an erase.
#[must_use]
pub fn is_written(sector: &[u8; SECTOR]) -> bool {
    written(sector).next().is_some()
}
