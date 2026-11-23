//! The FAT32 pieces the bridge's diagnostics log is written in: FAT entries, 32-byte directory
//! entries, the log's file names and the first sector of its `LOG` directory.
//!
//! Each helper works on one 512-byte sector the caller holds, so the writer reads and writes the
//! card a block at a time and nothing here touches it.
//!
//! **A FAT32 entry is 28 bits.** The top 4 bits of each 32-bit slot are reserved, and a writer
//! keeps whatever they hold: [`entry`] masks them off and [`set_entry`] leaves them alone.
//!
//! **Every timestamp is 1980-01-01 00:00**, FAT's earliest. The P4 has no clock that knows the
//! date, and a made-up one would read as a real time on a laptop.
//!
//! **The log's files are `LOG/NNNNNNNN.BIN`**: eight decimal digits, so each is a short name
//! with no long-name entries to write, and every reader lists them in order.

use super::BLOCK;

/// The FAT entries in one sector.
pub const ENTRIES_PER_SECTOR: u32 = (BLOCK / 4) as u32;

/// The bits of a slot that are the entry.
const ENTRY_MASK: u32 = 0x0FFF_FFFF;

/// The end-of-chain marker a writer puts in a chain's last entry.
pub const EOC: u32 = 0x0FFF_FFFF;

/// The marker of a cluster the media reports bad.
pub const BAD: u32 = 0x0FFF_FFF7;

/// The lowest entry that ends a chain: readers take any value from here to [`EOC`] as its end.
const END_MIN: u32 = 0x0FFF_FFF8;

/// The FAT sector, counted from the FAT's start, that holds `cluster`'s entry.
#[must_use]
pub const fn sector_of(cluster: u32) -> u32 {
    cluster / ENTRIES_PER_SECTOR
}

/// Where in its FAT sector `cluster`'s entry is, counted in entries.
#[must_use]
pub const fn slot_of(cluster: u32) -> usize {
    (cluster % ENTRIES_PER_SECTOR) as usize
}

/// The byte range of `cluster`'s entry in its FAT sector.
const fn entry_at(cluster: u32) -> usize {
    slot_of(cluster) * 4
}

/// `cluster`'s entry in `sector`, the FAT sector [`sector_of`] names, its reserved bits masked
/// off.
#[must_use]
pub fn entry(sector: &[u8; BLOCK], cluster: u32) -> u32 {
    let at = entry_at(cluster);
    let bytes = [sector[at], sector[at + 1], sector[at + 2], sector[at + 3]];
    u32::from_le_bytes(bytes) & ENTRY_MASK
}

/// Set `cluster`'s entry in `sector` to `value`'s low 28 bits, keeping the slot's top 4 bits.
pub fn set_entry(sector: &mut [u8; BLOCK], cluster: u32, value: u32) {
    let at = entry_at(cluster);
    let slot = &mut sector[at..at + 4];
    let old = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
    let new = (old & !ENTRY_MASK) | (value & ENTRY_MASK);
    slot.copy_from_slice(&new.to_le_bytes());
}

/// Whether `entry` marks its cluster free.
#[must_use]
pub const fn is_free(entry: u32) -> bool {
    entry == 0
}

/// Whether `entry` links to a next cluster in the data area of a volume of `clusters`.
#[must_use]
pub const fn is_link(entry: u32, clusters: u32) -> bool {
    entry >= 2 && (entry - 2) < clusters
}

/// Whether `entry` ends its chain.
#[must_use]
pub const fn is_end(entry: u32) -> bool {
    entry >= END_MIN
}

/// Whether `entry` marks its cluster bad.
#[must_use]
pub const fn is_bad(entry: u32) -> bool {
    entry == BAD
}

/// The bytes in one directory entry.
pub const DIR_ENTRY: usize = 32;

/// The directory entries in one sector.
pub const DIR_ENTRIES_PER_SECTOR: usize = BLOCK / DIR_ENTRY;

