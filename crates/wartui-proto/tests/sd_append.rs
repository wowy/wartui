//! The SD log's writer, driven step by step on an in-memory card and checked against `fatfs`, a
//! FAT32 implementation written by someone else, and against a strict `fsck` of the test's own
//! that replays every power cut a scenario could suffer.

use std::collections::HashMap;
use std::io::{Read, Write};

use wartui_proto::sd::BLOCK;
use wartui_proto::sd::append::{Appender, CHUNK_BLOCKS, Fault, Policy, Refused, SCAN_MAX, Step};
use wartui_proto::sd::fat::{
    self, BAD, DELETED, DirEntry, EOC, attr, dir_entry, entry, is_end, is_free, is_link,
    parse_log_name, put_dir_entry, sector_of, set_entry,
};
use wartui_proto::sd::format::plan;
use wartui_proto::sd::inspect::{Volume, volume};
use wartui_proto::sd::log::{Kind, RECORD_MAX, decode, encode, records};

#[path = "common/sd_card.rs"]
mod sd_card;
use sd_card::{Card, Driver};

/// The bridge's sizes: 64 MiB files, 256 MiB kept free.
const POLICY: Policy = Policy { file_max: 64 << 20, keep_free_mb: 256 };

/// A 32 GB card, decimal as sold, and a 512 MiB one, which takes 4 KiB clusters.
const CARD_32GB: u64 = 62_500_000;
const CARD_512MIB: u64 = 1_048_576;

/// FSInfo's free count and next-free hint, by offset.
const FREE_AT: usize = 488;
const NEXT_AT: usize = 492;

/// A seeded xorshift, so every random test is reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn block(&mut self) -> [u8; BLOCK] {
        let mut block = [0; BLOCK];
        for chunk in block.chunks_mut(8) {
            chunk.copy_from_slice(&self.next().to_le_bytes());
        }
        block
    }
}

/// A card formatted by the bridge's own plan, and its volume.
fn planned(card_blocks: u64) -> (Card, Volume) {
    let layout = plan(card_blocks, 0x1234_ABCD).expect("the card takes the layout");
    let card = Card::formatted(&layout);
    let found = volume(&card.block(8192), 8192).expect("a FAT32 volume");
    (card, found)
}

/// A volume `fatfs` formatted, with no partition table: `sectors` long, clusters of
/// `cluster_bytes`, `fats` FATs.
fn by_fatfs(sectors: u32, cluster_bytes: u32, fats: u8) -> (Card, Volume) {
    let mut card = Card::new(u64::from(sectors));
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat32)
        .bytes_per_cluster(cluster_bytes)
        .total_sectors(sectors)
        .fats(fats);
    fatfs::format_volume(card.partition(0), options).expect("formats");
    let found = volume(&card.block(0), 0).expect("a FAT32 volume");
    (card, found)
}

/// The smallest FAT32 volume `fatfs` makes: 512-byte clusters, so a flush crosses clusters and
/// FAT sectors often.
fn small() -> (Card, Volume) {
    by_fatfs(70_000, 512, 2)
}

fn mounted(found: &Volume, policy: Policy) -> Appender {
    let mut app = Appender::new();
    app.mount(found, policy).expect("mounts");
    app
}

fn cluster_bytes(found: &Volume) -> u32 {
    u32::from(found.sectors_per_cluster) * BLOCK as u32
}

fn lba(found: &Volume, cluster: u32) -> u32 {
    found.cluster_lba(cluster).expect("a cluster of the data area")
}

fn le32(block: &[u8; BLOCK], at: usize) -> u32 {
    u32::from_le_bytes(block[at..at + 4].try_into().expect("four bytes"))
}

fn put32(card: &mut Card, at_lba: u32, at: usize, value: u32) {
    let mut block = card.block(u64::from(at_lba));
    block[at..at + 4].copy_from_slice(&value.to_le_bytes());
    card.put(u64::from(at_lba), block);
}

/// The free count FSInfo records.
fn fsinfo_free(card: &Card, found: &Volume) -> u32 {
    le32(&card.block(u64::from(found.fsinfo_lba.expect("an FSInfo sector"))), FREE_AT)
}

/// The free clusters `fatfs` counts in the FAT, its FSInfo count set aside.
fn counted_free(card: &Card, found: &Volume) -> u32 {
    let mut copy = card.clone();
    if let Some(at) = found.fsinfo_lba {
        put32(&mut copy, at, FREE_AT, u32::MAX);
    }
    let fs = fatfs::FileSystem::new(copy.partition(found.start), fatfs::FsOptions::new())
        .expect("mounts");
    fs.stats().expect("stats").free_clusters()
}

/// The files `fatfs` lists in `path`, `""` for the root, with their sizes.
fn listed(card: &Card, found: &Volume, path: &str) -> Vec<(String, u64)> {
    let mut copy = card.clone();
    let fs = fatfs::FileSystem::new(copy.partition(found.start), fatfs::FsOptions::new())
        .expect("mounts");
    let root = fs.root_dir();
    let dir = if path.is_empty() { root } else { root.open_dir(path).expect("opens") };
    dir.iter()
        .map(|e| e.expect("reads"))
        .filter(fatfs::DirEntry::is_file)
        .map(|e| (e.file_name(), e.len()))
        .collect()
}

/// A file's bytes, as `fatfs` reads them.
fn read(card: &Card, found: &Volume, path: &str) -> Vec<u8> {
    let mut copy = card.clone();
    let fs = fatfs::FileSystem::new(copy.partition(found.start), fatfs::FsOptions::new())
        .expect("mounts");
    let mut out = Vec::new();
    fs.root_dir().open_file(path).expect("opens").read_to_end(&mut out).expect("reads");
    out
}

/// Make `dirs`, then write `files`, with `fatfs`.
fn create(card: &mut Card, found: &Volume, dirs: &[&str], files: &[(&str, &[u8])]) {
    let fs = fatfs::FileSystem::new(card.partition(found.start), fatfs::FsOptions::new())
        .expect("mounts");
    let root = fs.root_dir();
    for dir in dirs {
        root.create_dir(dir).expect("creates the directory");
    }
    for (path, bytes) in files {
        root.create_file(path).expect("creates").write_all(bytes).expect("writes");
    }
    drop(root);
    fs.unmount().expect("unmounts");
}

/// A FAT copy as a table of entries, one per cluster and the two reserved ones.
fn fat_table(card: &Card, found: &Volume, copy: u32) -> Vec<u32> {
    let len = found.clusters as usize + 2;
    let base = found.fat_start + copy * found.fat_blocks;
    let mut out = Vec::with_capacity(len);
    for sector in 0..(len as u32).div_ceil(128) {
        let block = card.block(u64::from(base + sector));
        for slot in 0..128 {
            if out.len() < len {
                out.push(entry(&block, sector * 128 + slot));
            }
        }
    }
    out
}

/// Set FAT entries in every copy.
fn set_fat(card: &mut Card, found: &Volume, entries: &[(u32, u32)]) {
    let mut sectors: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
    for &(cluster, value) in entries {
        sectors.entry(sector_of(cluster)).or_default().push((cluster, value));
    }
    for copy in 0..u32::from(found.fats) {
        for (sector, entries) in &sectors {
            let at = u64::from(found.fat_start + copy * found.fat_blocks + sector);
            let mut block = card.block(at);
            for &(cluster, value) in entries {
                set_entry(&mut block, cluster, value);
            }
            card.put(at, block);
        }
    }
}

