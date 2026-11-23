//! The microSD card's FAT32 format and the reading of a card, checked against `fatfs`, a FAT32
//! implementation written by someone else: a plan that only agreed with its own reader would
//! prove nothing about a laptop's.

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

use wartui_proto::sd::BLOCK;
use wartui_proto::sd::format::{FormatError, Layout, Op, plan};
use wartui_proto::sd::inspect::{partition, volume};

/// A card of `blocks`, holding only the blocks written non-zero: a 128 GB card fits in memory.
#[derive(Clone)]
struct Card {
    blocks: u64,
    written: HashMap<u64, [u8; BLOCK]>,
}

impl Card {
    fn new(blocks: u64) -> Self {
        Self { blocks, written: HashMap::new() }
    }

    fn block(&self, lba: u64) -> [u8; BLOCK] {
        self.written.get(&lba).copied().unwrap_or([0; BLOCK])
    }

    fn put(&mut self, lba: u64, data: [u8; BLOCK]) {
        assert!(lba < self.blocks, "block {lba} is past the card's end");
        if data == [0; BLOCK] {
            self.written.remove(&lba);
        } else {
            self.written.insert(lba, data);
        }
    }

    fn apply(&mut self, op: &Op) {
        match op {
            Op::Zeros { lba, count } => {
                for lba in u64::from(*lba)..u64::from(*lba) + u64::from(*count) {
                    self.put(lba, [0; BLOCK]);
                }
            }
            Op::Block { lba, data } => self.put(u64::from(*lba), *data),
        }
    }

    /// A card that held something before: an old GPT at both ends, and junk where the FATs and
    /// the root directory go.
    fn used(blocks: u64, layout: &Layout) -> Self {
        let mut card = Self::new(blocks);
        for lba in [1, 2, blocks - 1, blocks - 33] {
            card.put(lba, [0xA5; BLOCK]);
        }
        for lba in [layout.fat_start() + 1, layout.fat_start() + layout.fat_blocks + 7] {
            card.put(u64::from(lba), [0xA5; BLOCK]);
        }
        card.put(u64::from(layout.data_start()) + 1, [0xA5; BLOCK]);
        card
    }

    fn formatted(layout: &Layout) -> Self {
        let mut card = Self::used(u64::from(layout.card_blocks), layout);
        for op in layout.ops() {
            card.apply(&op);
        }
        card
    }

    /// The partition at `start` as a stream from its first byte, as `fatfs` reads a volume.
    fn partition(&mut self, start: u32) -> Partition<'_> {
        let len = (self.blocks - u64::from(start)) * BLOCK as u64;
        Partition { card: self, base: u64::from(start) * BLOCK as u64, len, pos: 0 }
    }
}

struct Partition<'a> {
    card: &'a mut Card,
    base: u64,
    len: u64,
    pos: u64,
}

impl Partition<'_> {
    /// The block holding the stream's position, and the offset in it.
    fn at(&self) -> (u64, usize) {
        let byte = self.base + self.pos;
        (byte / BLOCK as u64, (byte % BLOCK as u64) as usize)
    }
}

impl Read for Partition<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let (lba, offset) = self.at();
        let n = buf.len().min(BLOCK - offset).min((self.len - self.pos.min(self.len)) as usize);
        buf[..n].copy_from_slice(&self.card.block(lba)[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for Partition<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let (lba, offset) = self.at();
        let n = buf.len().min(BLOCK - offset).min((self.len - self.pos.min(self.len)) as usize);
        let mut block = self.card.block(lba);
        block[offset..offset + n].copy_from_slice(&buf[..n]);
        self.card.put(lba, block);
        self.pos += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for Partition<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let pos = match to {
            SeekFrom::Start(pos) => Some(pos),
            SeekFrom::Current(by) => self.pos.checked_add_signed(by),
            SeekFrom::End(by) => self.len.checked_add_signed(by),
        };
        self.pos = pos.ok_or_else(|| io::Error::other("seek before the partition"))?;
        Ok(self.pos)
    }
}

/// One card size and the geometry its plan pins.
struct Geometry {
    name: &'static str,
    card_blocks: u64,
    sectors_per_cluster: u8,
    reserved: u16,
    fat_blocks: u32,
    clusters: u32,
}

