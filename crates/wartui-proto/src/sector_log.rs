//! A flash sector kept as a log of fixed-size records, the layout the P4's settings
//! ([`crate::settings`]) and bonds ([`crate::bonds`]) sectors share.
//!
//! **A change is one record at the first erased slot.** NOR flash programs only by clearing bits,
//! so a change costs a slot written and nothing erased. A slot is erased while every byte reads
//! `0xFF`, and the log ends at the first one ([`written`]). **A full sector is erased and
//! rewritten** with what it holds after the change ([`Write::Erase`]). What a record means, and
//! whether a change changes anything, is the owner's.

use crate::ota::SECTOR;

/// The flash write a change takes, in `SLOT`-byte records, with up to `HELD` bytes rewritten
/// after an erase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write<const SLOT: usize, const HELD: usize> {
    /// Write `record` at `offset`, the sector's first erased slot.
    At {
        /// The byte offset in the sector.
        offset: usize,
        /// The slot to write.
        record: [u8; SLOT],
    },
    /// Erase the sector, then write `records[..len]` at 0.
    Erase {
        /// What the sector holds after the change, as its owner lays it out.
        records: [u8; HELD],
        /// Bytes of `records` to write.
        len: usize,
    },
}

/// The slots up to the first erased one.
pub fn written<const SLOT: usize>(sector: &[u8; SECTOR]) -> impl Iterator<Item = &[u8; SLOT]> {
    sector.as_chunks::<SLOT>().0.iter().take_while(|slot| **slot != [0xFF; SLOT])
}

/// The write that logs `record` in `sector`: at the first erased slot while one is left, else an
/// erase and what `rewrite` lays out from 0. `rewrite` gets an erased buffer and returns the bytes
/// it filled.
pub fn write_for<const SLOT: usize, const HELD: usize>(
    sector: &[u8; SECTOR],
    record: [u8; SLOT],
    rewrite: impl FnOnce(&mut [u8; HELD]) -> usize,
) -> Write<SLOT, HELD> {
    let used = written::<SLOT>(sector).count();
    if used < SECTOR / SLOT {
        return Write::At { offset: used * SLOT, record };
    }
    let mut records = [0xFF; HELD];
    let len = rewrite(&mut records);
    Write::Erase { records, len }
}

#[cfg(test)]
mod tests {
    use super::{SECTOR, Write, write_for, written};

    /// Fills the rewrite buffer with `0x11` and says it filled `len` bytes.
    fn rewrite(len: usize) -> impl FnOnce(&mut [u8; 8]) -> usize {
        move |records| {
            records[..len].fill(0x11);
            len
        }
    }

    #[test]
    fn sector_log_writes_at_first_erased_slot_when_slots_are_left() {
        let mut sector = [0xFF; SECTOR];
        sector[..8].fill(0);
        assert_eq!(
            write_for::<4, 8>(&sector, [1, 2, 3, 4], rewrite(4)),
            Write::At { offset: 8, record: [1, 2, 3, 4] }
        );
    }

    #[test]
    fn sector_log_ends_at_first_erased_slot_when_a_later_slot_is_written() {
        let mut sector = [0xFF; SECTOR];
        sector[..4].fill(0);
        sector[8..12].fill(0);
        assert_eq!(written::<4>(&sector).count(), 1);
    }

    #[test]
    fn sector_log_erases_and_rewrites_when_sector_is_full() {
        let sector = [0; SECTOR];
        let Write::Erase { records, len } = write_for::<4, 8>(&sector, [1; 4], rewrite(4)) else {
            panic!("a full sector takes an erase");
        };
        assert_eq!(len, 4);
        assert_eq!(records, [0x11, 0x11, 0x11, 0x11, 0xFF, 0xFF, 0xFF, 0xFF], "the rest erased");
    }

    #[test]
    fn sector_log_counts_whole_slots_only_when_slot_does_not_divide_sector() {
        // 48-byte slots leave 16 bytes at the sector's end that no slot covers.
        let sector = [0; SECTOR];
        assert_eq!(written::<48>(&sector).count(), SECTOR / 48);
    }
}
