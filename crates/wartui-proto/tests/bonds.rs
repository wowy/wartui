//! The bond sector: its records, how it reads, and the write each change takes.

use wartui_proto::bonds::{
    self, ADD, BondAddr, BondLevel, BondRecord, Bonds, MAX_BONDS, REMOVE, Record, SLOT, SLOTS,
    Write, apply, record, write_for,
};
use wartui_proto::ota::SECTOR;

/// A random static address ending in `n`, in HCI order.
fn addr(n: u8) -> BondAddr {
    BondAddr { kind: 1, addr: [n, 0x11, 0x22, 0x33, 0x44, 0xC2] }
}

fn bond(n: u8) -> BondRecord {
    BondRecord {
        addr: addr(n),
        irk: Some(0x0102_0304_0506_0708_090A_0B0C_0D0E_0F10 + u128::from(n)),
        ltk: 0xF0E0_D0C0_B0A0_9080_7060_5040_3020_1000 + u128::from(n),
        level: BondLevel::Encrypted,
    }
}

/// `sector` with `change` stored as the P4 stores it. Panics when the change stores nothing.
fn store(sector: &mut [u8; SECTOR], change: &Record) {
    match write_for(sector, change).expect("the change stores something") {
        Write::At { offset, record } => {
            for (old, new) in sector[offset..offset + SLOT].iter().zip(&record) {
                // NOR flash programs only by clearing bits: no write may set one.
                assert_eq!(new & !old, 0, "the record sets a bit at {offset}");
            }
            sector[offset..offset + SLOT].copy_from_slice(&record);
        }
        Write::Erase { records, len } => {
            sector.fill(0xFF);
            sector[..len].copy_from_slice(&records[..len]);
        }
    }
}

/// `sector` with `slot` at the next erased slot, unchecked.
fn put(sector: &mut [u8; SECTOR], slot: [u8; SLOT]) {
    let at = sector.as_chunks::<SLOT>().0.iter().position(|s| *s == [0xFF; SLOT]).unwrap() * SLOT;
    sector[at..at + SLOT].copy_from_slice(&slot);
}

fn list(ns: &[u8]) -> Bonds {
    Bonds::from_iter(ns.iter().map(|&n| bond(n)))
}

#[test]
fn bonds_read_nothing_when_sector_is_erased() {
    let sector = [0xFF; SECTOR];
    assert_eq!(bonds::read(&sector), Bonds::new());
    assert!(!bonds::is_written(&sector));
}

#[test]
fn bonds_write_at_offset_0_when_sector_is_erased() {
    let sector = [0xFF; SECTOR];
    let change = Record::Add(bond(1));
    assert_eq!(write_for(&sector, &change), Some(Write::At { offset: 0, record: record(&change) }));
}

#[test]
fn bonds_read_back_each_field_when_bond_is_stored() {
    let mut sector = [0xFF; SECTOR];
    let without_irk = BondRecord { irk: None, level: BondLevel::Authenticated, ..bond(2) };
    store(&mut sector, &Record::Add(bond(1)));
    store(&mut sector, &Record::Add(without_irk));
    assert_eq!(bonds::read(&sector), Bonds::from_iter([bond(1), without_irk]));
    assert!(bonds::is_written(&sector));
}

#[test]
fn bonds_keep_last_record_when_address_is_paired_again() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, &Record::Add(bond(1)));
    store(&mut sector, &Record::Add(bond(2)));
    let again = BondRecord { ltk: 7, ..bond(1) };
    store(&mut sector, &Record::Add(again));
    // Paired again, so it is the newest.
    assert_eq!(bonds::read(&sector), Bonds::from_iter([bond(2), again]));
}

#[test]
fn bonds_drop_bond_when_its_address_is_removed() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, &Record::Add(bond(1)));
    store(&mut sector, &Record::Add(bond(2)));
    store(&mut sector, &Record::Remove(addr(1)));
    assert_eq!(bonds::read(&sector), list(&[2]));
}

#[test]
fn bonds_evict_oldest_when_one_past_max_is_added() {
    let mut sector = [0xFF; SECTOR];
    for n in 1..=5 {
        store(&mut sector, &Record::Add(bond(n)));
    }
    assert_eq!(MAX_BONDS, 4);
    assert_eq!(bonds::read(&sector), list(&[2, 3, 4, 5]));
}

#[test]
fn bonds_evict_least_recently_paired_when_full() {
    let mut sector = [0xFF; SECTOR];
    for n in 1..=4 {
        store(&mut sector, &Record::Add(bond(n)));
    }
    // Bond 1 pairs again, so bond 2 is now the oldest.
    store(&mut sector, &Record::Add(BondRecord { ltk: 9, ..bond(1) }));
    store(&mut sector, &Record::Add(bond(5)));
    assert_eq!(
        bonds::read(&sector),
        Bonds::from_iter([bond(3), bond(4), BondRecord { ltk: 9, ..bond(1) }, bond(5)])
    );
}