/// The chain from `first` in FAT1, as the card holds it.
fn chain_of(card: &Card, found: &Volume, first: u32) -> Vec<u32> {
    let fat = fat_table(card, found, 0);
    let mut out = vec![first];
    let mut at = first;
    while is_link(fat[at as usize], found.clusters) {
        at = fat[at as usize];
        out.push(at);
    }
    out
}

/// Put `entry` in the first end slot of the root's first sector.
fn root_entry(card: &mut Card, found: &Volume, entry: &DirEntry) {
    let at = u64::from(lba(found, found.root_cluster));
    let mut sector = card.block(at);
    let free = (0..16).find(|&i| dir_entry(&sector, i).is_some_and(|e| e.is_end()));
    assert!(put_dir_entry(&mut sector, free.expect("room in the root"), entry));
    card.put(at, sector);
}

/// Take every free cluster but the lowest `leave` into one foreign file, `FILLER.BIN`, so a
/// small volume runs short of space in a few flushes.
fn fill(card: &mut Card, found: &Volume, leave: usize) {
    let fat = fat_table(card, found, 0);
    let free: Vec<u32> = (2..found.clusters + 2).filter(|&c| is_free(fat[c as usize])).collect();
    let taken = &free[leave..];
    let mut links: Vec<(u32, u32)> = taken.windows(2).map(|w| (w[0], w[1])).collect();
    links.push((*taken.last().expect("some to take"), EOC));
    set_fat(card, found, &links);
    let size = u32::try_from(taken.len()).expect("fits") * cluster_bytes(found);
    root_entry(card, found, &DirEntry::file(*b"FILLER  BIN", taken[0], size));
    if let Some(at) = found.fsinfo_lba {
        put32(card, at, FREE_AT, u32::try_from(leave).expect("fits"));
    }
}

/// A stream of `PhoneCommand` records, about `bytes` long, with random payloads.
fn stream(rng: &mut Rng, bytes: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ms = 0;
    while out.len() < bytes {
        let payload: Vec<u8> = (0..rng.below(700)).map(|_| rng.next() as u8).collect();
        let mut record = [0u8; RECORD_MAX];
        let used = encode(Kind::PhoneCommand, ms, &payload, &mut record).expect("encodes");
        out.extend_from_slice(&record[..used]);
        ms += 1;
    }
    out
}

/// The records of `file` that decode, as bytes, and how many did not.
fn decoded(file: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let mut good = Vec::new();
    let mut bad = 0;
    for range in records(file) {
        let mut copy = file[range.clone()].to_vec();
        match decode(&mut copy) {
            Ok(_) => good.push(file[range].to_vec()),
            Err(_) => bad += 1,
        }
    }
    (good, bad)
}

/// Flush `bytes` in random pieces, each at most what the staging buffer takes.
fn flush_all(driver: &mut Driver, app: &mut Appender, rng: &mut Rng, bytes: &[u8]) {
    let mut at = 0;
    while at < bytes.len() {
        let n = (1 + rng.below(app.room())).min(bytes.len() - at);
        driver.flush(app, &bytes[at..at + n]).expect("flushes");
        at += n;
    }
}

/// What a strict check of the volume finds.
#[derive(Debug, Default)]
struct Findings {
    /// Clusters marked used that no entry reaches.
    lost: usize,
    /// Chains longer than their file's size.
    long: usize,
    /// FAT sectors that differ between the copies.
    fats_differ: usize,
    /// FSInfo's count is not the FAT's.
    fsinfo_wrong: bool,
    /// Anything a cut must never leave: cross-links, entries or links reaching free clusters,
    /// loops, sizes past their chains, broken `.` and `..`.
    bad: Vec<String>,
    /// The log's files: number, first cluster, size.
    log: Vec<(u32, u32, u32)>,
}

impl Findings {
    fn assert_allowed(&self, what: &str) {
        assert!(self.bad.is_empty(), "{what}: {:?}", self.bad);
        assert!(self.fats_differ <= 1, "{what}: the FATs differ in {} sectors", self.fats_differ);
    }
}

/// A strict `fsck`: walk every chain from every entry, through each FAT copy, then compare the
/// copies and recount the free clusters.
fn fsck(card: &Card, found: &Volume) -> Findings {
    let mut findings = Findings::default();
    let tables: Vec<Vec<u32>> =
        (0..u32::from(found.fats)).map(|c| fat_table(card, found, c)).collect();
    for (copy, fat) in tables.iter().enumerate() {
        let mut owned = Owners { by_cluster: vec![0; fat.len()], names: Vec::new() };
        let mut view = Findings::default();
        walk_dir(card, found, fat, found.root_cluster, None, "", &mut owned, &mut view);
        findings.bad.extend(view.bad.into_iter().map(|bad| format!("FAT{}: {bad}", copy + 1)));
        if copy == 0 {
            findings.long = view.long;
            findings.log = view.log;
            findings.lost = (2..found.clusters + 2)
                .filter(|&c| {
                    let e = fat[c as usize];
                    !is_free(e) && e != BAD && owned.by_cluster[c as usize] == 0
                })
                .count();
        }
    }
    if let [one, two] = &tables[..] {
        findings.fats_differ =
            one.chunks(128).zip(two.chunks(128)).filter(|(one, two)| one != two).count();
    }
    if found.fsinfo_lba.is_some() {
        let free = tables[0][2..].iter().filter(|&&e| is_free(e)).count();
        findings.fsinfo_wrong = fsinfo_free(card, found) as usize != free;
    }
    findings
}

/// The chain from `first` through `fat`, reporting what is wrong with it.
fn chain(fat: &[u32], found: &Volume, first: u32, what: &str, bad: &mut Vec<String>) -> Vec<u32> {
    let mut out = Vec::new();
    let mut seen = vec![false; fat.len()];
    let mut at = first;
    loop {
        if !is_link(at, found.clusters) {
            bad.push(format!("{what}: links to {at:#X}, outside the data area"));
            break;
        }
        let next = fat[at as usize];
        if is_free(next) {
            bad.push(format!("{what}: reaches free cluster {at}"));
            break;
        }
        if std::mem::replace(&mut seen[at as usize], true) {
            bad.push(format!("{what}: loops at {at}"));
            break;
        }
        out.push(at);
        if is_end(next) {
            break;
        }
        at = next;
    }
    out
}

/// Claim `chain` for `what`, reporting a cluster another entry reaches.
fn claim(owned: &mut Owners, chain: &[u32], what: &str, bad: &mut Vec<String>) {
    owned.names.push(what.to_owned());
    let me = owned.names.len();
    for &cluster in chain {
        let other = std::mem::replace(&mut owned.by_cluster[cluster as usize], me);
        if other != 0 {
            bad.push(format!("{what}: cross-linked with {} at {cluster}", owned.names[other - 1]));
        }
    }
}

