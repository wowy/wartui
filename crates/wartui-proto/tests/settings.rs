//! The settings sector: its records, how it reads, and the write each change takes.

use wartui_proto::ChannelPool;
use wartui_proto::fan::FanDuty;
use wartui_proto::ota::SECTOR;
use wartui_proto::settings::{
    self, FAN, POOL, RECORDS, Setting, Stored, WORD, Write, record, write_for,
};

fn duty(pct: u8) -> FanDuty {
    FanDuty::new(pct).expect("in range")
}

fn fan(pct: u8) -> Setting {
    Setting::Fan(duty(pct))
}

/// `sector` with `setting` stored as the P4 stores it. Panics when the sector already holds it.
fn store(sector: &mut [u8; SECTOR], setting: Setting) {
    match write_for(sector, setting).expect("the sector does not hold it") {
        Write::At { offset, record } => {
            for (old, new) in sector[offset..offset + WORD].iter().zip(&record) {
                // NOR flash programs only by clearing bits: no write may set one.
                assert_eq!(new & !old, 0, "the record sets a bit at {offset}");
            }
            sector[offset..offset + WORD].copy_from_slice(&record);
        }
        Write::Erase { records, len } => {
            sector.fill(0xFF);
            sector[..len].copy_from_slice(&records[..len]);
        }
    }
}

/// `sector` with `word` at the next erased word, unchecked.
fn put(sector: &mut [u8; SECTOR], word: [u8; WORD]) {
    let at = sector.as_chunks::<WORD>().0.iter().position(|w| *w == [0xFF; WORD]).unwrap() * WORD;
    sector[at..at + WORD].copy_from_slice(&word);
}

fn next_offset(sector: &[u8; SECTOR], setting: Setting) -> usize {
    match write_for(sector, setting) {
        Some(Write::At { offset, .. }) => offset,
        other => panic!("not a write at a word: {other:?}"),
    }
}

#[test]
fn settings_read_nothing_when_sector_is_erased() {
    let sector = [0xFF; SECTOR];
    assert_eq!(settings::read(&sector), Stored::default());
}

#[test]
fn settings_write_at_offset_0_when_sector_is_erased() {
    let sector = [0xFF; SECTOR];
    let setting = Setting::Pool(ChannelPool::Eu);
    assert_eq!(write_for(&sector, setting), Some(Write::At { offset: 0, record: record(setting) }));
}

#[test]
fn settings_read_last_record_when_several_are_written() {
    let mut sector = [0xFF; SECTOR];
    for pct in [40, 75, 0] {
        store(&mut sector, fan(pct));
    }
    assert_eq!(settings::read(&sector).fan, Some(duty(0)));
    assert_eq!(next_offset(&sector, fan(10)), 3 * WORD);
}

#[test]
fn settings_read_each_tags_last_record_when_fan_and_pool_interleave() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, fan(40));
    store(&mut sector, Setting::Pool(ChannelPool::Us));
    store(&mut sector, fan(60));
    store(&mut sector, Setting::Pool(ChannelPool::Eu));
    store(&mut sector, fan(30));
    assert_eq!(
        settings::read(&sector),
        Stored { fan: Some(duty(30)), pool: Some(ChannelPool::Eu) }
    );
}

#[test]
fn settings_skip_word_when_it_is_torn_or_its_tag_is_unknown() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, fan(40));
    store(&mut sector, Setting::Pool(ChannelPool::Us));
    // A write a power cut stopped part-way: some bits cleared, the check not yet.
    put(&mut sector, [POOL, 0x01, 0xFF, 0xFF]);
    put(&mut sector, [b'X', 7, !7, 0]);
    assert_eq!(
        settings::read(&sector),
        Stored { fan: Some(duty(40)), pool: Some(ChannelPool::Us) }
    );

    // A record after them still reads.
    store(&mut sector, fan(60));
    assert_eq!(settings::read(&sector).fan, Some(duty(60)));
}

