//! A FAT32 format as a list of block writes. [`plan`] fits the layout in [`super`] to a card,
//! and [`Layout::ops`] lists the writes, which the bridge carries out in order.

pub use super::BLOCK;
use super::{
    ALIGN, FSINFO_LEAD, FSINFO_STRUCT, FSINFO_TRAIL, MAX_CLUSTERS, MBR_ENTRY, MIN_CLUSTERS, bpb,
    fsinfo, mib,
};

/// Cluster sizes in sectors, tried largest first.
const CLUSTER_SIZES: [u8; 4] = [64, 32, 16, 8];

/// The fewest reserved sectors: room for the boot sector, FSInfo and their copies at 6 and 7.
const MIN_RESERVED: u32 = 32;

/// The blocks at a card's end a GPT keeps its backup header and table in.
const GPT_BACKUP: u32 = 33;

/// The volume label, in the boot sector and the root directory.
const LABEL: [u8; 11] = *b"WARTUI     ";

/// The MBR partition type: FAT32, LBA addressed.
const PARTITION_TYPE: u8 = 0x0C;

/// The root directory's cluster, the first in the data area.
const ROOT_CLUSTER: u32 = 2;

/// Where the FSInfo sector and the boot sector's copy sit, in sectors from the volume's start.
const FSINFO_SECTOR: u16 = 1;
const BACKUP_SECTOR: u16 = 6;

/// The FAT entries a fresh volume starts with: the media byte, the clean-shutdown marker, then
/// the root directory's cluster, ending its chain.
const FAT_HEAD: [u32; 3] = [0x0FFF_FFF8, 0x0FFF_FFFF, 0x0FFF_FFFF];

/// Why a card cannot take the layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatError {
    /// Even 4 KiB clusters leave fewer than FAT32's 65,525.
    TooSmall,
    /// The card has more than `u32::MAX` blocks (2 TiB), past what an MBR addresses.
    TooLarge,
}

/// A FAT32 volume fitted to a card. Counts are in [`BLOCK`]s, and LBAs from the card's start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The card's size.
    pub card_blocks: u32,
    /// Where the partition, and the volume's boot sector, starts.
    pub part_start: u32,
    /// The partition's size, to the card's end.
    pub part_blocks: u32,
    /// The cluster size.
    pub sectors_per_cluster: u8,
    /// The volume's reserved sectors, padded so the data area starts on a 4 MiB boundary.
    pub reserved: u16,
    /// Each of the two FATs' size.
    pub fat_blocks: u32,
    /// The clusters in the data area.
    pub clusters: u32,
    /// The volume id, also the MBR's disk signature.
    pub volume_id: u32,
}

/// Fits the layout to a card of `card_blocks`.
///
/// # Errors
///
/// [`FormatError::TooLarge`] past `u32::MAX` blocks, and [`FormatError::TooSmall`] when no
/// cluster size reaches FAT32's 65,525 clusters.
pub fn plan(card_blocks: u64, volume_id: u32) -> Result<Layout, FormatError> {
    let card_blocks = u32::try_from(card_blocks).map_err(|_| FormatError::TooLarge)?;
    CLUSTER_SIZES
        .into_iter()
        .find_map(|size| fit(card_blocks, size, volume_id))
        .ok_or(FormatError::TooSmall)
}

/// The layout with clusters of `size` sectors, when it holds a FAT32 cluster count.
fn fit(card_blocks: u32, size: u8, volume_id: u32) -> Option<Layout> {
    let part_blocks = card_blocks.checked_sub(ALIGN)?;
    let per_cluster = u64::from(size);
    // The FAT is sized for the clusters the partition would hold with no FATs at all, so it is
    // never short of an entry, and a little long.
    let most = u64::from(part_blocks.checked_sub(MIN_RESERVED)?) / per_cluster;
    let fat_blocks = ((most + 2) * 4).div_ceil(BLOCK as u64);
    let unpadded = u64::from(ALIGN) + u64::from(MIN_RESERVED) + 2 * fat_blocks;
    let pad = (u64::from(ALIGN) - unpadded % u64::from(ALIGN)) % u64::from(ALIGN);
    let reserved = u64::from(MIN_RESERVED) + pad;
    let data = u64::from(part_blocks).checked_sub(reserved + 2 * fat_blocks)?;
    let clusters = u32::try_from(data / per_cluster).ok()?;
    (MIN_CLUSTERS..=MAX_CLUSTERS).contains(&clusters).then_some(Layout {
        card_blocks,
        part_start: ALIGN,
        part_blocks,
        sectors_per_cluster: size,
        reserved: u16::try_from(reserved).ok()?,
        fat_blocks: u32::try_from(fat_blocks).ok()?,
        clusters,
        volume_id,
    })
}

