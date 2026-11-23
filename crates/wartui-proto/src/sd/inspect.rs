//! Reading a card the bridge did not format, from up to three blocks: block 0 ([`partition`]),
//! the volume's boot sector ([`volume`]) and its FSInfo sector, when it has one
//! ([`Volume::free_mb`]).
//!
//! Each block is a stranger's bytes. Every field is checked before it is used, sums are checked,
//! and a block that fails a check reads as no FAT32 volume rather than a guess.

use super::{
    BLOCK, FAT32_TYPES, FREE_UNKNOWN, MAX_CLUSTERS, MBR_ENTRY, MIN_CLUSTERS, bpb, fsinfo,
    fsinfo_signed, le16, le32, mib, signed,
};

/// The MBR's partition entries, and the bytes in each.
const MBR_ENTRIES: usize = 4;
const MBR_ENTRY_LEN: usize = 16;

/// The FSInfo field's values that name no FSInfo sector.
const NO_FSINFO: [u32; 2] = [0, 0xFFFF];

/// The LBA of the FAT32 volume's boot sector, from the card's block 0: 0 when block 0 is itself
/// a FAT32 boot sector, or else the start of the first of the MBR's four entries that is FAT32.
/// `None` for anything else: no table, another file system, or a GPT.
#[must_use]
pub fn partition(block0: &[u8; BLOCK]) -> Option<u32> {
    if !signed(block0) {
        return None;
    }
    // A boot sector first: its boot code may hold any byte where an MBR keeps its entry's type.
    if volume(block0, 0).is_some() {
        return Some(0);
    }
    (0..MBR_ENTRIES).map(|i| MBR_ENTRY + MBR_ENTRY_LEN * i).find_map(|entry| {
        let start = le32(block0, entry + 8);
        (FAT32_TYPES.contains(&block0[entry + 4]) && start != 0).then_some(start)
    })
}

/// A FAT32 volume, as its boot sector describes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Volume {
    /// The LBA of its boot sector.
    pub start: u32,
    /// The LBA of its FSInfo sector, or `None` when its boot sector names none.
    pub fsinfo_lba: Option<u32>,
    /// The clusters in its data area.
    pub clusters: u32,
    /// Its cluster size, in sectors.
    pub sectors_per_cluster: u8,
    /// Its size, in sectors.
    pub total_blocks: u32,
    /// The LBA of its first FAT: its start plus its reserved sectors.
    pub fat_start: u32,
    /// The sectors in each FAT.
    pub fat_blocks: u32,
    /// How many FATs it keeps, one after another from `fat_start`.
    pub fats: u8,
    /// The first cluster of its root directory, as its boot sector names it.
    pub root_cluster: u32,
    /// Its boot sector's extended flags: bit 7 set means one FAT is active and the others are
    /// not mirrored.
    pub ext_flags: u16,
    /// Its FAT32 version, which every FAT32 reader expects to be 0.
    pub version: u16,
}

