//! The FAT32 pieces the SD log is written in, and a check that `fatfs`, written by someone else,
//! reads a directory built from them.

use std::io::{Cursor, Read, Seek, SeekFrom, Write};

use wartui_proto::sd::BLOCK;
use wartui_proto::sd::fat::{
    BAD, DATE_1980, DELETED, DIR_ENTRIES_PER_SECTOR, DirEntry, END, ENTRIES_PER_SECTOR, EOC,
    LOG_DIR, LOG_MAX, attr, dir_entry, dot_sector, entry, is_bad, is_end, is_free, is_link,
    log_name, parse_log_name, put_dir_entry, sector_of, set_entry, slot_of,
};
use wartui_proto::sd::inspect::volume;

#[test]
fn fat_sector_and_slot_split_cluster_when_counted() {
    assert_eq!(ENTRIES_PER_SECTOR, 128);
    for (cluster, sector, slot) in [(0, 0, 0), (127, 0, 127), (128, 1, 0), (1000, 7, 104)] {
        assert_eq!((sector_of(cluster), slot_of(cluster)), (sector, slot), "cluster {cluster}");
    }
    assert_eq!(sector_of(u32::MAX), u32::MAX / 128);
    assert_eq!(slot_of(u32::MAX), 127);
}

#[test]
fn fat_entry_masks_top_bits_when_read() {
    let mut sector = [0; BLOCK];
    sector[4 * 5..4 * 6].copy_from_slice(&0xF123_4567u32.to_le_bytes());
    assert_eq!(entry(&sector, 5), 0x0123_4567);
    assert_eq!(entry(&sector, 128 + 5), 0x0123_4567, "the slot repeats in every sector");
}

#[test]
fn fat_set_entry_keeps_top_bits_when_written() {
    let mut sector = [0; BLOCK];
    sector[4 * 9..4 * 10].copy_from_slice(&0xA000_0000u32.to_le_bytes());
    set_entry(&mut sector, 9, 0xFFFF_FFFF);
    assert_eq!(sector[4 * 9..4 * 10], 0xAFFF_FFFFu32.to_le_bytes());
    set_entry(&mut sector, 9, 0);
    assert_eq!(sector[4 * 9..4 * 10], 0xA000_0000u32.to_le_bytes());
}

#[test]
fn fat_entry_round_trips_when_set_on_each_slot() {
    let mut sector = [0; BLOCK];
    for cluster in 0..128 {
        set_entry(&mut sector, cluster, 0x0ABC_0000 + cluster);
    }
    for cluster in 0..128 {
        assert_eq!(entry(&sector, cluster), 0x0ABC_0000 + cluster, "cluster {cluster}");
    }
}

#[test]
fn fat_predicates_classify_entry_when_marker_or_link() {
    let clusters = 100;
    assert!(is_free(0));
    assert!(!is_free(2));
    for link in [2, 50, clusters + 1] {
        assert!(is_link(link, clusters) && !is_end(link), "{link}");
    }
    for not_link in [0, 1, clusters + 2, BAD, EOC] {
        assert!(!is_link(not_link, clusters), "{not_link:#X}");
    }
    for end in [0x0FFF_FFF8, 0x0FFF_FFFC, EOC] {
        assert!(is_end(end) && !is_bad(end), "{end:#X}");
    }
    assert!(is_bad(BAD) && !is_end(BAD));
    // The most clusters FAT32 holds leaves the markers outside the data area.
    assert!(!is_link(BAD, 0x0FFF_FFF5));
}