/// Cards of 512 MiB, then 4 to 128 GB, decimal as cards are sold, and the bench's "64 GB" card.
const GEOMETRY: [Geometry; 7] = [
    Geometry {
        name: "512 MiB",
        card_blocks: 1_048_576,
        sectors_per_cluster: 8,
        reserved: 6160,
        fat_blocks: 1016,
        clusters: 129_024,
    },
    Geometry {
        name: "4 GB",
        card_blocks: 7_812_500,
        sectors_per_cluster: 64,
        reserved: 6286,
        fat_blocks: 953,
        clusters: 121_814,
    },
    Geometry {
        name: "16 GB",
        card_blocks: 31_250_000,
        sectors_per_cluster: 64,
        reserved: 564,
        fat_blocks: 3814,
        clusters: 488_025,
    },
    Geometry {
        name: "32 GB",
        card_blocks: 62_500_000,
        sectors_per_cluster: 64,
        reserved: 1126,
        fat_blocks: 7629,
        clusters: 976_178,
    },
    Geometry {
        name: "bench 64 GB",
        card_blocks: 123_596_800,
        sectors_per_cluster: 64,
        reserved: 2594,
        fat_blocks: 15_087,
        clusters: 1_930_560,
    },
    Geometry {
        name: "64 GB",
        card_blocks: 125_000_000,
        sectors_per_cluster: 64,
        reserved: 2252,
        fat_blocks: 15_258,
        clusters: 1_952_485,
    },
    Geometry {
        name: "128 GB",
        card_blocks: 250_000_000,
        sectors_per_cluster: 64,
        reserved: 4502,
        fat_blocks: 30_517,
        clusters: 3_905_098,
    },
];

const VOLUME_ID: u32 = 0x1234_ABCD;

fn layout(card_blocks: u64) -> Layout {
    plan(card_blocks, VOLUME_ID).expect("the card takes the layout")
}

#[test]
fn sd_plan_pins_geometry_when_card_is_each_size() {
    for row in &GEOMETRY {
        let layout = layout(row.card_blocks);
        let got = (layout.sectors_per_cluster, layout.reserved, layout.fat_blocks, layout.clusters);
        let want = (row.sectors_per_cluster, row.reserved, row.fat_blocks, row.clusters);
        assert_eq!(got, want, "{}", row.name);
        assert_eq!(layout.part_start, 8192, "{}", row.name);
        assert_eq!(u64::from(layout.part_start + layout.part_blocks), row.card_blocks);
        assert_eq!(layout.data_start() % 8192, 0, "{}: the data area is 4 MiB aligned", row.name);
        // The FAT has an entry for every cluster and the two reserved ones.
        assert!(u64::from(layout.fat_blocks) * 128 >= u64::from(layout.clusters) + 2);
    }
}

#[test]
fn sd_plan_pins_blocks_written_when_card_is_the_bench_card() {
    assert_eq!(layout(123_596_800).blocks_written(), 41_065);
}

#[test]
fn sd_plan_refuses_card_when_too_small_or_past_u32_blocks() {
    for blocks in [0, 8192, 8225, 100_000, 524_288] {
        assert_eq!(plan(blocks, VOLUME_ID), Err(FormatError::TooSmall), "{blocks} blocks");
    }
    assert_eq!(plan(u64::from(u32::MAX) + 1, VOLUME_ID), Err(FormatError::TooLarge));
    // 2 TiB less a block is the largest card an MBR addresses, and takes 32 KiB clusters.
    assert_eq!(layout(u64::from(u32::MAX)).sectors_per_cluster, 64);
}

#[test]
fn sd_plan_takes_smaller_clusters_when_32_kib_leaves_too_few() {
    // 4 GiB, 2 GiB, 1 GiB and 512 MiB: each a little short of the next size up's 65,525.
    for (blocks, size) in [(8_388_608, 64), (4_194_304, 32), (2_097_152, 16), (1_048_576, 8)] {
        assert_eq!(layout(blocks).sectors_per_cluster, size, "{blocks} blocks");
    }
}

#[test]
fn sd_ops_zero_block_0_first_and_write_mbr_last_when_listed() {
    for row in &GEOMETRY {
        let layout = layout(row.card_blocks);
        let ops: Vec<Op> = layout.ops().collect();
        assert_eq!(ops.first(), Some(&Op::Zeros { lba: 0, count: 8192 }), "{}", row.name);
        let Some(Op::Block { lba: 0, data }) = ops.last() else {
            panic!("{}: the MBR is not the last op", row.name);
        };
        assert_eq!(data[510..], [0x55, 0xAA]);
        // Nothing between them writes block 0.
        assert!(ops[1..ops.len() - 1].iter().all(|op| op.lba() != 0), "{}", row.name);
        // A GPT's backup header and table are zeroed.
        assert!(ops.contains(&Op::Zeros { lba: layout.card_blocks - 33, count: 33 }));
        for op in &ops {
            let end = u64::from(op.lba()) + u64::from(op.blocks());
            assert!(end <= row.card_blocks, "{}: {op:?} runs past the card", row.name);
        }
        let sum: u64 = ops.iter().map(|op| u64::from(op.blocks())).sum();
        assert_eq!(layout.blocks_written(), sum, "{}", row.name);
    }
}