impl Layout {
    /// The LBA of the first FAT.
    #[must_use]
    pub const fn fat_start(&self) -> u32 {
        self.part_start + self.reserved as u32
    }

    /// The LBA of the data area, where the root directory's cluster starts.
    #[must_use]
    pub const fn data_start(&self) -> u32 {
        self.fat_start() + 2 * self.fat_blocks
    }

    /// The writes that format the card, in the order to make them.
    #[must_use]
    pub const fn ops(&self) -> Ops {
        Ops { layout: *self, step: 0 }
    }

    /// The blocks [`Layout::ops`] writes, for a progress figure.
    #[must_use]
    pub fn blocks_written(&self) -> u64 {
        self.ops().map(|op| u64::from(op.blocks())).sum()
    }

    /// The free space a fresh volume reports, in MiB: every cluster but the root directory's.
    #[must_use]
    pub fn free_mb(&self) -> u32 {
        mib(u64::from(self.clusters - 1) * u64::from(self.sectors_per_cluster))
    }

    /// Volume sector 0, also written at [`BACKUP_SECTOR`].
    fn boot_sector(&self) -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        block[..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
        block[3..11].copy_from_slice(b"MSDOS5.0");
        put16(&mut block, bpb::BYTES_PER_SECTOR, BLOCK as u16);
        block[bpb::SECTORS_PER_CLUSTER] = self.sectors_per_cluster;
        put16(&mut block, bpb::RESERVED, self.reserved);
        block[bpb::FATS] = 2;
        block[bpb::MEDIA] = 0xF8;
        put16(&mut block, bpb::SECTORS_PER_TRACK, 63);
        put16(&mut block, bpb::HEADS, 255);
        put32(&mut block, bpb::HIDDEN, self.part_start);
        put32(&mut block, bpb::TOTAL_32, self.part_blocks);
        put32(&mut block, bpb::FAT_32, self.fat_blocks);
        put32(&mut block, bpb::ROOT_CLUSTER, ROOT_CLUSTER);
        put16(&mut block, bpb::FSINFO, FSINFO_SECTOR);
        put16(&mut block, bpb::BACKUP, BACKUP_SECTOR);
        block[bpb::DRIVE] = 0x80;
        block[bpb::SIGNATURE] = 0x29;
        put32(&mut block, bpb::VOLUME_ID, self.volume_id);
        block[bpb::LABEL..bpb::LABEL + 11].copy_from_slice(&LABEL);
        block[bpb::TYPE..bpb::TYPE + 8].copy_from_slice(b"FAT32   ");
        sign(&mut block);
        block
    }

    /// Volume sector [`FSINFO_SECTOR`], also written the sector after the boot sector's copy.
    fn fsinfo_sector(&self) -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        put32(&mut block, fsinfo::LEAD, FSINFO_LEAD);
        put32(&mut block, fsinfo::STRUCT, FSINFO_STRUCT);
        put32(&mut block, fsinfo::FREE, self.clusters - 1);
        put32(&mut block, fsinfo::NEXT_FREE, ROOT_CLUSTER + 1);
        put32(&mut block, fsinfo::TRAIL, FSINFO_TRAIL);
        block
    }

    /// The first sector of each FAT.
    fn fat_sector() -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        for (at, entry) in (0..).step_by(4).zip(FAT_HEAD) {
            put32(&mut block, at, entry);
        }
        block
    }

    /// The root directory's first sector: the volume label's entry.
    fn root_sector() -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        block[..11].copy_from_slice(&LABEL);
        block[11] = 0x08;
        block
    }

    /// The MBR: the disk signature and one partition entry.
    fn mbr(&self) -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        put32(&mut block, 440, self.volume_id);
        // CHS `FE FF FF` at both ends: past what CHS addresses, so a reader takes the LBAs.
        let chs = [0xFE, 0xFF, 0xFF];
        block[MBR_ENTRY + 1..MBR_ENTRY + 4].copy_from_slice(&chs);
        block[MBR_ENTRY + 4] = PARTITION_TYPE;
        block[MBR_ENTRY + 5..MBR_ENTRY + 8].copy_from_slice(&chs);
        put32(&mut block, MBR_ENTRY + 8, self.part_start);
        put32(&mut block, MBR_ENTRY + 12, self.part_blocks);
        sign(&mut block);
        block
    }
}