#[test]
fn fat_dir_entry_pins_bytes_when_encoded() {
    let file = DirEntry::file(*b"00000123BIN", 0x0012_3456, 0x0102_0304);
    #[rustfmt::skip]
    let want: [u8; 32] = [
        b'0', b'0', b'0', b'0', b'0', b'1', b'2', b'3', b'B', b'I', b'N',
        0x20,                   // archive
        0x00, 0x00,             // reserved, creation tenths
        0x00, 0x00, 0x21, 0x00, // created 00:00 on 1980-01-01
        0x21, 0x00,             // accessed 1980-01-01
        0x12, 0x00,             // cluster, high word
        0x00, 0x00, 0x21, 0x00, // written 00:00 on 1980-01-01
        0x56, 0x34,             // cluster, low word
        0x04, 0x03, 0x02, 0x01, // size
    ];
    assert_eq!(file.encode(), want);
    assert_eq!(DATE_1980, 0x0021);
}

#[test]
fn fat_dir_entry_round_trips_when_decoded() {
    for entry in [
        DirEntry::file(*b"00000001BIN", 3, 0),
        DirEntry::file(*b"README  TXT", 0x0FFF_FFEF, u32::MAX),
        DirEntry::dir(LOG_DIR, 0x0100_0000),
    ] {
        assert_eq!(DirEntry::decode(&entry.encode()), entry);
    }
}

#[test]
fn fat_dir_entry_masks_cluster_top_bits_when_decoded() {
    let mut bytes = DirEntry::file(*b"00000001BIN", 7, 1).encode();
    bytes[21] |= 0xF0;
    assert_eq!(DirEntry::decode(&bytes).cluster, 7);
}

#[test]
fn fat_dir_entry_reads_markers_when_name_or_attr_says() {
    let mut entry = DirEntry::file(*b"00000001BIN", 3, 0);
    assert!(!entry.is_end() && !entry.is_deleted() && !entry.is_long_name());
    entry.name[0] = DELETED;
    assert!(entry.is_deleted());
    entry.name[0] = END;
    assert!(entry.is_end());
    // A long name's attribute, with the archive bit a few writers add.
    entry.attr = attr::LFN | 0x40;
    assert!(entry.is_long_name());
    entry.attr = attr::DIR;
    assert!(!entry.is_long_name());
}

#[test]
fn fat_dir_slot_refuses_index_when_past_sector() {
    let mut sector = [0; BLOCK];
    let entry = DirEntry::dir(LOG_DIR, 5);
    let last = DIR_ENTRIES_PER_SECTOR - 1;
    assert!(put_dir_entry(&mut sector, last, &entry));
    assert_eq!(dir_entry(&sector, last), Some(entry));
    assert_eq!(sector[BLOCK - 32..BLOCK - 21], LOG_DIR);
    for past in [DIR_ENTRIES_PER_SECTOR, usize::MAX] {
        assert!(!put_dir_entry(&mut sector, past, &entry), "{past}");
        assert_eq!(dir_entry(&sector, past), None, "{past}");
    }
}

#[test]
fn fat_log_name_pins_digits_when_numbered() {
    assert_eq!(log_name(0), Some(*b"00000000BIN"));
    assert_eq!(log_name(123), Some(*b"00000123BIN"));
    assert_eq!(log_name(LOG_MAX), Some(*b"99999999BIN"));
    assert_eq!(log_name(LOG_MAX + 1), None);
    assert_eq!(log_name(u32::MAX), None);
}

#[test]
fn fat_parse_log_name_round_trips_when_name_is_ours() {
    for n in [0, 1, 123, 4_567_890, LOG_MAX] {
        assert_eq!(parse_log_name(&log_name(n).expect("eight digits")), Some(n));
    }
}

#[test]
fn fat_parse_log_name_refuses_name_when_foreign() {
    for name in [
        LOG_DIR,
        *b"0000012 BIN",
        *b" 0000012BIN",
        *b"0000012ABIN",
        *b"-0000012BIN",
        *b"00000123TXT",
        *b"00000123bin",
        *b"00000123BI ",
        *b"00000123   ",
        [0xE5, b'0', b'0', b'0', b'0', b'1', b'2', b'3', b'B', b'I', b'N'],
    ] {
        assert_eq!(parse_log_name(&name), None, "{:?}", String::from_utf8_lossy(&name));
    }
}