/// Which entry reaches each cluster: an index into `names`, plus one, or 0 for none.
struct Owners {
    by_cluster: Vec<usize>,
    names: Vec<String>,
}

#[allow(clippy::too_many_arguments)]
fn walk_dir(
    card: &Card,
    found: &Volume,
    fat: &[u32],
    first: u32,
    parent: Option<u32>,
    path: &str,
    owned: &mut Owners,
    findings: &mut Findings,
) {
    let what = if path.is_empty() { "/" } else { path };
    let clusters = chain(fat, found, first, what, &mut findings.bad);
    claim(owned, &clusters, what, &mut findings.bad);
    let per = cluster_bytes(found);
    let mut index = 0;
    'dir: for &cluster in &clusters {
        for sector in 0..u32::from(found.sectors_per_cluster) {
            let block = card.block(u64::from(lba(found, cluster) + sector));
            for i in 0..16 {
                let e = dir_entry(&block, i).expect("16 entries");
                // `fatfs` gives even `.` and `..` a long-name entry.
                if e.is_long_name() && !e.is_end() && !e.is_deleted() {
                    continue;
                }
                index += 1;
                if let Some(parent) = parent.filter(|_| index <= 2) {
                    let (name, target) = if index == 1 {
                        (*b".          ", first)
                    } else {
                        (*b"..         ", parent)
                    };
                    // `fatfs` names the root by its cluster rather than as 0.
                    let root_named = index == 2 && parent == 0 && e.cluster == found.root_cluster;
                    if e.name != name
                        || e.attr & attr::DIR == 0
                        || (e.cluster != target && !root_named)
                    {
                        findings.bad.push(format!("{what}: broken dot entry {index}: {e:?}"));
                    }
                    continue;
                }
                if e.is_end() {
                    break 'dir;
                }
                if e.is_deleted() || e.is_long_name() || e.attr & attr::VOLUME != 0 {
                    continue;
                }
                let name = String::from_utf8_lossy(&e.name).into_owned();
                let child = format!("{path}/{}", name.trim_end());
                if e.attr & attr::DIR != 0 {
                    // A subdirectory of the root names the root as cluster 0.
                    let me = if parent.is_some() { first } else { 0 };
                    walk_dir(card, found, fat, e.cluster, Some(me), &child, owned, findings);
                    continue;
                }
                if let Some(number) = parse_log_name(&e.name).filter(|_| path == "/LOG") {
                    findings.log.push((number, e.cluster, e.size));
                }
                if e.cluster == 0 {
                    if e.size != 0 {
                        findings.bad.push(format!("{child}: {} bytes and no cluster", e.size));
                    }
                    continue;
                }
                let file = chain(fat, found, e.cluster, &child, &mut findings.bad);
                claim(owned, &file, &child, &mut findings.bad);
                let need = e.size.div_ceil(per) as usize;
                if file.len() < need {
                    findings.bad.push(format!("{child}: {} bytes past its chain", e.size));
                } else if file.len() > need {
                    findings.long += 1;
                }
            }
        }
    }
}

/// The first `size` bytes of the chain from `first`, through FAT1.
fn chain_bytes(card: &Card, found: &Volume, first: u32, size: u32) -> (Vec<u8>, Vec<u64>) {
    let mut bytes = Vec::new();
    let mut lbas = Vec::new();
    if first == 0 {
        return (bytes, lbas);
    }
    'chain: for cluster in chain_of(card, found, first) {
        for sector in 0..u32::from(found.sectors_per_cluster) {
            if bytes.len() >= size as usize {
                break 'chain;
            }
            let at = u64::from(lba(found, cluster) + sector);
            bytes.extend_from_slice(&card.block(at));
            lbas.push(at);
        }
    }
    bytes.truncate(size as usize);
    (bytes, lbas)
}

// Opening.

/// The steps of the first open on a freshly planned card: FSInfo, the root, `LOG/`'s cluster
/// marked, written and entered, the census, the file's entry, FSInfo again.
fn fresh_open_steps(found: &Volume) -> Vec<Step> {
    let fsinfo = found.fsinfo_lba.expect("an FSInfo sector");
    let root = lba(found, 2);
    let log = lba(found, 3);
    let mut steps = vec![
        Step::Read(fsinfo),
        Step::Read(root),
        Step::Read(found.fat_start),
        Step::Write(found.fat_start),
        Step::Write(found.fat_start + found.fat_blocks),
    ];
    steps.extend((0..u32::from(found.sectors_per_cluster)).map(|i| Step::Write(log + i)));
    steps.extend([
        Step::Write(root),
        Step::Read(log),
        Step::Write(log),
        Step::Read(fsinfo),
        Step::Write(fsinfo),
        Step::Done(Ok(())),
    ]);
    steps
}

#[test]
fn appender_opens_first_file_when_card_is_freshly_planned() {
    for (blocks, data_start, spc) in [(CARD_32GB, 24_576, 64), (CARD_512MIB, 16_384, 8)] {
        let (card, found) = planned(blocks);
        assert_eq!((found.data_start(), found.sectors_per_cluster), (data_start, spc));
        let mut app = mounted(&found, POLICY);
        let mut driver = Driver::new(card);
        assert_eq!(driver.open(&mut app), Ok(()));
        assert_eq!(driver.steps, fresh_open_steps(&found), "{blocks} blocks");
        assert_eq!((app.file(), app.size(), app.tail()), (Some(1), 0, &[][..]));
        assert!(!app.wants_rotate());

        assert_eq!(listed(&driver.card, &found, "LOG"), [("00000001.BIN".to_owned(), 0)]);
        let free = found.clusters - 2;
        assert_eq!(fsinfo_free(&driver.card, &found), free, "the root and LOG/ are used");
        assert_eq!(counted_free(&driver.card, &found), free);
        let next = le32(&driver.card.block(u64::from(found.fsinfo_lba.expect("FSInfo"))), NEXT_AT);
        assert_eq!(next, 4, "the hint moves past LOG/'s cluster");
        assert_eq!(app.free_mb(), Some(free / (2048 / u32::from(spc))));
        let findings = fsck(&driver.card, &found);
        findings.assert_allowed("opened");
        assert_eq!((findings.lost, findings.long, findings.fsinfo_wrong), (0, 0, false));
    }
}