#[test]
fn settings_skip_record_when_value_is_unknown_or_check_byte_is_bad() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, fan(30));
    store(&mut sector, Setting::Pool(ChannelPool::Eu));
    put(&mut sector, [FAN, 101, !101, 0]);
    put(&mut sector, [FAN, 50, !51, 0]);
    put(&mut sector, [POOL, 3, !3, 0]);
    put(&mut sector, [POOL, 0, !0, 1]);
    assert_eq!(
        settings::read(&sector),
        Stored { fan: Some(duty(30)), pool: Some(ChannelPool::Eu) }
    );
}

#[test]
fn settings_write_nothing_when_value_is_already_stored() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, fan(40));
    store(&mut sector, Setting::Pool(ChannelPool::Us));
    assert_eq!(write_for(&sector, fan(40)), None);
    assert_eq!(write_for(&sector, Setting::Pool(ChannelPool::Us)), None);
}

#[test]
fn settings_write_at_last_word_when_one_is_left() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..RECORDS - 1 {
        store(&mut sector, fan(if i % 2 == 0 { 20 } else { 30 }));
    }
    assert_eq!(next_offset(&sector, fan(50)), (RECORDS - 1) * WORD);
}

#[test]
fn settings_erase_and_keep_fan_when_sector_is_full_and_pool_is_written() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..RECORDS {
        store(&mut sector, fan(u8::try_from(i % 101).expect("in range")));
    }
    let last = duty(u8::try_from((RECORDS - 1) % 101).expect("in range"));
    assert_eq!(settings::read(&sector).fan, Some(last));

    let pool = Setting::Pool(ChannelPool::Us);
    let Some(Write::Erase { records, len }) = write_for(&sector, pool) else {
        panic!("a full sector erases")
    };
    assert_eq!(len, 2 * WORD);
    assert_eq!(records[..WORD], record(Setting::Fan(last)));
    assert_eq!(records[WORD..2 * WORD], record(pool));

    store(&mut sector, pool);
    assert_eq!(settings::read(&sector), Stored { fan: Some(last), pool: Some(ChannelPool::Us) });
    assert_eq!(next_offset(&sector, fan(5)), 2 * WORD);
}

#[test]
fn settings_erase_and_write_one_record_when_full_sector_holds_only_that_setting() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..RECORDS {
        store(&mut sector, fan(if i % 2 == 0 { 20 } else { 30 }));
    }
    let Some(Write::Erase { records, len }) = write_for(&sector, fan(70)) else {
        panic!("a full sector erases")
    };
    assert_eq!(len, WORD);
    assert_eq!(records[..WORD], record(fan(70)));
}

#[test]
fn settings_record_pins_bytes_when_setting_is_a_pool() {
    assert_eq!(record(Setting::Pool(ChannelPool::Us)), [b'P', 0, 0xFF, 0]);
    assert_eq!(record(Setting::Pool(ChannelPool::Eu)), [b'P', 1, 0xFE, 0]);
    assert_eq!(record(Setting::Pool(ChannelPool::All)), [b'P', 2, 0xFD, 0]);
}

#[test]
fn settings_record_pins_bytes_when_setting_is_a_fan_duty() {
    // The layout a duty is already stored in.
    for pct in [0, 40, 100] {
        assert_eq!(record(fan(pct)), [b'F', pct, !pct, 0]);
    }
}

#[test]
fn settings_skip_record_when_tag_is_a_bluetooth_slot() {
    // A `B` record, a tag this build does not know, reads as an unknown tag and is skipped.
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, Setting::Pool(ChannelPool::Eu));
    put(&mut sector, [b'B', 9, !9, 0]);
    store(&mut sector, fan(40));
    assert_eq!(
        settings::read(&sector),
        Stored { fan: Some(duty(40)), pool: Some(ChannelPool::Eu) }
    );
}

#[test]
fn settings_erase_and_drop_bluetooth_record_when_sector_is_full() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, Setting::Pool(ChannelPool::Eu));
    put(&mut sector, [b'B', 9, !9, 0]);
    for i in 0..RECORDS - 2 {
        store(&mut sector, fan(if i % 2 == 0 { 20 } else { 30 }));
    }
    let Some(Write::Erase { len, .. }) = write_for(&sector, fan(70)) else {
        panic!("a full sector erases")
    };
    assert_eq!(len, 2 * WORD);
}