/// The FAT32 volume whose boot sector `boot` was read at LBA `start`, or `None` when it is not
/// one: FAT12 or FAT16, exFAT, or bytes that only look like a boot sector. A boot sector whose
/// FSInfo field is 0 or `0xFFFF` names no FSInfo sector, which some formatters write. A volume
/// that ends past LBA `u32::MAX` is none either: no SD card addresses a block there, and every
/// LBA a [`Volume`] computes then fits a `u32`.
///
/// It reads mirroring, version and root cluster without judging them: a volume it reports is not
/// always one the bridge can write.
#[must_use]
pub fn volume(boot: &[u8; BLOCK], start: u32) -> Option<Volume> {
    if !signed(boot) || usize::from(le16(boot, bpb::BYTES_PER_SECTOR)) != BLOCK {
        return None;
    }
    let sectors_per_cluster = boot[bpb::SECTORS_PER_CLUSTER];
    let reserved = u32::from(le16(boot, bpb::RESERVED));
    let fats = boot[bpb::FATS];
    let fat_blocks = le32(boot, bpb::FAT_32);
    let total_blocks = le32(boot, bpb::TOTAL_32);
    let fsinfo_sector = u32::from(le16(boot, bpb::FSINFO));
    let has_fsinfo = !NO_FSINFO.contains(&fsinfo_sector);
    // FAT32 keeps its sizes in the 32-bit fields and leaves the FAT16 ones zero.
    let fat16_fields =
        [bpb::ROOT_ENTRIES, bpb::TOTAL_16, bpb::FAT_16].into_iter().any(|at| le16(boot, at) != 0);
    if !sectors_per_cluster.is_power_of_two()
        || fat16_fields
        || reserved == 0
        || fats == 0
        || fat_blocks == 0
        || (has_fsinfo && fsinfo_sector >= reserved)
    {
        return None;
    }
    let ahead_of_data = u32::from(fats).checked_mul(fat_blocks)?.checked_add(reserved)?;
    // Past `u32::MAX` it is no card's volume.
    start.checked_add(total_blocks)?;
    let clusters = total_blocks.checked_sub(ahead_of_data)? / u32::from(sectors_per_cluster);
    // The FAT has an entry for every cluster, past its two reserved ones.
    let entries = u64::from(fat_blocks) * (BLOCK as u64 / 4);
    if !(MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters) || entries < u64::from(clusters) + 2 {
        return None;
    }
    Some(Volume {
        start,
        fsinfo_lba: if has_fsinfo { Some(start.checked_add(fsinfo_sector)?) } else { None },
        clusters,
        sectors_per_cluster,
        total_blocks,
        // Within the volume, which ends at or before `u32::MAX`.
        fat_start: start + reserved,
        fat_blocks,
        fats,
        root_cluster: le32(boot, bpb::ROOT_CLUSTER),
        ext_flags: le16(boot, bpb::EXT_FLAGS),
        version: le16(boot, bpb::FS_VERSION),
    })
}

impl Volume {
    /// The LBA of its data area, cluster 2's first sector: past every FAT.
    #[must_use]
    pub const fn data_start(&self) -> u32 {
        // Within the volume, as `volume` checked.
        self.fat_start + self.fats as u32 * self.fat_blocks
    }

    /// The LBA of `cluster`'s first sector, or `None` for a cluster outside its data area, the
    /// two reserved entries 0 and 1 included.
    #[must_use]
    pub const fn cluster_lba(&self, cluster: u32) -> Option<u32> {
        if cluster < 2 || cluster - 2 >= self.clusters {
            return None;
        }
        // A cluster of the data area lies within the volume, as `volume` checked.
        Some(self.data_start() + (cluster - 2) * self.sectors_per_cluster as u32)
    }

    /// The free space its FSInfo sector `fsinfo` records, in MiB. `None` when the sector's
    /// signatures are wrong, or it records no count or one past the volume's clusters.
    #[must_use]
    pub fn free_mb(&self, fsinfo: &[u8; BLOCK]) -> Option<u32> {
        let free = le32(fsinfo, fsinfo::FREE);
        (fsinfo_signed(fsinfo) && free != FREE_UNKNOWN && free <= self.clusters)
            .then(|| mib(u64::from(free) * u64::from(self.sectors_per_cluster)))
    }

    /// Its size, in MiB.
    #[must_use]
    pub fn size_mb(&self) -> u32 {
        mib(u64::from(self.total_blocks))
    }

    /// Whether it lies on a card of `card_blocks`, its FSInfo sector, when it has one, and last
    /// block included. A volume that runs past the card's end is no FAT32 volume.
    #[must_use]
    pub fn fits(&self, card_blocks: u64) -> bool {
        u64::from(self.start) + u64::from(self.total_blocks) <= card_blocks
            && self.fsinfo_lba.is_none_or(|lba| u64::from(lba) < card_blocks)
    }
}
