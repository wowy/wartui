//! The `trial` sector's record and boot count.

use wartui_proto::ota::SECTOR;
use wartui_proto::ota::trial::{
    ARMED_MAGIC, COUNT_OFFSET, COUNT_WORDS, Count, Record, WORD, armed_word, decode_record,
};

/// Count one boot in `sector` as the node does, returning the count or `None` when full.
fn boot(sector: &mut [u8; SECTOR]) -> Option<u32> {
    let mut count = Count::new();
    for i in 0..COUNT_WORDS {
        let at = COUNT_OFFSET + i * WORD;
        let old: [u8; WORD] = sector[at..at + WORD].try_into().unwrap();
        if let Some((new, n)) = count.step(old) {
            for (o, n) in old.iter().zip(&new) {
                // NOR flash programs only by clearing bits: no write may set one.
                assert_eq!(n & !o, 0, "word {i} sets a bit: {old:02x?} -> {new:02x?}");
            }
            sector[at..at + WORD].copy_from_slice(&new);
            return Some(n);
        }
    }
    None
}

#[test]
fn trial_record_round_trips_when_armed_for_either_slot() {
    assert_eq!(decode_record(armed_word(0).unwrap()), Record::Armed(0));
    assert_eq!(decode_record(armed_word(1).unwrap()), Record::Armed(1));
}

#[test]
fn trial_record_refuses_to_arm_when_slot_is_past_one() {
    assert_eq!(armed_word(2), None);
    assert_eq!(armed_word(0xFF), None);
}

#[test]
fn trial_record_decodes_erased_when_sector_is_erased() {
    assert_eq!(decode_record([0xFF; WORD]), Record::Erased);
}

#[test]
fn trial_record_decodes_garbage_when_magic_or_slot_is_wrong() {
    let [a, b, c] = ARMED_MAGIC;
    for word in [
        [b'X', b, c, 0],
        [a, b, b'X', 1],
        [a, b, c, 2],
        [a, b, c, 0xFF],
        // A record whose write a power cut interrupted part way.
        [a, b, 0xFF, 0xFF],
        [a, 0xFF, 0xFF, 0xFF],
        [0, 0, 0, 0],
    ] {
        assert_eq!(decode_record(word), Record::Garbage, "{word:02x?}");
    }
}

#[test]
fn trial_record_counts_each_boot_when_crossing_word_boundaries() {
    let mut sector = [0xFF; SECTOR];
    // Past the 32 bits of the first count word, and well past any trial's limit.
    for expected in 1..=70 {
        assert_eq!(boot(&mut sector), Some(expected));
    }
    let cleared: u32 = sector[COUNT_OFFSET..].iter().map(|b| b.count_zeros()).sum();
    assert_eq!(cleared, 70);
    // The count never reaches into the record.
    assert_eq!(sector[..COUNT_OFFSET], [0xFF; COUNT_OFFSET]);
}

#[test]
fn trial_record_reports_full_when_every_count_bit_is_clear() {
    let mut sector = [0xFF; SECTOR];
    sector[COUNT_OFFSET..].fill(0);
    assert_eq!(boot(&mut sector), None);

    // The last bit is the last boot counted.
    let last = SECTOR - 1;
    sector[last] = 0x80;
    let total = u32::try_from(COUNT_WORDS * WORD * 8).unwrap();
    assert_eq!(boot(&mut sector), Some(total));
    assert_eq!(boot(&mut sector), None);
}