#[test]
fn appender_pins_flush_steps_when_steady_growing_or_crossing_a_fat_sector() {
    let (mut card, found) = planned(CARD_512MIB);
    let fsinfo = found.fsinfo_lba.expect("FSInfo");
    // `LOG/` at 126, the file from 127, and 128 bad: the next cluster is 129, in FAT sector 1.
    put32(&mut card, fsinfo, NEXT_AT, 126);
    set_fat(&mut card, &found, &[(128, BAD)]);
    put32(&mut card, fsinfo, FREE_AT, found.clusters - 2);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    let (log, fat1, fat2) = (lba(&found, 126), found.fat_start, found.fat_start + found.fat_blocks);
    assert_eq!(driver.steps[5], Step::Write(log), "LOG/'s dot sector");

    // The first flush takes cluster 127.
    driver.flush(&mut app, &[0x11; 1000]).expect("flushes");
    let c127 = lba(&found, 127);
    let growth = [
        Step::Write(fat1),
        Step::Write(fat2),
        Step::Data { lba: c127, first: 0, count: 2 },
        Step::Write(log),
        Step::Read(fsinfo),
        Step::Write(fsinfo),
        Step::Done(Ok(())),
    ];
    assert_eq!(driver.steps, growth);
    assert_eq!((app.size(), app.tail().len()), (1000, 1000 - 512));

    // A steady flush stays in the cluster: the tail block rewritten, then the entry.
    driver.flush(&mut app, &[0x22; 3000]).expect("flushes");
    let steady =
        [Step::Data { lba: c127 + 1, first: 0, count: 7 }, Step::Write(log), Step::Done(Ok(()))];
    assert_eq!(driver.steps, steady);
    assert_eq!(app.size(), 4000);

    // Crossing into FAT sector 1 past a bad cluster: the data in two runs, the link back in
    // sector 0.
    driver.flush(&mut app, &[0x33; 1000]).expect("flushes");
    let crossing = [
        Step::Read(fat1 + 1),
        Step::Write(fat1 + 1),
        Step::Write(fat2 + 1),
        Step::Data { lba: c127 + 7, first: 0, count: 1 },
        Step::Data { lba: lba(&found, 129), first: 1, count: 2 },
        Step::Read(fat1),
        Step::Write(fat1),
        Step::Write(fat2),
        Step::Write(log),
        Step::Read(fsinfo),
        Step::Write(fsinfo),
        Step::Done(Ok(())),
    ];
    assert_eq!(driver.steps, crossing);
    assert_eq!(chain_of(&driver.card, &found, 127), [127, 129]);
    let want: Vec<u8> = [vec![0x11; 1000], vec![0x22; 3000], vec![0x33; 1000]].concat();
    assert_eq!(read(&driver.card, &found, "LOG/00000001.BIN"), want);
    assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
    fsck(&driver.card, &found).assert_allowed("crossed");
}

#[test]
fn appender_reads_back_every_record_when_flushed_in_random_pieces() {
    for (name, (card, found)) in
        [("512 B clusters", small()), ("planned 512 MiB", planned(CARD_512MIB))]
    {
        let mut rng = Rng(0x2545_F491_4F6C_DD1D);
        let mut app = mounted(&found, Policy { file_max: 64 << 20, keep_free_mb: 1 });
        let mut driver = Driver::new(card);
        driver.open(&mut app).expect("opens");
        let bytes = stream(&mut rng, 300_000);
        let mut at = 0;
        while at < bytes.len() {
            let n = (1 + rng.below(app.room())).min(bytes.len() - at);
            driver.flush(&mut app, &bytes[at..at + n]).expect("flushes");
            at += n;
            // The FSInfo count is the FAT's after every flush.
            assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
        }
        assert_eq!(app.size() as usize, bytes.len(), "{name}");
        assert_eq!(
            listed(&driver.card, &found, "LOG"),
            [("00000001.BIN".into(), bytes.len() as u64)]
        );
        let file = read(&driver.card, &found, "LOG/00000001.BIN");
        assert!(file == bytes, "{name}: the file reads back as written");
        let (good, bad) = decoded(&file);
        assert_eq!((good.len(), bad), (decoded(&bytes).0.len(), 0), "{name}");
        let findings = fsck(&driver.card, &found);
        findings.assert_allowed(name);
        assert_eq!((findings.lost, findings.long, findings.fats_differ), (0, 0, 0), "{name}");
    }
}

#[test]
fn appender_numbers_past_highest_when_log_holds_foreign_files() {
    let (mut card, found) = small();
    let files: [(&str, &[u8]); 5] = [
        ("LOG/00000003.BIN", b"three"),
        ("LOG/00000010.BIN", &[0xAB; 1500]),
        ("LOG/FOO.TXT", b"foo"),
        ("LOG/a long name for a file.txt", b"long"),
        ("LOG/12345678.TXT", b"not ours"),
    ];
    create(&mut card, &found, &["LOG"], &files);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    assert_eq!(app.file(), Some(11));
    driver.flush(&mut app, b"eleven").expect("flushes");
    for (path, bytes) in files {
        assert_eq!(read(&driver.card, &found, path), bytes, "{path} is untouched");
    }
    assert_eq!(read(&driver.card, &found, "LOG/00000011.BIN"), b"eleven");
    fsck(&driver.card, &found).assert_allowed("numbered");
}

#[test]
fn appender_counts_free_space_when_fsinfo_count_is_unknown() {
    let (mut card, found) = planned(CARD_512MIB);
    let fsinfo = found.fsinfo_lba.expect("FSInfo");
    put32(&mut card, fsinfo, FREE_AT, u32::MAX);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    // 129,026 entries in 1,009 sectors: 31 runs of 32 and one of 17.
    let runs: Vec<Step> = (0..32)
        .map(|i| Step::ReadRun {
            lba: found.fat_start + 32 * i,
            count: if i < 31 { 32 } else { 17 },
        })
        .collect();
    assert_eq!(driver.steps[1..33], runs[..]);
    assert_eq!(fsinfo_free(&driver.card, &found), found.clusters - 2);
    assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
    driver.flush(&mut app, &[7; 9000]).expect("flushes");
    assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
}

#[test]
fn appender_scans_at_most_one_sector_when_counting_a_large_card() {
    let (mut card, found) = planned(CARD_32GB);
    put32(&mut card, found.fsinfo_lba.expect("FSInfo"), FREE_AT, u32::MAX);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    // The driver checks every call against `SCAN_MAX`.
    driver.open(&mut app).expect("opens");
    let runs = driver.steps.iter().filter(|s| matches!(s, Step::ReadRun { .. })).count();
    assert_eq!(runs, (found.clusters as usize + 2).div_ceil(128).div_ceil(32));
    assert_eq!(driver.max_scanned, SCAN_MAX, "a FAT sector is the most a call takes");
    assert_eq!(app.free_mb(), Some((found.clusters - 2) / 32));
}