/// A directory entry's attribute bits.
pub mod attr {
    /// The volume label, an entry of the root directory that is no file.
    pub const VOLUME: u8 = 0x08;
    /// A subdirectory.
    pub const DIR: u8 = 0x10;
    /// A file changed since it was last backed up, which every new file is.
    pub const ARCHIVE: u8 = 0x20;
    /// A long-name entry: read-only, hidden, system and volume label at once.
    pub const LFN: u8 = 0x0F;
    /// The bits a long-name entry is compared on.
    pub const LFN_MASK: u8 = 0x3F;
}

/// A name's first byte when its entry is deleted and free to reuse.
pub const DELETED: u8 = 0xE5;

/// A name's first byte when its entry, and every one after it, has never been used.
pub const END: u8 = 0x00;

/// FAT's date for 1980-01-01: day 1, month 1, year 0.
pub const DATE_1980: u16 = 0x0021;

/// FAT's time for 00:00:00.
pub const TIME_ZERO: u16 = 0;

/// A short-name directory entry: the fields the log reads and writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntry {
    /// The 8.3 name, padded with spaces, without its dot.
    pub name: [u8; 11],
    /// The [`attr`] bits.
    pub attr: u8,
    /// The first cluster, or 0 for an empty file or a `..` naming the root.
    pub cluster: u32,
    /// The size in bytes; 0 for a directory.
    pub size: u32,
}

/// Bytes of a directory entry, by offset.
mod dir {
    pub const ATTR: usize = 11;
    pub const CREATE_TIME: usize = 14;
    pub const CREATE_DATE: usize = 16;
    pub const ACCESS_DATE: usize = 18;
    pub const CLUSTER_HI: usize = 20;
    pub const WRITE_TIME: usize = 22;
    pub const WRITE_DATE: usize = 24;
    pub const CLUSTER_LO: usize = 26;
    pub const SIZE: usize = 28;
}

impl DirEntry {
    /// An archive file `name` of `size` bytes from `cluster`.
    #[must_use]
    pub const fn file(name: [u8; 11], cluster: u32, size: u32) -> Self {
        Self { name, attr: attr::ARCHIVE, cluster, size }
    }

    /// A subdirectory `name` from `cluster`.
    #[must_use]
    pub const fn dir(name: [u8; 11], cluster: u32) -> Self {
        Self { name, attr: attr::DIR, cluster, size: 0 }
    }

    /// The entry's 32 bytes, every timestamp 1980-01-01 00:00.
    #[must_use]
    pub fn encode(&self) -> [u8; DIR_ENTRY] {
        let mut out = [0; DIR_ENTRY];
        out[..11].copy_from_slice(&self.name);
        out[dir::ATTR] = self.attr;
        let [lo0, lo1, hi0, hi1] = self.cluster.to_le_bytes();
        let fields: [(usize, [u8; 2]); 7] = [
            (dir::CREATE_TIME, TIME_ZERO.to_le_bytes()),
            (dir::CREATE_DATE, DATE_1980.to_le_bytes()),
            (dir::ACCESS_DATE, DATE_1980.to_le_bytes()),
            (dir::CLUSTER_HI, [hi0, hi1]),
            (dir::WRITE_TIME, TIME_ZERO.to_le_bytes()),
            (dir::WRITE_DATE, DATE_1980.to_le_bytes()),
            (dir::CLUSTER_LO, [lo0, lo1]),
        ];
        for (at, bytes) in fields {
            out[at..at + 2].copy_from_slice(&bytes);
        }
        out[dir::SIZE..].copy_from_slice(&self.size.to_le_bytes());
        out
    }

    /// The entry in `bytes`. Its cluster loses the high word's reserved top 4 bits.
    #[must_use]
    pub fn decode(bytes: &[u8; DIR_ENTRY]) -> Self {
        let mut name = [0; 11];
        name.copy_from_slice(&bytes[..11]);
        let at = |i: usize| [bytes[i], bytes[i + 1]];
        let [hi0, hi1] = at(dir::CLUSTER_HI);
        let [lo0, lo1] = at(dir::CLUSTER_LO);
        let size = &bytes[dir::SIZE..];
        Self {
            name,
            attr: bytes[dir::ATTR],
            cluster: u32::from_le_bytes([lo0, lo1, hi0, hi1]) & ENTRY_MASK,
            size: u32::from_le_bytes([size[0], size[1], size[2], size[3]]),
        }
    }