#[test]
fn bonds_skip_slot_when_it_is_torn_or_its_tag_is_unknown() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, &Record::Add(bond(1)));
    // A write a power cut stopped part-way: the CRC not yet written.
    let mut torn = record(&Record::Add(bond(2)));
    torn[SLOT - 4..].fill(0xFF);
    put(&mut sector, torn);
    // A tag this build does not know, under a good CRC.
    let mut unknown = record(&Record::Remove(addr(1)));
    unknown[0] = b'X';
    let crc = wartui_proto::spike::crc32(&unknown[..SLOT - 4]);
    unknown[SLOT - 4..].copy_from_slice(&crc.to_le_bytes());
    put(&mut sector, unknown);
    assert_eq!(bonds::read(&sector), list(&[1]));

    // A record after them still reads.
    store(&mut sector, &Record::Add(bond(3)));
    assert_eq!(bonds::read(&sector), list(&[1, 3]));
}

#[test]
fn bonds_skip_record_when_level_or_irk_flag_is_unknown() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, &Record::Add(bond(1)));
    for (at, value) in [(8, 3), (9, 2)] {
        let mut bad = record(&Record::Add(bond(2)));
        bad[at] = value;
        let crc = wartui_proto::spike::crc32(&bad[..SLOT - 4]);
        bad[SLOT - 4..].copy_from_slice(&crc.to_le_bytes());
        put(&mut sector, bad);
    }
    assert_eq!(bonds::read(&sector), list(&[1]));
}

#[test]
fn bonds_write_nothing_when_change_changes_nothing() {
    let mut sector = [0xFF; SECTOR];
    store(&mut sector, &Record::Add(bond(1)));
    store(&mut sector, &Record::Add(bond(2)));
    assert_eq!(write_for(&sector, &Record::Add(bond(2))), None);
    assert_eq!(write_for(&sector, &Record::Remove(addr(9))), None);
}

#[test]
fn bonds_write_at_last_slot_when_one_is_left() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..SLOTS - 1 {
        store(&mut sector, &Record::Add(bond(if i % 2 == 0 { 1 } else { 2 })));
    }
    match write_for(&sector, &Record::Add(bond(3))) {
        Some(Write::At { offset, .. }) => assert_eq!(offset, (SLOTS - 1) * SLOT),
        other => panic!("not a write at a slot: {other:?}"),
    }
}

#[test]
fn bonds_erase_and_rewrite_kept_bonds_when_sector_is_full() {
    let mut sector = [0xFF; SECTOR];
    for i in 0..SLOTS {
        store(&mut sector, &Record::Add(bond(1 + u8::try_from(i % 3).unwrap())));
    }
    let before = bonds::read(&sector);
    let Some(Write::Erase { records, len }) = write_for(&sector, &Record::Add(bond(7))) else {
        panic!("a full sector erases")
    };
    let kept = apply(&before, &Record::Add(bond(7)));
    assert_eq!(len, kept.len() * SLOT);
    for (at, kept) in kept.iter().enumerate() {
        assert_eq!(records[at * SLOT..(at + 1) * SLOT], record(&Record::Add(*kept)));
    }

    store(&mut sector, &Record::Add(bond(7)));
    assert_eq!(bonds::read(&sector), kept);
    // The rewrite leaves the rest of the sector erased, so the next change is a slot write.
    match write_for(&sector, &Record::Remove(addr(7))) {
        Some(Write::At { offset, .. }) => assert_eq!(offset, kept.len() * SLOT),
        other => panic!("not a write at a slot: {other:?}"),
    }
}

#[test]
fn bonds_record_lays_out_fields_when_encoded() {
    let add = record(&Record::Add(bond(0x5A)));
    assert_eq!(add[0], ADD);
    assert_eq!(add[1], 1, "address type");
    assert_eq!(add[2..8], addr(0x5A).addr);
    assert_eq!((add[8], add[9], add[10], add[11]), (1, 1, 0, 0), "level, IRK flag, reserved");
    assert_eq!(add[12..28], bond(0x5A).ltk.to_le_bytes());
    assert_eq!(add[28..44], bond(0x5A).irk.unwrap().to_le_bytes());
    let crc = wartui_proto::spike::crc32(&add[..44]);
    assert_eq!(add[44..], crc.to_le_bytes());

    let remove = record(&Record::Remove(addr(0x5A)));
    assert_eq!(remove[0], REMOVE);
    assert_eq!(remove[2..8], addr(0x5A).addr);
    assert!(remove[8..44].iter().all(|&b| b == 0));
}

#[test]
fn bond_addr_reverses_octets_when_shown() {
    assert_eq!(addr(0x5A).mac(), [0xC2, 0x44, 0x33, 0x22, 0x11, 0x5A]);
}