#[test]
fn fat_dot_sector_pins_entries_when_built() {
    let sector = dot_sector(0x0001_0005);
    let dot = dir_entry(&sector, 0).expect("an entry");
    assert_eq!(dot, DirEntry::dir(*b".          ", 0x0001_0005));
    let dot_dot = dir_entry(&sector, 1).expect("an entry");
    assert_eq!(dot_dot, DirEntry::dir(*b"..         ", 0));
    assert!(sector[64..].iter().all(|&byte| byte == 0), "then the end marker");
}

/// Block `lba` of `image`.
fn block(image: &mut Cursor<Vec<u8>>, lba: u32) -> [u8; BLOCK] {
    let mut out = [0; BLOCK];
    image.seek(SeekFrom::Start(u64::from(lba) * BLOCK as u64)).expect("seeks");
    image.read_exact(&mut out).expect("reads");
    out
}

fn put(image: &mut Cursor<Vec<u8>>, lba: u32, data: &[u8; BLOCK]) {
    image.seek(SeekFrom::Start(u64::from(lba) * BLOCK as u64)).expect("seeks");
    image.write_all(data).expect("writes");
}

#[test]
fn fat_fatfs_reads_log_dir_when_built_from_helpers() {
    // A FAT32 volume of 512-byte clusters with no partition table, the smallest `fatfs` makes.
    let blocks: u32 = 70_000;
    let mut image = Cursor::new(vec![0u8; blocks as usize * BLOCK]);
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat32)
        .bytes_per_cluster(512)
        .total_sectors(blocks);
    fatfs::format_volume(&mut image, options).expect("formats");
    image.set_position(0);

    let found = volume(&block(&mut image, 0), 0).expect("a FAT32 volume");
    assert_eq!(found.sectors_per_cluster, 1);
    let (dir, file) = (found.root_cluster + 1, found.root_cluster + 2);

    // Both clusters a chain of one, in both FATs.
    for copy in 0..u32::from(found.fats) {
        let lba = found.fat_start + copy * found.fat_blocks;
        let mut fat = block(&mut image, lba + sector_of(dir));
        assert!(is_free(entry(&fat, dir)) && is_free(entry(&fat, file)), "free to take");
        set_entry(&mut fat, dir, EOC);
        set_entry(&mut fat, file, EOC);
        put(&mut image, lba + sector_of(dir), &fat);
    }

    let text = b"one record\0";
    let mut data = [0; BLOCK];
    data[..text.len()].copy_from_slice(text);
    put(&mut image, found.cluster_lba(file).expect("a cluster"), &data);

    let mut log = dot_sector(dir);
    let name = log_name(123).expect("eight digits");
    let size = u32::try_from(text.len()).expect("small");
    assert!(put_dir_entry(&mut log, 2, &DirEntry::file(name, file, size)));
    put(&mut image, found.cluster_lba(dir).expect("a cluster"), &log);

    let root_lba = found.cluster_lba(found.root_cluster).expect("a cluster");
    let mut root = block(&mut image, root_lba);
    let free = (0..DIR_ENTRIES_PER_SECTOR)
        .find(|&i| dir_entry(&root, i).is_some_and(|e| e.is_end()))
        .expect("room in the root");
    assert!(put_dir_entry(&mut root, free, &DirEntry::dir(LOG_DIR, dir)));
    put(&mut image, root_lba, &root);

    image.set_position(0);
    let fs = fatfs::FileSystem::new(&mut image, fatfs::FsOptions::new()).expect("mounts");
    let names: Vec<String> = fs
        .root_dir()
        .open_dir("LOG")
        .expect("opens")
        .iter()
        .map(|e| e.expect("reads").file_name())
        .collect();
    assert_eq!(names, [".", "..", "00000123.BIN"]);
    let mut read = Vec::new();
    fs.root_dir()
        .open_file("LOG/00000123.BIN")
        .expect("opens")
        .read_to_end(&mut read)
        .expect("reads");
    assert_eq!(read, text);
}