#[test]
fn appender_keeps_fsinfo_equal_to_fatfs_count_when_rotating_and_deleting() {
    let (mut card, found) = small();
    fill(&mut card, &found, 2048 + 60);
    let mut app = mounted(&found, Policy { file_max: 4000, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    let mut rng = Rng(7);
    driver.open(&mut app).expect("opens");
    for _ in 0..30 {
        let len = 1 + rng.below(3000);
        let bytes = stream(&mut rng, len);
        let bytes = &bytes[..bytes.len().min(app.room())];
        driver.flush(&mut app, bytes).expect("flushes");
        assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
        if app.wants_rotate() {
            driver.open(&mut app).expect("opens");
            assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
        }
    }
    assert!(app.file() > Some(5), "rotated: {:?}", app.file());
    fsck(&driver.card, &found).assert_allowed("rotated");
}

#[test]
fn appender_deletes_oldest_when_free_space_falls_under_reserve() {
    let (mut card, found) = small();
    // 1 MiB of 512-byte clusters is the reserve, and 60 clusters spare above it.
    fill(&mut card, &found, 2048 + 60);
    let mut app = mounted(&found, Policy { file_max: 6000, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    let filler = read(&driver.card, &found, "FILLER.BIN").len();
    let mut chains = HashMap::new();
    while app.file() != Some(8) {
        let number = app.file().expect("a file");
        driver.flush(&mut app, &[number as u8; 3000]).expect("flushes");
        if app.wants_rotate() {
            let log = fsck(&driver.card, &found).log;
            let (_, first, _) = *log.iter().find(|f| f.0 == number).expect("listed");
            chains.insert(number, chain_of(&driver.card, &found, first));
            driver.open(&mut app).expect("opens");
        }
    }
    // Five files of 12 clusters overrun the 59 spare: the oldest went to keep the reserve.
    driver.flush(&mut app, &[8; 3000]).expect("flushes");
    let names: Vec<String> = listed(&driver.card, &found, "LOG").into_iter().map(|f| f.0).collect();
    assert!(!names.contains(&"00000001.BIN".to_owned()), "{names:?}");
    let fat = fat_table(&driver.card, &found, 0);
    for cluster in &chains[&1] {
        assert!(is_free(fat[*cluster as usize]), "cluster {cluster} of file 1 is freed");
    }
    for name in &names {
        let number: u32 = name[..8].parse().expect("ours");
        let want = driver.contents[&number].clone();
        assert_eq!(read(&driver.card, &found, &format!("LOG/{name}")), want, "{name}");
    }
    assert_eq!(read(&driver.card, &found, "FILLER.BIN").len(), filler, "the stranger's file");
    assert!(counted_free(&driver.card, &found) >= 2048, "the reserve holds");
    assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
    fsck(&driver.card, &found).assert_allowed("deleted");
}

#[test]
fn appender_deletes_long_name_entries_when_oldest_file_carries_them() {
    let (mut card, found) = small();
    // `fatfs` gives each name long-name entries, ours included.
    let files: [(&str, &[u8]); 2] =
        [("LOG/00000001.BIN", &[1; 3000]), ("LOG/00000002.BIN", &[2; 3000])];
    create(&mut card, &found, &["LOG"], &files);
    let log = {
        let root = card.block(u64::from(lba(&found, found.root_cluster)));
        (0..16).find_map(|i| dir_entry(&root, i).filter(|e| e.name == fat::LOG_DIR)).expect("LOG")
    };
    let log_lba = u64::from(lba(&found, log.cluster));
    let sector = card.block(log_lba);
    let short = (0..16)
        .find(|&i| dir_entry(&sector, i).and_then(|e| parse_log_name(&e.name)) == Some(1))
        .expect("file 1 in LOG/'s first sector");
    let checksum = fat::lfn_checksum(&dir_entry(&sector, short).expect("an entry").name);
    let long: Vec<usize> = (0..short)
        .rev()
        .take_while(|&i| {
            dir_entry(&sector, i).is_some_and(|e| e.is_long_name())
                && sector[i * 32 + fat::LFN_CHECKSUM] == checksum
        })
        .collect();
    assert!(!long.is_empty(), "file 1 carries long-name entries in its sector");
    // Four clusters spare above the reserve: file 3's first flush deletes file 1, and only it.
    fill(&mut card, &found, 2048 + 4);

    let mut app = mounted(&found, Policy { file_max: 64 << 20, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    assert_eq!(app.file(), Some(3));
    driver.flush(&mut app, &[3; 3000]).expect("flushes");
    let sector = driver.card.block(log_lba);
    assert_eq!(sector[short * 32], DELETED, "file 1's entry");
    for i in long {
        assert_eq!(sector[i * 32], DELETED, "file 1's long-name entry {i}");
    }
    let names: Vec<String> = listed(&driver.card, &found, "LOG").into_iter().map(|f| f.0).collect();
    assert_eq!(names, ["00000002.BIN", "00000003.BIN"]);
    assert_eq!(read(&driver.card, &found, "LOG/00000002.BIN"), [2; 3000]);
    assert_eq!(read(&driver.card, &found, "LOG/00000003.BIN"), [3; 3000]);
    fsck(&driver.card, &found).assert_allowed("long names");
}

#[test]
fn appender_keeps_deleted_entry_when_later_flush_rewrites_its_sector() {
    let (mut card, found) = small();
    fill(&mut card, &found, 2048 + 40);
    let mut app = mounted(&found, Policy { file_max: 4000, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    let first_of =
        |card: &Card, n: u32| fsck(card, &found).log.iter().find(|f| f.0 == n).map(|f| f.1);
    let mut doomed = None;
    loop {
        driver.flush(&mut app, &[0x5A; 2000]).expect("flushes");
        if doomed.is_none() && app.file() == Some(1) && app.size() >= 4000 {
            let first = first_of(&driver.card, 1).expect("file 1");
            doomed = Some(chain_of(&driver.card, &found, first));
        }
        if first_of(&driver.card, 1).is_none() {
            break;
        }
        if app.wants_rotate() {
            driver.open(&mut app).expect("opens");
        }
    }
    let open = app.file().expect("a file");
    // File 1's entry is entry 2 of `LOG/`'s first sector, beside the open file's.
    let log_lba = {
        let root = driver.card.block(u64::from(lba(&found, found.root_cluster)));
        let log = (0..16).find_map(|i| dir_entry(&root, i).filter(|e| e.name == fat::LOG_DIR));
        lba(&found, log.expect("LOG").cluster)
    };
    let sector = driver.card.block(u64::from(log_lba));
    let shared =
        (0..16).any(|i| dir_entry(&sector, i).and_then(|e| parse_log_name(&e.name)) == Some(open));
    assert!(shared, "the open file's entry shares the sector");
    for _ in 0..3 {
        driver.flush(&mut app, &[0x6B; 700]).expect("flushes");
        let sector = driver.card.block(u64::from(log_lba));
        assert_eq!(sector[2 * 32], DELETED, "file 1's entry stays deleted");
        let fat = fat_table(&driver.card, &found, 0);
        for cluster in doomed.as_ref().expect("file 1's chain") {
            assert!(is_free(fat[*cluster as usize]), "cluster {cluster} stays free");
        }
    }
    fsck(&driver.card, &found).assert_allowed("coherent");
}

#[test]
fn appender_reuses_oldest_slot_when_log_dir_is_full() {
    let (mut card, found) = small();
    // `fatfs` gives every name a long-name entry, `.` and `..` included: 14 files fill two
    // clusters of 16 entries.
    let names: Vec<String> = (1..=14).map(|n| format!("LOG/{n:08}.BIN")).collect();
    let contents = vec![0xC3; 700];
    let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &contents[..])).collect();
    create(&mut card, &found, &["LOG"], &files);
    let log = {
        let root = card.block(u64::from(lba(&found, found.root_cluster)));
        (0..16).find_map(|i| dir_entry(&root, i).filter(|e| e.name == fat::LOG_DIR)).expect("LOG")
    };
    let log_chain = chain_of(&card, &found, log.cluster);
    let sectors: Vec<[u8; BLOCK]> =
        log_chain.iter().map(|&c| card.block(u64::from(lba(&found, c)))).collect();
    let used = |sector: &[u8; BLOCK]| {
        (0..16).all(|i| dir_entry(sector, i).is_some_and(|e| !e.is_end() && !e.is_deleted()))
    };
    assert!(sectors.iter().all(used), "LOG/ is full");
    let (_, first, _) = *fsck(&card, &found).log.iter().find(|f| f.0 == 1).expect("file 1");
    let doomed = chain_of(&card, &found, first);

    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    assert_eq!(app.file(), Some(15));
    assert_eq!(chain_of(&driver.card, &found, log.cluster), log_chain, "LOG/ never grows");
    let fat = fat_table(&driver.card, &found, 0);
    assert!(doomed.iter().all(|&c| is_free(fat[c as usize])), "file 1's clusters are freed");
    let mut listed: Vec<String> =
        listed(&driver.card, &found, "LOG").into_iter().map(|f| f.0).collect();
    listed.sort();
    let want: Vec<String> = (2..=15).map(|n| format!("{n:08}.BIN")).collect();
    assert_eq!(listed, want);
    driver.flush(&mut app, b"fifteen").expect("flushes");
    assert_eq!(read(&driver.card, &found, "LOG/00000015.BIN"), b"fifteen");
    assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
    fsck(&driver.card, &found).assert_allowed("reused");
}

#[test]
fn appender_writes_log_when_fatfs_formatted_with_4_8_or_16_kib_clusters() {
    for (sectors, cluster) in [(600_000, 4096), (1_200_000, 8192), (2_400_000, 16_384)] {
        let (card, found) = by_fatfs(sectors, cluster, 2);
        assert_eq!(cluster_bytes(&found), cluster);
        let mut app = mounted(&found, POLICY);
        let mut driver = Driver::new(card);
        let mut rng = Rng(u64::from(cluster));
        driver.open(&mut app).expect("opens");
        let bytes = stream(&mut rng, 100_000);
        flush_all(&mut driver, &mut app, &mut rng, &bytes);
        assert!(read(&driver.card, &found, "LOG/00000001.BIN") == bytes, "{cluster}");
        assert_eq!(fsinfo_free(&driver.card, &found), counted_free(&driver.card, &found));
        fsck(&driver.card, &found).assert_allowed("foreign");
    }
}

#[test]
fn appender_writes_log_when_volume_has_one_fat() {
    let (card, found) = by_fatfs(70_000, 512, 1);
    assert_eq!(found.fats, 1);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, &[9; 5000]).expect("flushes");
    // The new clusters' marks, then their links: one write each, to the one FAT.
    let fat = found.fat_start..found.fat_start + found.fat_blocks;
    let fat_writes =
        driver.steps.iter().filter(|s| matches!(s, Step::Write(at) if fat.contains(at)));
    assert_eq!(fat_writes.count(), 2);
    assert_eq!(read(&driver.card, &found, "LOG/00000001.BIN"), [9; 5000]);
    fsck(&driver.card, &found).assert_allowed("one FAT");
}

#[test]
fn appender_finds_root_when_moved_to_another_cluster() {
    let (mut card, found) = small();
    // The root's one cluster copied to 50, and the boot sector pointed there.
    let root = card.block(u64::from(lba(&found, 2)));
    card.put(u64::from(lba(&found, 50)), root);
    set_fat(&mut card, &found, &[(50, EOC), (2, 0)]);
    for boot in [0, 6] {
        put32(&mut card, boot, 44, 50);
    }
    let moved = volume(&card.block(0), 0).expect("a volume");
    assert_eq!(moved.root_cluster, 50);
    let mut app = mounted(&moved, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, b"moved").expect("flushes");
    assert_eq!(read(&driver.card, &moved, "LOG/00000001.BIN"), b"moved");
    fsck(&driver.card, &moved).assert_allowed("moved");
}

#[test]
fn appender_creates_log_when_root_spans_clusters() {
    let (mut card, found) = small();
    let names: Vec<String> = (0..20).map(|n| format!("ROOT{n:04}.TXT")).collect();
    let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"r"[..])).collect();
    create(&mut card, &found, &[], &files);
    assert!(chain_of(&card, &found, found.root_cluster).len() > 1, "the root grew");
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, b"spans").expect("flushes");
    assert_eq!(read(&driver.card, &found, "LOG/00000001.BIN"), b"spans");
    assert_eq!(listed(&driver.card, &found, "").len(), 20);
    // And found again in the root's second cluster.
    let mut again = mounted(&found, POLICY);
    driver.open(&mut again).expect("opens");
    assert_eq!(again.file(), Some(2));
    fsck(&driver.card, &found).assert_allowed("spans");
}

#[test]
fn appender_counts_free_space_when_boot_sector_names_no_fsinfo() {
    for field in [0u16, 0xFFFF] {
        let (mut card, found) = planned(CARD_512MIB);
        let mut boot = card.block(8192);
        boot[48..50].copy_from_slice(&field.to_le_bytes());
        card.put(8192, boot);
        let bare = volume(&boot, 8192).expect("a volume");
        assert_eq!(bare.fsinfo_lba, None);
        let mut app = mounted(&bare, POLICY);
        let mut driver = Driver::new(card);
        driver.open(&mut app).expect("opens");
        assert!(matches!(driver.steps[0], Step::ReadRun { .. }), "{field:#X}: counted");
        driver.flush(&mut app, &[1; 16_000]).expect("flushes");
        assert!(driver.writes.iter().all(|(at, _)| *at != 8193), "FSInfo is never written");
        // `fatfs` needs the FSInfo sector named to mount.
        let mut copy = driver.card.clone();
        copy.put(8192, named_fsinfo(&boot));
        assert_eq!(app.free_mb(), Some(counted_free(&copy, &found) / 256), "{field:#X}");
        assert_eq!(read(&copy, &found, "LOG/00000001.BIN"), [1; 16_000]);
    }
}

/// `boot` with its FSInfo field back at sector 1.
fn named_fsinfo(boot: &[u8; BLOCK]) -> [u8; BLOCK] {
    let mut boot = *boot;
    boot[48..50].copy_from_slice(&1u16.to_le_bytes());
    boot
}

#[test]
fn appender_refuses_volume_when_unmirrored_versioned_or_oddly_shaped() {
    let (_, found) = planned(CARD_512MIB);
    let mut app = Appender::new();
    let cases = [
        (Volume { ext_flags: 0x0081, ..found }, Refused::Unmirrored),
        (Volume { version: 0x0100, ..found }, Refused::Version(0x0100)),
        (Volume { fats: 3, ..found }, Refused::Fats(3)),
        (Volume { root_cluster: 1, ..found }, Refused::RootCluster(1)),
        (
            Volume { root_cluster: found.clusters + 2, ..found },
            Refused::RootCluster(found.clusters + 2),
        ),
    ];
    for (odd, why) in cases {
        assert_eq!(app.mount(&odd, POLICY), Err(why));
        app.open();
        assert_eq!(app.next(), Step::Done(Err(Fault::Misuse)), "{why:?}: nothing mounted");
    }
    // Mirroring on with an active-FAT number set is still mirrored.
    assert_eq!(app.mount(&Volume { ext_flags: 0x0001, ..found }, POLICY), Ok(()));
}

#[test]
fn appender_faults_corrupt_when_a_link_is_out_of_range() {
    // LOG's entry names a cluster past the data area.
    let (mut card, found) = small();
    root_entry(&mut card, &found, &DirEntry::dir(fat::LOG_DIR, found.clusters + 2));
    let mut app = mounted(&found, POLICY);
    assert_eq!(Driver::new(card).open(&mut app), Err(Fault::Corrupt));

    // The root's chain leads out of the data area.
    let (mut card, found) = small();
    let names: Vec<String> = (0..20).map(|n| format!("ROOT{n:04}.TXT")).collect();
    let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &b"r"[..])).collect();
    create(&mut card, &found, &[], &files);
    set_fat(&mut card, &found, &[(found.root_cluster, found.clusters + 9)]);
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    assert_eq!(driver.open(&mut app), Err(Fault::Corrupt));
    // The writer stays stopped.
    assert_eq!(driver.open(&mut app), Err(Fault::Corrupt));
    assert!(driver.writes.is_empty(), "nothing written");
}