    /// Whether this entry, and every one after it in its directory, has never been used.
    #[must_use]
    pub const fn is_end(&self) -> bool {
        self.name[0] == END
    }

    /// Whether this entry is deleted and free to reuse.
    #[must_use]
    pub const fn is_deleted(&self) -> bool {
        self.name[0] == DELETED
    }

    /// Whether this is a piece of a long name rather than a file or directory.
    #[must_use]
    pub const fn is_long_name(&self) -> bool {
        self.attr & attr::LFN_MASK == attr::LFN
    }
}

/// Where a long-name entry keeps the checksum of the short name it belongs to.
pub const LFN_CHECKSUM: usize = 13;

/// The checksum a long-name entry carries of its short name, `name`.
#[must_use]
pub fn lfn_checksum(name: &[u8; 11]) -> u8 {
    name.iter().fold(0u8, |sum, &byte| sum.rotate_right(1).wrapping_add(byte))
}

/// Directory entry `index` of `sector`, or `None` past its [`DIR_ENTRIES_PER_SECTOR`].
#[must_use]
pub fn dir_entry(sector: &[u8; BLOCK], index: usize) -> Option<DirEntry> {
    let at = index.checked_mul(DIR_ENTRY)?;
    let bytes: &[u8; DIR_ENTRY] = sector.get(at..at + DIR_ENTRY)?.try_into().ok()?;
    Some(DirEntry::decode(bytes))
}

/// Write `entry` as directory entry `index` of `sector`. Returns `false`, writing nothing, past
/// its [`DIR_ENTRIES_PER_SECTOR`].
pub fn put_dir_entry(sector: &mut [u8; BLOCK], index: usize, entry: &DirEntry) -> bool {
    let Some(at) = index.checked_mul(DIR_ENTRY) else { return false };
    let Some(slot) = sector.get_mut(at..at + DIR_ENTRY) else { return false };
    slot.copy_from_slice(&entry.encode());
    true
}

/// The log directory's short name, `LOG`.
pub const LOG_DIR: [u8; 11] = *b"LOG        ";

/// The extension of every log file.
const LOG_EXT: [u8; 3] = *b"BIN";

/// The highest number a log file's eight digits hold.
pub const LOG_MAX: u32 = 99_999_999;

/// Log file `n`'s short name: `*b"00000123BIN"` for 123. `None` past [`LOG_MAX`].
#[must_use]
pub fn log_name(n: u32) -> Option<[u8; 11]> {
    if n > LOG_MAX {
        return None;
    }
    let mut name = [0; 11];
    let mut rest = n;
    for digit in name[..8].iter_mut().rev() {
        // A remainder of 10 is one digit.
        *digit = b'0' + (rest % 10) as u8;
        rest /= 10;
    }
    name[8..].copy_from_slice(&LOG_EXT);
    Some(name)
}

/// The number of the log file named `name`, or `None` when it is not one of the log's names:
/// exactly eight ASCII digits, then `BIN`.
#[must_use]
pub fn parse_log_name(name: &[u8; 11]) -> Option<u32> {
    if name[8..] != LOG_EXT {
        return None;
    }
    name[..8].iter().try_fold(0u32, |n, &byte| {
        // Eight digits are at most `LOG_MAX`, so the sum never overflows.
        byte.is_ascii_digit().then(|| n * 10 + u32::from(byte - b'0'))
    })
}

/// The `.` entry's name.
const DOT: [u8; 11] = *b".          ";

/// The `..` entry's name.
const DOT_DOT: [u8; 11] = *b"..         ";

/// The first sector of a directory in the root that starts at `self_cluster`: `.` naming itself,
/// `..` naming the root as cluster 0, as FAT requires, and the end marker. The cluster's other
/// sectors are zeros.
#[must_use]
pub fn dot_sector(self_cluster: u32) -> [u8; BLOCK] {
    let mut sector = [0; BLOCK];
    sector[..DIR_ENTRY].copy_from_slice(&DirEntry::dir(DOT, self_cluster).encode());
    sector[DIR_ENTRY..2 * DIR_ENTRY].copy_from_slice(&DirEntry::dir(DOT_DOT, 0).encode());
    sector
}
