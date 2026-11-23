//! The P4 bridge's microSD card: the FAT32 volume it writes ([`format`]), how it reads one it
//! did not ([`inspect`]), the FAT32 pieces its diagnostics log is written in ([`fat`]) and the
//! log's records ([`log`]).
//!
//! The layout is the one SD cards ship with, so a card the bridge formats looks like a new card
//! to a laptop or a phone:
//!
//! | LBA                | What                                                 |
//! | ------------------ | ---------------------------------------------------- |
//! | 0                  | MBR: one partition, type `0x0C` (FAT32, LBA)         |
//! | 1 to 8191          | zeros                                                |
//! | 8192               | the volume: boot sector, FSInfo, copies at 6 and 7   |
//! | 8192 + reserved    | two FATs                                             |
//! | a multiple of 8192 | the data area, its first cluster the root directory  |
//!
//! A card's flash is managed in allocation units of up to 4 MiB, so the partition starts at
//! 4 MiB and the data area on a 4 MiB boundary: no cluster straddles two units. Clusters are
//! 32 KiB, as SDHC and SDXC cards ship, once the volume then holds the 65,525 clusters FAT32
//! needs; a small card takes the largest cluster that still reaches that count.
//!
//! A format writes the MBR last. A card whose format was cut short has a zeroed block 0, which
//! no reader takes for a volume, rather than a partition table pointing at a half-written one.
//!
//! [`inspect`] reads whatever a stranger's card holds, so it bounds every read and every sum:
//! nothing in a block can make it panic or overflow.

pub mod fat;
pub mod format;
pub mod inspect;
pub mod log;

/// The bytes in one card block. Every SD card addresses 512-byte blocks.
pub const BLOCK: usize = 512;

/// Where the partition starts, and the alignment of its data area: 4 MiB in blocks.
const ALIGN: u32 = 8192;

/// The fewest clusters a FAT32 volume holds. Below it a reader takes the volume for FAT16.
const MIN_CLUSTERS: u32 = 65_525;

/// The most clusters a FAT32 volume holds: entries at or past `0x0FFFFFF7` are markers.
const MAX_CLUSTERS: u32 = 0x0FFF_FFF5;

/// The partition types that name a FAT32 volume: CHS and LBA addressed.
const FAT32_TYPES: [u8; 2] = [0x0B, 0x0C];

/// Where the MBR's first partition entry starts.
const MBR_ENTRY: usize = 446;

/// The FSInfo sector's three signatures, at 0, 484 and 508.
const FSINFO_LEAD: u32 = 0x4161_5252;
const FSINFO_STRUCT: u32 = 0x6141_7272;
const FSINFO_TRAIL: u32 = 0xAA55_0000;

/// The FSInfo value for an unknown free count.
const FREE_UNKNOWN: u32 = 0xFFFF_FFFF;

/// Bytes of a boot sector, by offset.
mod bpb {
    pub const BYTES_PER_SECTOR: usize = 11;
    pub const SECTORS_PER_CLUSTER: usize = 13;
    pub const RESERVED: usize = 14;
    pub const FATS: usize = 16;
    pub const ROOT_ENTRIES: usize = 17;
    pub const TOTAL_16: usize = 19;
    pub const MEDIA: usize = 21;
    pub const FAT_16: usize = 22;
    pub const SECTORS_PER_TRACK: usize = 24;
    pub const HEADS: usize = 26;
    pub const HIDDEN: usize = 28;
    pub const TOTAL_32: usize = 32;
    pub const FAT_32: usize = 36;
    pub const EXT_FLAGS: usize = 40;
    pub const FS_VERSION: usize = 42;
    pub const ROOT_CLUSTER: usize = 44;
    pub const FSINFO: usize = 48;
    pub const BACKUP: usize = 50;
    pub const DRIVE: usize = 64;
    pub const SIGNATURE: usize = 66;
    pub const VOLUME_ID: usize = 67;
    pub const LABEL: usize = 71;
    pub const TYPE: usize = 82;
}

/// Bytes of an FSInfo sector, by offset.
mod fsinfo {
    pub const LEAD: usize = 0;
    pub const STRUCT: usize = 484;
    pub const FREE: usize = 488;
    pub const NEXT_FREE: usize = 492;
    pub const TRAIL: usize = 508;
}

/// Whether a block ends in the `55 AA` boot signature.
fn signed(block: &[u8; BLOCK]) -> bool {
    block[BLOCK - 2..] == [0x55, 0xAA]
}

/// The `u16` at `at`, little-endian, or 0 past the block's end.
fn le16(block: &[u8; BLOCK], at: usize) -> u16 {
    let mut bytes = [0; 2];
    if let Some(src) = block.get(at..at.saturating_add(2)) {
        bytes.copy_from_slice(src);
    }
    u16::from_le_bytes(bytes)
}

/// The `u32` at `at`, little-endian, or 0 past the block's end.
fn le32(block: &[u8; BLOCK], at: usize) -> u32 {
    let mut bytes = [0; 4];
    if let Some(src) = block.get(at..at.saturating_add(4)) {
        bytes.copy_from_slice(src);
    }
    u32::from_le_bytes(bytes)
}

/// `blocks` of [`BLOCK`] bytes in MiB, rounded down, and `u32::MAX` past it.
#[must_use]
pub fn mib(blocks: u64) -> u32 {
    u32::try_from(blocks / 2048).unwrap_or(u32::MAX)
}