#[test]
fn sd_ops_pin_mbr_and_boot_sector_bytes_when_card_is_the_bench_card() {
    let layout = layout(123_596_800);
    let card = Card::formatted(&layout);
    let mbr = card.block(0);
    assert_eq!(mbr[440..444], VOLUME_ID.to_le_bytes(), "disk signature");
    #[rustfmt::skip]
    let entry = [
        0x00,                   // status
        0xFE, 0xFF, 0xFF,       // CHS first
        0x0C,                   // FAT32, LBA
        0xFE, 0xFF, 0xFF,       // CHS last
        0x00, 0x20, 0x00, 0x00, // LBA start 8192
        0x00, 0xD0, 0x5D, 0x07, // 123,588,608 blocks
    ];
    assert_eq!(mbr[446..462], entry);
    assert!(mbr[462..510].iter().all(|&byte| byte == 0), "one entry");
    let boot = card.block(8192);
    assert_eq!(boot[..11], *b"\xEB\x58\x90MSDOS5.0");
    assert_eq!(boot[21], 0xF8, "media");
    assert_eq!(boot[28..32], 8192u32.to_le_bytes(), "hidden sectors");
    assert_eq!(boot[64..67], [0x80, 0, 0x29], "drive, then the extended boot signature");
    assert_eq!(boot[67..71], VOLUME_ID.to_le_bytes());
    assert_eq!(boot[71..90], *b"WARTUI     FAT32   ");
    assert_eq!(card.block(8192 + 6), boot, "the backup boot sector");
    let fsinfo = card.block(8193);
    assert_eq!(fsinfo[488..496], [(1_930_560u32 - 1).to_le_bytes(), 3u32.to_le_bytes()].concat());
    assert_eq!(card.block(8193 + 6), fsinfo, "the backup FSInfo");
}

#[test]
fn sd_format_mounts_in_fatfs_when_card_is_each_size() {
    for row in &GEOMETRY {
        let layout = layout(row.card_blocks);
        let mut card = Card::formatted(&layout);
        let cluster = u32::from(layout.sectors_per_cluster) * BLOCK as u32;

        // FSInfo's count says nothing about the FATs, so count them too: a copy whose FSInfo
        // records no count makes `fatfs` read every entry.
        let mut uncounted = card.clone();
        let fsinfo_lba = u64::from(layout.part_start) + 1;
        let mut fsinfo = uncounted.block(fsinfo_lba);
        fsinfo[488..492].copy_from_slice(&u32::MAX.to_le_bytes());
        uncounted.put(fsinfo_lba, fsinfo);
        {
            let fs = fatfs::FileSystem::new(
                uncounted.partition(layout.part_start),
                fatfs::FsOptions::new(),
            )
            .unwrap_or_else(|e| panic!("{}: does not mount: {e}", row.name));
            let stats = fs.stats().expect("stats");
            assert_eq!(stats.free_clusters(), layout.clusters - 1, "{}: counted", row.name);
        }

        let fs = fatfs::FileSystem::new(card.partition(layout.part_start), fatfs::FsOptions::new())
            .unwrap_or_else(|e| panic!("{}: does not mount: {e}", row.name));
        assert_eq!(fs.fat_type(), fatfs::FatType::Fat32, "{}", row.name);
        assert_eq!(fs.volume_id(), VOLUME_ID);
        assert_eq!(fs.volume_label(), "WARTUI", "{}", row.name);
        assert_eq!(fs.read_volume_label_from_root_dir().expect("reads"), Some("WARTUI".into()));
        let stats = fs.stats().expect("stats");
        assert_eq!(stats.cluster_size(), cluster, "{}", row.name);
        assert_eq!(stats.total_clusters(), layout.clusters, "{}", row.name);
        assert_eq!(stats.free_clusters(), layout.clusters - 1, "{}", row.name);
        assert_eq!(
            (u64::from(stats.free_clusters()) * u64::from(cluster)) >> 20,
            u64::from(layout.free_mb()),
            "{}",
            row.name
        );

        // A file over several clusters, written and read back.
        let text: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let root = fs.root_dir();
        root.create_file("drive.csv").expect("creates").write_all(&text).expect("writes");
        let mut read = Vec::new();
        root.open_file("drive.csv").expect("opens").read_to_end(&mut read).expect("reads");
        assert_eq!(read, text, "{}", row.name);
        drop(root);
        fs.unmount().expect("unmounts");

        let start = u64::from(layout.part_start);
        assert_eq!(
            card.block(start + 6),
            card.block(start),
            "{}: the backup boot sector",
            row.name
        );
    }
}