#[test]
fn appender_faults_corrupt_when_deleting_a_looped_chain() {
    let (mut card, found) = small();
    let names: Vec<String> = (1..=14).map(|n| format!("LOG/{n:08}.BIN")).collect();
    let contents = vec![0xC3; 1500];
    let files: Vec<(&str, &[u8])> = names.iter().map(|n| (n.as_str(), &contents[..])).collect();
    create(&mut card, &found, &["LOG"], &files);
    let log = fsck(&card, &found).log;
    let (_, first, _) = *log.iter().find(|f| f.0 == 1).expect("file 1");
    let chain = chain_of(&card, &found, first);
    set_fat(&mut card, &found, &[(chain[2], chain[0])]);
    let mut app = mounted(&found, POLICY);
    assert_eq!(Driver::new(card).open(&mut app), Err(Fault::Corrupt));
}

#[test]
fn appender_faults_when_root_is_full_or_log_is_a_file() {
    let (mut card, found) = small();
    // The root's one sector filled with empty files; its chain ends there.
    let root = u64::from(lba(&found, found.root_cluster));
    for n in 0..16u8 {
        if dir_entry(&card.block(root), 15).is_some_and(|e| !e.is_end()) {
            break;
        }
        let mut name = *b"FULL000ATXT";
        name[7] += n;
        root_entry(&mut card, &found, &DirEntry::file(name, 0, 0));
    }
    assert_eq!(chain_of(&card, &found, found.root_cluster).len(), 1);
    let mut app = mounted(&found, POLICY);
    assert_eq!(Driver::new(card).open(&mut app), Err(Fault::RootFull));

    let (mut card, found) = small();
    root_entry(&mut card, &found, &DirEntry::file(fat::LOG_DIR, 0, 0));
    let mut app = mounted(&found, POLICY);
    assert_eq!(Driver::new(card).open(&mut app), Err(Fault::NotADirectory));
}