/// One write of a format.
// `Block` dwarfs `Zeros`, but boxing it would need an allocator in the bridge firmware. An op is
// built, written and dropped, never stored in bulk.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Zero `count` blocks from `lba`. A run may be megabytes long: [`Op::transfers`] splits it.
    Zeros {
        /// The first block.
        lba: u32,
        /// How many.
        count: u32,
    },
    /// Write `data` at `lba`.
    Block {
        /// The block.
        lba: u32,
        /// Its bytes.
        data: [u8; BLOCK],
    },
}

impl Op {
    /// The first block the op writes.
    #[must_use]
    pub const fn lba(&self) -> u32 {
        match self {
            Self::Zeros { lba, .. } | Self::Block { lba, .. } => *lba,
        }
    }

    /// The blocks the op writes.
    #[must_use]
    pub const fn blocks(&self) -> u32 {
        match self {
            Self::Zeros { count, .. } => *count,
            Self::Block { .. } => 1,
        }
    }

    /// The op as transfers of at most `max` blocks, in order: a `Zeros` run in pieces, a `Block`
    /// as one. A `max` of 0 is taken as 1.
    #[must_use]
    pub const fn transfers(&self, max: u32) -> Transfers {
        let max = if max == 0 { 1 } else { max };
        Transfers { lba: self.lba(), left: self.blocks(), max }
    }
}

/// [`Op::transfers`]: each transfer's first block and its count.
#[derive(Debug, Clone)]
pub struct Transfers {
    lba: u32,
    left: u32,
    max: u32,
}

impl Iterator for Transfers {
    type Item = (u32, u32);

    fn next(&mut self) -> Option<(u32, u32)> {
        if self.left == 0 {
            return None;
        }
        let count = self.left.min(self.max);
        let lba = self.lba;
        self.lba = self.lba.saturating_add(count);
        self.left -= count;
        Some((lba, count))
    }
}

/// [`Layout::ops`]: the writes of a format, in order.
#[derive(Debug, Clone)]
pub struct Ops {
    layout: Layout,
    step: u8,
}

impl Iterator for Ops {
    type Item = Op;

    fn next(&mut self) -> Option<Op> {
        let layout = &self.layout;
        let start = layout.part_start;
        let op = match self.step {
            // The old MBR, any GPT header and whatever sat ahead of the partition.
            0 => Op::Zeros { lba: 0, count: ALIGN },
            // A GPT's backup header and table, which a reader would otherwise find and trust.
            1 => Op::Zeros { lba: layout.card_blocks - GPT_BACKUP, count: GPT_BACKUP },
            // The reserved sectors, both FATs and the root directory's cluster, in one run.
            2 => Op::Zeros {
                lba: start,
                count: layout.data_start() - start + u32::from(layout.sectors_per_cluster),
            },
            3 => Op::Block { lba: start, data: layout.boot_sector() },
            4 => Op::Block { lba: start + u32::from(FSINFO_SECTOR), data: layout.fsinfo_sector() },
            5 => Op::Block { lba: start + u32::from(BACKUP_SECTOR), data: layout.boot_sector() },
            6 => Op::Block {
                lba: start + u32::from(BACKUP_SECTOR + FSINFO_SECTOR),
                data: layout.fsinfo_sector(),
            },
            7 => Op::Block { lba: layout.fat_start(), data: Layout::fat_sector() },
            8 => Op::Block {
                lba: layout.fat_start() + layout.fat_blocks,
                data: Layout::fat_sector(),
            },
            9 => Op::Block { lba: layout.data_start(), data: Layout::root_sector() },
            // Last, so a format cut short leaves block 0 zeroed rather than naming a volume.
            10 => Op::Block { lba: 0, data: layout.mbr() },
            _ => return None,
        };
        self.step += 1;
        Some(op)
    }
}

fn put16(block: &mut [u8; BLOCK], at: usize, value: u16) {
    block[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put32(block: &mut [u8; BLOCK], at: usize, value: u32) {
    block[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Ends a block in the `55 AA` boot signature.
fn sign(block: &mut [u8; BLOCK]) {
    block[BLOCK - 2..].copy_from_slice(&[0x55, 0xAA]);
}