#[test]
fn sd_inspect_reads_layout_when_card_is_formatted() {
    for row in &GEOMETRY {
        let layout = layout(row.card_blocks);
        let card = Card::formatted(&layout);
        let start = partition(&card.block(0)).expect("a FAT32 partition");
        assert_eq!(start, layout.part_start, "{}", row.name);
        let found = volume(&card.block(u64::from(start)), start).expect("a FAT32 volume");
        assert_eq!(found.clusters, layout.clusters, "{}", row.name);
        assert_eq!(found.sectors_per_cluster, layout.sectors_per_cluster);
        assert_eq!(found.fsinfo_lba, start + 1);
        assert_eq!(found.size_mb(), layout.part_blocks / 2048, "{}", row.name);
        let fsinfo = card.block(u64::from(found.fsinfo_lba));
        assert_eq!(found.free_mb(&fsinfo), Some(layout.free_mb()), "{}", row.name);
    }
}

#[test]
fn sd_inspect_reads_volume_at_block_0_when_card_has_no_partition_table() {
    let layout = layout(31_250_000);
    let card = Card::formatted(&layout);
    let boot = card.block(u64::from(layout.part_start));
    assert_eq!(partition(&boot), Some(0));
    assert_eq!(volume(&boot, 0).expect("a volume").fsinfo_lba, 1);
}

#[test]
fn sd_inspect_reads_nothing_when_card_holds_exfat_zeros_or_fat16() {
    let layout = layout(31_250_000);
    let card = Card::formatted(&layout);

    let mut exfat = card.block(0);
    exfat[446 + 4] = 0x07;
    assert_eq!(partition(&exfat), None, "an exFAT entry");
    assert_eq!(partition(&[0; BLOCK]), None, "a zeroed block 0");
    let mut no_start = card.block(0);
    no_start[454..458].fill(0);
    assert_eq!(partition(&no_start), None, "an entry starting at block 0");

    // A 1 GB FAT16 boot sector: its sizes in the 16-bit fields and a fixed root directory.
    let mut fat16 = [0; BLOCK];
    fat16[..11].copy_from_slice(b"\xEB\x3C\x90MSDOS5.0");
    fat16[11..13].copy_from_slice(&512u16.to_le_bytes());
    fat16[13] = 32;
    fat16[14..16].copy_from_slice(&8u16.to_le_bytes());
    fat16[16] = 2;
    fat16[17..19].copy_from_slice(&512u16.to_le_bytes());
    fat16[21] = 0xF8;
    fat16[22..24].copy_from_slice(&245u16.to_le_bytes());
    fat16[32..36].copy_from_slice(&1_953_125u32.to_le_bytes());
    fat16[54..62].copy_from_slice(b"FAT16   ");
    fat16[510..].copy_from_slice(&[0x55, 0xAA]);
    assert_eq!(partition(&fat16), None, "a FAT16 boot sector at block 0");
    assert_eq!(volume(&fat16, 8192), None, "a FAT16 boot sector");

    // A FAT32 boot sector with too few clusters is FAT16 to every reader.
    let mut small = card.block(u64::from(layout.part_start));
    small[32..36].copy_from_slice(&(layout.data_start() - 8192 + 65_524 * 64).to_le_bytes());
    assert_eq!(volume(&small, 8192), None, "65,524 clusters");
}

#[test]
fn sd_inspect_reads_no_free_space_when_fsinfo_is_unsigned_unknown_or_too_large() {
    let layout = layout(31_250_000);
    let card = Card::formatted(&layout);
    let found = volume(&card.block(8192), 8192).expect("a volume");
    let fsinfo = card.block(8193);
    for (at, value, why) in [
        (0, 0, "lead signature"),
        (484, 0, "struct signature"),
        (508, 0, "trail signature"),
        (488, u32::MAX, "no count"),
        (488, layout.clusters + 1, "more free than the volume holds"),
    ] {
        let mut bad = fsinfo;
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        assert_eq!(found.free_mb(&bad), None, "{why}");
    }
    let mut full = fsinfo;
    full[488..492].fill(0);
    assert_eq!(found.free_mb(&full), Some(0));
}

#[test]
fn sd_inspect_never_panics_when_blocks_are_random() {
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let found = volume(&Card::formatted(&layout(31_250_000)).block(8192), 8192).expect("a volume");
    for round in 0..20_000 {
        let mut block = [0; BLOCK];
        for chunk in block.chunks_mut(8) {
            chunk.copy_from_slice(&next().to_le_bytes());
        }
        // Half the blocks look enough like a boot sector to reach its arithmetic.
        if round % 2 == 0 {
            block[11..13].copy_from_slice(&512u16.to_le_bytes());
            block[13] = 1 << (round / 2 % 8);
            block[17..21].fill(0);
            block[22..24].fill(0);
            block[510..].copy_from_slice(&[0x55, 0xAA]);
        }
        let _ = partition(&block);
        for start in [0, 8192, u32::MAX] {
            if let Some(found) = volume(&block, start) {
                let _ = found.free_mb(&block);
                let _ = found.size_mb();
            }
        }
        let _ = found.free_mb(&block);
    }
}