#[test]
fn appender_faults_numbers_when_highest_file_is_log_max() {
    let (mut card, found) = small();
    create(&mut card, &found, &["LOG"], &[("LOG/99999999.BIN", b"last")]);
    let mut app = mounted(&found, POLICY);
    assert_eq!(Driver::new(card).open(&mut app), Err(Fault::Numbers));
}

#[test]
fn appender_faults_full_when_only_open_file_is_left() {
    let (mut card, found) = small();
    fill(&mut card, &found, 2048 + 10);
    let mut app = mounted(&found, Policy { file_max: 64 << 20, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, &[1; 4000]).expect("eight clusters fit");
    assert_eq!(driver.flush(&mut app, &[2; 2000]), Err(Fault::Full));
    assert_eq!(driver.open(&mut app), Err(Fault::Full), "the writer stays stopped");
    assert_eq!(read(&driver.card, &found, "LOG/00000001.BIN"), [1; 4000]);
    fsck(&driver.card, &found).assert_allowed("full");
}

#[test]
fn appender_rotates_when_file_reaches_max() {
    let (card, found) = small();
    let mut app = mounted(&found, Policy { file_max: 10_000, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, &[1; 9000]).expect("flushes");
    assert!(!app.wants_rotate());
    driver.flush(&mut app, &[1; 1000]).expect("flushes");
    assert!(app.wants_rotate());
    driver.open(&mut app).expect("opens");
    // LOG/ is remembered: the rotation reads only its census sector.
    assert_eq!(driver.steps[0], Step::Read(lba(&found, 3)));
    assert_eq!((app.file(), app.size(), app.tail().len()), (Some(2), 0, 0));
    driver.flush(&mut app, &[2; 100]).expect("flushes");
    let files = listed(&driver.card, &found, "LOG");
    assert_eq!(files, [("00000001.BIN".into(), 10_000), ("00000002.BIN".into(), 100)]);
}

#[test]
fn appender_opens_next_file_when_reset_after_a_failed_transfer() {
    let (card, found) = small();
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, &[1; 700]).expect("flushes");
    // The next flush's data write fails, after its new clusters were marked.
    app.flush(app.tail().len() + 3000, &[0; BLOCK]);
    loop {
        match app.next() {
            Step::Read(at) => app.fed(&driver.card.block(u64::from(at))),
            Step::Write(at) => driver.card.put(u64::from(at), *app.block()),
            Step::Data { .. } => break,
            step => panic!("{step:?} before the data"),
        }
    }
    app.reset();
    assert_eq!((app.file(), app.free_mb()), (None, None));
    app.mount(&found, POLICY).expect("mounts");
    driver.open(&mut app).expect("opens");
    assert_eq!(app.file(), Some(2));
    driver.flush(&mut app, &[2; 900]).expect("flushes");
    assert_eq!(read(&driver.card, &found, "LOG/00000001.BIN"), [1; 700]);
    assert_eq!(read(&driver.card, &found, "LOG/00000002.BIN"), [2; 900]);
    let findings = fsck(&driver.card, &found);
    findings.assert_allowed("reset");
    assert!(findings.lost > 0, "the failed flush's clusters are lost, not reused");
}

#[test]
fn appender_stops_with_misuse_when_driver_breaks_the_contract() {
    let (card, found) = small();
    let mut app = mounted(&found, POLICY);
    app.flush(10, &[0; BLOCK]);
    assert_eq!(app.next(), Step::Done(Err(Fault::Misuse)), "no file open");

    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    app.flush(CHUNK_BLOCKS * BLOCK + 1, &[0; BLOCK]);
    assert_eq!(app.next(), Step::Done(Err(Fault::Misuse)), "more than the staging buffer");

    let mut app = mounted(&found, POLICY);
    driver.open(&mut app).expect("opens");
    app.flush(600, &[0; BLOCK]);
    app.open();
    assert_eq!(app.next(), Step::Done(Err(Fault::Misuse)), "a task begun mid-task");
}

#[test]
fn appender_never_panics_when_fed_random_blocks() {
    let (_, found) = planned(CARD_512MIB);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut app = mounted(&found, POLICY);
    app.open();
    for round in 0..50_000 {
        let step = app.next();
        assert!(app.scanned() <= SCAN_MAX);
        match step {
            Step::Read(_) => {
                let mut block = rng.block();
                // Some blocks look like directory sectors naming LOG, or FSInfo sectors.
                if round % 3 == 0 {
                    block[..11].copy_from_slice(&fat::LOG_DIR);
                    block[11] = [attr::DIR, attr::ARCHIVE, 0x0F, 0][rng.below(4)];
                    block[64] = 0;
                }
                app.fed(&block);
            }
            Step::ReadRun { count, .. } => {
                for _ in 0..count {
                    app.fed(&rng.block());
                }
            }
            Step::Write(_) | Step::Data { .. } => {
                if rng.below(20) == 0 {
                    app.fed(&rng.block());
                }
            }
            Step::Done(_) => {
                if rng.below(4) == 0 || app.file().is_none() {
                    app.mount(&found, POLICY).expect("mounts");
                    app.open();
                } else {
                    let len = app.tail().len() + rng.below(app.room() + 1);
                    app.flush(len, &rng.block());
                }
            }
        }
        assert!(app.scanned() <= SCAN_MAX);
    }
}

// Power cuts.

/// When each task of a scenario ended: the writes made by then, and each open file's size.
struct Mark {
    writes: usize,
    file: u32,
    size: u32,
}

#[test]
fn appender_leaves_only_allowed_damage_when_power_is_cut_after_any_write() {
    let (mut card, found) = small();
    fill(&mut card, &found, 2048 + 50);
    let start = card.clone();
    let mut app = mounted(&found, Policy { file_max: 3000, keep_free_mb: 1 });
    let mut driver = Driver::new(card);
    let mut rng = Rng(0xC0FF_EE00_D15E_A5E5);
    let mut marks = Vec::new();
    driver.open(&mut app).expect("opens");
    let mut deleted = false;
    for _ in 0..24 {
        let len = 300 + rng.below(2500);
        let bytes = stream(&mut rng, len);
        let bytes = &bytes[..bytes.len().min(app.room())];
        driver.flush(&mut app, bytes).expect("flushes");
        let file = app.file().expect("a file");
        marks.push(Mark { writes: driver.writes.len(), file, size: app.size() });
        if app.wants_rotate() {
            driver.open(&mut app).expect("opens");
        }
        deleted |= !fsck(&driver.card, &found).log.iter().any(|f| f.0 == 1);
    }
    assert!(deleted, "the scenario deletes a file");
    assert!(driver.writes.len() > 150, "{} writes", driver.writes.len());

    let mut cut = start;
    let mut seen = Findings::default();
    for k in 0..=driver.writes.len() {
        let committed = |file: u32| {
            marks.iter().filter(|m| m.writes <= k && m.file == file).map(|m| m.size).max()
        };
        let findings =
            check_cut(&cut, &found, &driver.contents, None, &committed, &format!("cut at {k}"));
        seen.lost = seen.lost.max(findings.lost);
        seen.long = seen.long.max(findings.long);
        seen.fats_differ = seen.fats_differ.max(findings.fats_differ);
        seen.fsinfo_wrong |= findings.fsinfo_wrong;
        if let Some(&(at, new)) = driver.writes.get(k) {
            let old = cut.block(at);
            for (half, torn) in [(0, [&new[..256], &old[256..]]), (1, [&old[..256], &new[256..]])] {
                let mut torn_card = cut.clone();
                torn_card.put(at, torn.concat().try_into().expect("a block"));
                let what = format!("write {k} to {at} torn, half {half} new");
                check_cut(&torn_card, &found, &driver.contents, Some(at), &committed, &what);
            }
            cut.put(at, new);
        }
    }
    // Every kind of allowed damage turns up at some cut, so the replay reaches each.
    assert!(seen.lost > 0 && seen.long > 0 && seen.fats_differ == 1 && seen.fsinfo_wrong);
}

/// The volume after a cut holds only allowed damage, and each log file a prefix of what was
/// flushed to it: at least its last committed size, every byte as written but in a torn block.
fn check_cut(
    card: &Card,
    found: &Volume,
    contents: &HashMap<u32, Vec<u8>>,
    torn: Option<u64>,
    committed: &dyn Fn(u32) -> Option<u32>,
    what: &str,
) -> Findings {
    let findings = fsck(card, found);
    findings.assert_allowed(what);
    for &(number, first, size) in &findings.log {
        let flushed = contents.get(&number).map_or(&[][..], Vec::as_slice);
        assert!(size as usize <= flushed.len(), "{what}: file {number} is {size} bytes");
        assert!(size >= committed(number).unwrap_or(0), "{what}: file {number} lost a flush");
        let (bytes, lbas) = chain_bytes(card, found, first, size);
        for (i, (got, want)) in bytes.chunks(BLOCK).zip(flushed.chunks(BLOCK)).enumerate() {
            let want = &want[..got.len()];
            assert!(got == want || torn == Some(lbas[i]), "{what}: file {number} block {i}");
        }
        let (good, _) = decoded(&bytes);
        let (all, _) = decoded(&flushed[..size as usize]);
        let mut rest = all.iter();
        for record in &good {
            assert!(rest.any(|r| r == record), "{what}: file {number} holds a record out of order");
        }
    }
    findings
}

#[test]
fn appender_fits_in_2_5_kib_when_held_in_a_static() {
    // Four sectors, the clusters a flush allocates, and the bookkeeping.
    assert!(std::mem::size_of::<Appender>() <= 2560, "{}", std::mem::size_of::<Appender>());
}

#[test]
fn fsck_reports_damage_when_entries_cross_link_reach_free_clusters_or_outgrow_chains() {
    let (card, found) = small();
    let mut app = mounted(&found, POLICY);
    let mut driver = Driver::new(card);
    driver.open(&mut app).expect("opens");
    driver.flush(&mut app, &[1; 3000]).expect("flushes");
    let (_, first, _) = fsck(&driver.card, &found).log[0];
    let log_lba = u64::from(lba(&found, 3));
    let clean = driver.card.clone();
    fsck(&clean, &found).assert_allowed("clean");

    let damage = |entry: DirEntry| {
        let mut card = clean.clone();
        let mut sector = card.block(log_lba);
        assert!(put_dir_entry(&mut sector, 3, &entry));
        card.put(log_lba, sector);
        fsck(&card, &found).bad
    };
    let cross = damage(DirEntry::file(*b"OTHER   BIN", first, 512));
    assert!(cross.iter().any(|b| b.contains("cross-linked")), "{cross:?}");
    let free = damage(DirEntry::file(*b"OTHER   BIN", 500, 512));
    assert!(free.iter().any(|b| b.contains("reaches free")), "{free:?}");
    let short = damage(DirEntry::file(*b"OTHER   BIN", 0, 1));
    assert!(short.iter().any(|b| b.contains("no cluster")), "{short:?}");
    let long_size = damage(DirEntry::file(*b"00000001BIN", first, 10_000));
    assert!(long_size.iter().any(|b| b.contains("past its chain")), "{long_size:?}");

    let mut differ = clean.clone();
    let fat2 = u64::from(found.fat_start + found.fat_blocks);
    for sector in [0, 1] {
        differ.put(fat2 + sector, [0xEE; BLOCK]);
    }
    assert_eq!(fsck(&differ, &found).fats_differ, 2);
}
