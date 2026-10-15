//! The node's `trial` sector: which app slot is on trial, and how many times it has booted.
//!
//! **The sector, not `otadata`, decides whether a boot is a trial.** The prebuilt espflash
//! bootloader selects a slot by `otadata` sequence alone and reads no image state, so the
//! node rolls back on its own, and the state it writes to `otadata` is never what it decides
//! from. The sector is ordered against the `otadata` writes so a power cut cannot leave an
//! image running untrialed; `firmware/node/src/ota.rs` has the order.
//!
//! **The layout is fixed, and does not change before 1.0.** It is on-flash state that one
//! firmware writes and the next reads across an update, possibly between minor versions, as
//! the frozen update frames are ([`crate::air`]). The first word is the record:
//! [`ARMED_MAGIC`] and the slot's OTA app number, 0 or 1, or all `0xFF` when nothing is on
//! trial. The rest of the sector is the boot count, starting at [`COUNT_OFFSET`].
//!
//! **A boot clears one bit and erases nothing.** NOR flash programs only by clearing bits, so
//! each boot clears the lowest set bit of the first count byte that is not yet zero, and the
//! count is the number of clear bits. These nodes lose power routinely and RTC memory does not
//! survive that; a power cut here loses at most the bit being written.
//!
//! How many boots a trial gets is the firmware's policy, not part of the layout: the image on
//! trial applies its own.

use super::SECTOR;

/// The first three bytes of an armed record. The fourth names the slot on trial.
pub const ARMED_MAGIC: [u8; 3] = *b"WTR";

/// Bytes in a flash word, the unit the record and the count are read and written in.
pub const WORD: usize = 4;

/// Where the boot count starts in the sector, past the record.
pub const COUNT_OFFSET: usize = WORD;

/// Words in the count.
pub const COUNT_WORDS: usize = (SECTOR - COUNT_OFFSET) / WORD;

/// What the record says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Record {
    /// Erased: nothing on trial.
    Erased,
    /// Armed for the slot with this OTA app number, 0 or 1.
    Armed(u8),
    /// Anything else, such as a write a power cut interrupted.
    Garbage,
}

/// The record arming a trial of OTA app slot `slot`, or `None` for a slot other than 0 or 1.
#[must_use]
pub const fn armed_word(slot: u8) -> Option<[u8; WORD]> {
    if slot > 1 {
        return None;
    }
    Some([ARMED_MAGIC[0], ARMED_MAGIC[1], ARMED_MAGIC[2], slot])
}

/// Read a record.
#[must_use]
pub fn decode_record(word: [u8; WORD]) -> Record {
    match word {
        [0xFF, 0xFF, 0xFF, 0xFF] => Record::Erased,
        [a, b, c, slot @ (0 | 1)] if [a, b, c] == ARMED_MAGIC => Record::Armed(slot),
        _ => Record::Garbage,
    }
}

/// One more boot in a count word: the bits already clear in it, and the word with the next
/// one cleared. `None` when every bit is clear.
#[must_use]
pub fn count_word(word: [u8; WORD]) -> Option<(u32, [u8; WORD])> {
    let i = word.iter().position(|&byte| byte != 0)?;
    let mut next = word;
    next[i] &= next[i] - 1;
    // `i` is below `WORD`.
    #[allow(clippy::cast_possible_truncation)]
    let cleared = 8 * i as u32 + word[i].count_zeros();
    Some((cleared, next))
}

/// Counts a boot across the count words, read in order from the first.
#[derive(Debug, Clone, Copy, Default)]
pub struct Count {
    cleared: u32,
}

impl Count {
    /// A count that has read no words.
    #[must_use]
    pub const fn new() -> Self {
        Self { cleared: 0 }
    }

    /// Take the next count word. `Some` names the word to write in its place and the count
    /// including this boot, and the walk stops there. `None` means the word is full and the
    /// walk goes on to the next; past [`COUNT_WORDS`] the count is full.
    pub fn step(&mut self, word: [u8; WORD]) -> Option<([u8; WORD], u32)> {
        match count_word(word) {
            Some((cleared, next)) => Some((next, self.cleared + cleared + 1)),
            None => {
                self.cleared += 32;
                None
            }
        }
    }
}
