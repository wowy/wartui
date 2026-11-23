//! The SD log's writer: it appends to one file at a time, `LOG/NNNNNNNN.BIN` on a FAT32 volume,
//! as a state machine that touches no card. The bridge asks [`Appender::next`] for a [`Step`],
//! carries it out, and hands every block it reads back through [`Appender::fed`]: the firmware
//! owns the card and its DMA buffers, and the FAT32 decisions are tested on the host against a
//! reader written by someone else.
//!
//! **A failed transfer ends the task.** The driver calls [`Appender::reset`] and mounts again.
//! A write that failed may have landed or not, and resuming from a guess is how a writer
//! cross-links a volume.
//!
//! **Each call does at most one sector's scanning**: 128 FAT entries or 16 directory entries
//! ([`SCAN_MAX`], reported by [`Appender::scanned`]). The P4 runs every future on one core, and
//! the firmware yields between steps, so counting a 64 GB card's free space, deleting a 64 MiB
//! file or reading a full directory is a run of short steps, never one long stretch.
//!
//! **One cache per kind.** One FAT sector and one directory sector are held, and every FAT or
//! directory write goes through its cache. The oldest file's entry often shares a sector with the
//! open file's. A delete made from a second copy of that sector would be undone by the next
//! flush writing the first, leaving an entry whose clusters are free: a cross-link once they are
//! reused. The directory cache writes through; the FAT cache is written to every FAT before
//! another FAT sector is read into it.
//!
//! **Writes are ordered so a cut leaves only harmless damage:**
//!
//! - a new cluster is marked end-of-chain on the card before its data and before anything links
//!   to it, so no torn FAT sector holds a link without its target's mark;
//! - data lands before the FAT link or the directory entry that reaches it;
//! - each FAT sector goes to the first FAT, then the second;
//! - a directory entry's size grows only after its chain covers it;
//! - a delete marks the entry `0xE5` before it frees the chain.
//!
//! What a cut, or a torn block, leaves at worst:
//!
//! | Cut during           | Left on the card                                     | Lost          |
//! | -------------------- | ---------------------------------------------------- | ------------- |
//! | marking new clusters | clusters marked used that nothing reaches            | nothing       |
//! | the data             | bytes past the file's size                           | this flush    |
//! | the tail block       | the same, and that block torn                        | + 511 bytes   |
//! | the FAT links        | a chain past its size; the FATs differ in a sector   | this flush    |
//! | the directory entry  | the old entry or the new one                         | this flush    |
//! | FSInfo               | a wrong free count                                   | nothing       |
//! | a delete             | an `0xE5` entry; clusters marked used, unreached     | nothing       |
//! | creating `LOG/`      | a cluster marked used that nothing reaches           | nothing       |
//!
//! So a cut leaves lost clusters, a wrong FSInfo count, FATs differing in one sector, or a chain
//! longer than its file: what `fsck` repairs and every reader tolerates. It never leaves a
//! cross-link, an entry reaching a free cluster, or a size past the chain. The loss is the flush
//! in progress, and up to 511 bytes before it when the tail block tears. A torn directory sector
//! is the one hazard outside the table: a card that tears a block may garble the other entries
//! in it.
//!
//! **The staged blocks start at the file's last partial block.** Staged block 0 is file offset
//! `size - tail().len()`, rewritten in place, so the file grows without a gap and the bridge
//! needs no read-modify-write of its own.
//!
//! **Free space is FSInfo's count when it has one,** and otherwise counted once, the first FAT
//! read in [`CHUNK_BLOCKS`] runs. Clusters are taken forward from FSInfo's next-free hint, so a
//! fresh card costs one FAT read per 128 clusters of log. When a flush would leave less free than
//! [`Policy::keep_free_mb`], capped at an eighth of the volume, the oldest log file is deleted
//! first, but only while deleting all the log's files but the open one would restore the reserve.
//! Each counts at its size, which undercounts a chain a cut left longer. When other files took
//! the space, deleting the log's gains nothing and loses its history, so the flush stops with
//! [`Fault::Full`] and keeps every file. The root and `LOG/` are never extended: a full `LOG/`
//! reuses the oldest file's entry.
//!
//! **A stranger's volume is written when nothing in it surprises the writer:** any cluster size,
//! a root anywhere, no FSInfo, one FAT. Mirroring off, a FAT32 version past 0 or more than two
//! FATs is refused at [`Appender::mount`]. A link out of range, a loop, or a free cluster inside a
//! chain stops the writer with [`Fault::Corrupt`]; it repairs nothing. FAT entries keep their top
//! four bits, and FAT\[1\]'s clean-shutdown bit is left alone. A log file another writer gave
//! long-name entries loses them with its short entry, in the same write, when they share its
//! sector; any in the sector before stay behind as orphans, which readers drop because their
//! checksum matches no entry.

use super::fat::{self, DirEntry, attr};
use super::inspect::Volume;
use super::{BLOCK, fsinfo, fsinfo_signed, le32, mib, put32};

/// The blocks the bridge stages for one flush: its staging buffer and its longest transfer.
pub const CHUNK_BLOCKS: usize = 32;

/// The bytes the staged blocks hold.
const CHUNK_BYTES: usize = CHUNK_BLOCKS * BLOCK;

/// The most scanning one call does: a sector of FAT entries, each counting 1.
pub const SCAN_MAX: u16 = fat::ENTRIES_PER_SECTOR as u16;

/// A directory entry's share of [`SCAN_MAX`]: a sector of either is the same work.
const DIR_ENTRY_COST: u16 = SCAN_MAX / fat::DIR_ENTRIES_PER_SECTOR as u16;

/// The extended-flags bit that turns FAT mirroring off.
const UNMIRRORED: u16 = 0x0080;

/// What a zeroing write writes.
static ZEROS: [u8; BLOCK] = [0; BLOCK];

/// The volume before [`Appender::mount`].
const NO_VOLUME: Volume = Volume {
    start: 0,
    fsinfo_lba: None,
    clusters: 0,
    sectors_per_cluster: 1,
    total_blocks: 0,
    fat_start: 0,
    fat_blocks: 0,
    fats: 0,
    root_cluster: 0,
    ext_flags: 0,
    version: 0,
};

/// How large a file grows and how much of the card the log leaves free.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    /// The size at which [`Appender::wants_rotate`] asks for the next file, in bytes.
    pub file_max: u32,
    /// The free space a flush keeps, in MiB, capped at an eighth of the volume.
    pub keep_free_mb: u32,
}

/// One thing for the bridge to do. Every step but [`Step::Done`] completes before the next
/// [`Appender::next`], and that call means it succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Read the block at this LBA, then pass it to [`Appender::fed`].
    Read(u32),
    /// Read `count` blocks from `lba` into the staging buffer, then pass each to
    /// [`Appender::fed`] in order. Only while nothing is staged.
    ReadRun {
        /// The first block.
        lba: u32,
        /// How many, at most [`CHUNK_BLOCKS`].
        count: u16,
    },
    /// Write [`Appender::block`] at this LBA.
    Write(u32),
    /// Write staged blocks `first..first + count` to `lba` onward.
    Data {
        /// Where the first of them goes.
        lba: u32,
        /// The first staged block.
        first: u16,
        /// How many, at most [`CHUNK_BLOCKS`].
        count: u16,
    },
    /// The task is over.
    Done(Result<(), Fault>),
}

/// Why [`Appender::mount`] will not write a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Its FAT32 version, which no writer but its own understands past 0.
    Version(u16),
    /// One FAT is active and the others are not kept in step.
    Unmirrored,
    /// It keeps this many FATs, none or more than two.
    Fats(u8),
    /// Its boot sector names this root cluster, outside the data area.
    RootCluster(u32),
}

/// Why a task stopped. Each stops the writer until it is mounted again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// A link out of range, a loop, a free cluster in a chain, or a log entry not where the
    /// writer left it. Nothing is repaired.
    Corrupt,
    /// `LOG` is missing and the root has no free entry; the root is never extended.
    RootFull,
    /// The root's `LOG` is a file.
    NotADirectory,
    /// `LOG/` has no free entry and none of the log's files to delete; it is never extended.
    LogDirFull,
    /// The card is short of clusters, and deleting the log's files but the open one cannot
    /// restore the reserve.
    Full,
    /// The next file number is past [`fat::LOG_MAX`].
    Numbers,
    /// The driver broke the contract: a task before [`Appender::mount`], a task begun before the
    /// last one was done, a flush with no open file, or a staged length outside
    /// `tail().len()..=CHUNK_BLOCKS * 512`.
    Misuse,
}

/// The task in progress, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    // open
    Fsinfo,
    FsinfoParse,
    Count,
    Locate,
    Walk,
    RootDone,
    CreateAlloc,
    CreateDot,
    CreateZeros,
    CreateEntry,
    Decide,
    Entry,
    // flush
    Reserve,
    Alloc,
    Data,
    Link,
    Size,
    // delete the oldest, inside either
    Forget,
    Free,
    Freed,
    // the end of either
    Finish,
    FinishWrite,
}

/// A directory entry's place: its sector and its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Slot {
    lba: u32,
    index: u8,
}

/// A log file, by number, and its entry.
#[derive(Debug, Clone, Copy)]
struct Named {
    number: u32,
    slot: Slot,
}

/// The file being appended to.
#[derive(Debug, Clone, Copy)]
struct Open {
    number: u32,
    slot: Slot,
    first: u32,
    last: u32,
    /// The clusters in its chain.
    chain: u32,
    size: u32,
}

/// Which directory a walk reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Census,
}

/// A walk through a directory's chain, a sector at a time.
#[derive(Debug, Clone, Copy)]
struct Walk {
    kind: Kind,
    cluster: u32,
    sector: u8,
    /// The links followed, bounded by the volume's clusters.
    steps: u32,
    /// Whether the current sector was scanned.
    scanned: bool,
    done: bool,
}

/// What the root holds.
#[derive(Debug, Clone, Copy)]
struct Root {
    log: Option<u32>,
    /// `LOG` is a file.
    file: bool,
    free: Option<Slot>,
}

/// What `LOG/` holds.
#[derive(Debug, Clone, Copy)]
struct Census {
    highest: Option<u32>,
    /// The lowest-numbered log file but the open one.
    oldest: Option<Named>,
    /// The clusters the log's files but the open one hold, by their sizes: at most what
    /// deleting them frees.
    reclaimable: u32,
    free: Option<Slot>,
}

/// A flush in progress.
#[derive(Debug, Clone, Copy)]
struct Plan {
    /// The file block staged block 0 is.
    base: u32,
    blocks: u16,
    /// The file's size after it.
    size: u32,
    /// The clusters it adds.
    want: u8,
    /// The next staged block to write, then the next link to make.
    next: u16,
}

/// The one FAT sector held, counted from the first FAT's start.
#[derive(Debug, Clone)]
struct FatCache {
    sector: Option<u32>,
    data: [u8; BLOCK],
    dirty: bool,
    /// The FATs a dirty sector has been written to.
    written: u8,
}

/// The one directory sector held, by LBA. It writes through.
#[derive(Debug, Clone)]
struct DirCache {
    lba: Option<u32>,
    data: [u8; BLOCK],
}

/// What [`Appender::fed`] takes the next block for.
#[derive(Debug, Clone, Copy)]
enum Expect {
    Nothing,
    Fat(u32),
    Dir { lba: u32, scan: bool },
    Scratch,
    Count { sector: u32, left: u16 },
}

/// What [`Appender::block`] returns.
#[derive(Debug, Clone, Copy)]
enum Source {
    Fat,
    Dir,
    Scratch,
    Zeros,
}

/// The volume's FSInfo sector.
#[derive(Debug, Clone, Copy)]
struct Info {
    read: bool,
    /// Read, and signed: the writer keeps it current.
    valid: bool,
    /// The free count or the cursor moved since it was written.
    dirty: bool,
}

/// The log's writer. See the module docs.
#[derive(Debug, Clone)]
pub struct Appender {
    vol: Volume,
    mounted: bool,
    file_max: u32,
    /// [`Policy::keep_free_mb`], capped, in clusters.
    reserve: u32,
    fat: FatCache,
    dir: DirCache,
    /// FSInfo's read-modify-write, and the `LOG/` dot sector.
    scratch: [u8; BLOCK],
    tail: [u8; BLOCK],
    tail_len: u16,
    phase: Phase,
    after_census: Phase,
    stopped: Option<Fault>,
    expect: Expect,
    source: Source,
    work: u16,
    free: Option<u32>,
    /// Where allocation looks next.
    cursor: u32,
    info: Info,
    /// The free count's next FAT sector, and its tally.
    count_next: u32,
    counted: u32,
    log_dir: Option<u32>,
    file: Option<Open>,
    root: Root,
    census: Census,
    walk: Walk,
    plan: Plan,
    /// The clusters this task allocated, in order.
    new: [u32; CHUNK_BLOCKS],
    got: u8,
    want: u8,
    /// FAT entries this allocation looked at, bounded by the volume's clusters.
    looked: u32,
    zeros_next: u8,
    doomed: Named,
    /// The next cluster of the doomed file's chain to free, or 0.
    freeing: u32,
    freed_steps: u32,
}

impl Default for Appender {
    fn default() -> Self {
        Self::new()
    }
}

/// `Err` carries the step a phase stops at.
type Flow = Result<(), Step>;

impl Appender {
    /// An appender with no volume. `const`, so the bridge can hold it in a static.
    #[must_use]
    pub const fn new() -> Self {
        let none = Slot { lba: 0, index: 0 };
        Self {
            vol: NO_VOLUME,
            mounted: false,
            file_max: 0,
            reserve: 0,
            fat: FatCache { sector: None, data: [0; BLOCK], dirty: false, written: 0 },
            dir: DirCache { lba: None, data: [0; BLOCK] },
            scratch: [0; BLOCK],
            tail: [0; BLOCK],
            tail_len: 0,
            phase: Phase::Idle,
            after_census: Phase::Idle,
            stopped: None,
            expect: Expect::Nothing,
            source: Source::Zeros,
            work: 0,
            free: None,
            cursor: 2,
            info: Info { read: false, valid: false, dirty: false },
            count_next: 0,
            counted: 0,
            log_dir: None,
            file: None,
            root: Root { log: None, file: false, free: None },
            census: Census { highest: None, oldest: None, reclaimable: 0, free: None },
            walk: Walk {
                kind: Kind::Root,
                cluster: 0,
                sector: 0,
                steps: 0,
                scanned: false,
                done: false,
            },
            plan: Plan { base: 0, blocks: 0, size: 0, want: 0, next: 0 },
            new: [0; CHUNK_BLOCKS],
            got: 0,
            want: 0,
            looked: 0,
            zeros_next: 0,
            doomed: Named { number: 0, slot: none },
            freeing: 0,
            freed_steps: 0,
        }
    }

    /// Take `volume` to write, forgetting any other. It reads nothing: the checks are on the
    /// boot sector's fields alone.
    ///
    /// # Errors
    ///
    /// A [`Refused`] naming what the writer does not handle.
    pub fn mount(&mut self, volume: &Volume, policy: Policy) -> Result<(), Refused> {
        self.reset();
        if volume.version != 0 {
            return Err(Refused::Version(volume.version));
        }
        if !(1..=2).contains(&volume.fats) {
            return Err(Refused::Fats(volume.fats));
        }
        if volume.ext_flags & UNMIRRORED != 0 {
            return Err(Refused::Unmirrored);
        }
        if volume.cluster_lba(volume.root_cluster).is_none() {
            return Err(Refused::RootCluster(volume.root_cluster));
        }
        self.vol = *volume;
        self.mounted = true;
        // A file never grows past what a FAT32 size holds.
        self.file_max = policy.file_max.min(u32::MAX - CHUNK_BYTES as u32);
        let keep_mb = policy.keep_free_mb.min(volume.size_mb() / 8);
        let reserve = u64::from(keep_mb) * 2048 / u64::from(volume.sectors_per_cluster);
        self.reserve = u32::try_from(reserve).unwrap_or(u32::MAX);
        Ok(())
    }

    /// Begin opening the next log file: read FSInfo, find or create `LOG/`, count what is in
    /// it, and add a file numbered past the highest. Called again when [`Self::wants_rotate`],
    /// it closes nothing: the old file's entry was final after its last flush.
    pub fn open(&mut self) {
        if self.stopped.is_some() {
            return;
        }
        if !self.mounted || self.phase != Phase::Idle {
            return self.misuse();
        }
        self.file = None;
        self.tail_len = 0;
        self.phase = Phase::Fsinfo;
    }

    /// The open file's partial last block, 0 to 511 bytes, which the bridge copies to the start
    /// of staged block 0.
    #[must_use]
    pub fn tail(&self) -> &[u8] {
        &self.tail[..usize::from(self.tail_len)]
    }

    /// The bytes a flush takes after the tail.
    #[must_use]
    pub fn room(&self) -> usize {
        CHUNK_BYTES - usize::from(self.tail_len)
    }

    /// Begin a flush of `staged_len` bytes, the tail included, from staged block 0 on. `last` is
    /// a copy of the last staged block, so the writer keeps the new tail.
    pub fn flush(&mut self, staged_len: usize, last: &[u8; BLOCK]) {
        if self.stopped.is_some() {
            return;
        }
        let tail = usize::from(self.tail_len);
        let Some(file) = self.file else { return self.misuse() };
        if self.phase != Phase::Idle || !(tail..=CHUNK_BYTES).contains(&staged_len) {
            return self.misuse();
        }
        if staged_len == tail {
            return;
        }
        // The tail is the size's partial block, so the start is block-aligned.
        let start = file.size - u32::from(self.tail_len);
        // At most `CHUNK_BYTES`.
        let Some(size) = start.checked_add(staged_len as u32) else { return self.misuse() };
        let cluster = u64::from(self.vol.sectors_per_cluster) * BLOCK as u64;
        let want = u64::from(size).div_ceil(cluster).checked_sub(u64::from(file.chain));
        // Staged blocks span at most one cluster each.
        let Some(want) =
            want.and_then(|w| u8::try_from(w).ok()).filter(|&w| usize::from(w) <= CHUNK_BLOCKS)
        else {
            return self.misuse();
        };
        self.plan = Plan {
            base: start / BLOCK as u32,
            // At most `CHUNK_BLOCKS`.
            blocks: staged_len.div_ceil(BLOCK) as u16,
            size,
            want,
            next: 0,
        };
        let keep = size as usize % BLOCK;
        self.tail[..keep].copy_from_slice(&last[..keep]);
        self.tail_len = keep as u16;
        self.phase = Phase::Reserve;
    }

    /// The next thing to do. See [`Step`].
    // Not an iterator: each step completes on the card before the next call, which is how the
    // writer learns a write succeeded, and a finished task keeps answering `Done`.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Step {
        self.work = 0;
        loop {
            if let Err(step) = self.advance() {
                return step;
            }
        }
    }

    /// The block a [`Step::Read`] or each block a [`Step::ReadRun`] brought back.
    pub fn fed(&mut self, block: &[u8; BLOCK]) {
        self.work = 0;
        match self.expect {
            Expect::Nothing => {}
            Expect::Fat(sector) => {
                self.fat.data = *block;
                self.fat.sector = Some(sector);
                self.fat.dirty = false;
                self.expect = Expect::Nothing;
            }
            Expect::Dir { lba, scan } => {
                self.dir.data = *block;
                self.dir.lba = Some(lba);
                self.expect = Expect::Nothing;
                if scan {
                    self.scan(lba);
                }
            }
            Expect::Scratch => {
                self.scratch = *block;
                self.expect = Expect::Nothing;
            }
            Expect::Count { sector, left } => {
                let first = sector * fat::ENTRIES_PER_SECTOR;
                let end = (first + fat::ENTRIES_PER_SECTOR).min(self.vol.clusters + 2);
                for cluster in first.max(2)..end {
                    self.work += 1;
                    if fat::is_free(fat::entry(block, cluster)) {
                        self.counted += 1;
                    }
                }
                self.expect = match left {
                    0 | 1 => Expect::Nothing,
                    _ => Expect::Count { sector: sector + 1, left: left - 1 },
                };
            }
        }
    }

    /// What a [`Step::Write`] writes.
    #[must_use]
    pub fn block(&self) -> &[u8; BLOCK] {
        match self.source {
            Source::Fat => &self.fat.data,
            Source::Dir => &self.dir.data,
            Source::Scratch => &self.scratch,
            Source::Zeros => &ZEROS,
        }
    }

    /// Whether the open file reached [`Policy::file_max`], so the bridge opens the next.
    #[must_use]
    pub fn wants_rotate(&self) -> bool {
        self.file.is_some_and(|file| file.size >= self.file_max)
    }

    /// The open file's number.
    #[must_use]
    pub fn file(&self) -> Option<u32> {
        self.file.map(|file| file.number)
    }

    /// The open file's size, in bytes; 0 with none open.
    #[must_use]
    pub fn size(&self) -> u32 {
        self.file.map_or(0, |file| file.size)
    }

    /// The volume's free space, in MiB, once an open has read or counted it.
    #[must_use]
    pub fn free_mb(&self) -> Option<u32> {
        self.free.map(|free| mib(u64::from(free) * u64::from(self.vol.sectors_per_cluster)))
    }

    /// The scanning the last [`Self::next`] or [`Self::fed`] did: FAT entries, a directory entry
    /// counting as 8. At most [`SCAN_MAX`].
    #[must_use]
    pub fn scanned(&self) -> u16 {
        self.work
    }

    /// Forget the volume and everything read from it. The driver calls it after any failed
    /// transfer, then mounts again.
    pub fn reset(&mut self) {
        // The buffers keep their bytes: a read refills each before anything uses it.
        self.vol = NO_VOLUME;
        self.mounted = false;
        self.fat.sector = None;
        self.fat.dirty = false;
        self.fat.written = 0;
        self.dir.lba = None;
        self.tail_len = 0;
        self.phase = Phase::Idle;
        self.stopped = None;
        self.expect = Expect::Nothing;
        self.source = Source::Zeros;
        self.work = 0;
        self.free = None;
        self.cursor = 2;
        self.info = Info { read: false, valid: false, dirty: false };
        self.log_dir = None;
        self.file = None;
    }

    fn misuse(&mut self) {
        self.stopped = Some(Fault::Misuse);
        self.phase = Phase::Idle;
    }

    /// Stop the writer at `fault`.
    fn fault(&mut self, fault: Fault) -> Flow {
        self.stopped = Some(fault);
        self.phase = Phase::Idle;
        Err(Step::Done(Err(fault)))
    }

    /// One phase's work: `Ok` when it moved on without a step, `Err` with the step it stops at.
    fn advance(&mut self) -> Flow {
        match self.phase {
            Phase::Idle => Err(Step::Done(self.stopped.map_or(Ok(()), Err))),
            Phase::Fsinfo => self.read_fsinfo(),
            Phase::FsinfoParse => {
                self.parse_fsinfo();
                Ok(())
            }
            Phase::Count => self.count(),
            Phase::Locate => {
                self.locate();
                Ok(())
            }
            Phase::Walk => self.walk(),
            Phase::RootDone => self.root_done(),
            Phase::CreateAlloc => {
                self.alloc()?;
                self.zeros_next = 1;
                self.phase = Phase::CreateDot;
                Ok(())
            }
            Phase::CreateDot => self.create_dot(),
            Phase::CreateZeros => self.create_zeros(),
            Phase::CreateEntry => self.create_entry(),
            Phase::Decide => self.decide(),
            Phase::Entry => self.entry(),
            Phase::Reserve => self.reserve(),
            Phase::Alloc => {
                self.alloc()?;
                self.phase = Phase::Data;
                Ok(())
            }
            Phase::Data => self.data(),
            Phase::Link => self.link(),
            Phase::Size => self.write_size(),
            Phase::Forget => self.forget(),
            Phase::Free => self.free_chain(),
            Phase::Freed => {
                self.fat_commit()?;
                self.start_census(self.after_census);
                Ok(())
            }
            Phase::Finish => self.finish(),
            Phase::FinishWrite => self.finish_write(),
        }
    }

    // The caches.

    /// Hold FAT sector `sector`, writing a dirty one out first.
    fn fat_ready(&mut self, sector: u32) -> Flow {
        if self.fat.sector == Some(sector) {
            return Ok(());
        }
        self.fat_commit()?;
        self.fat.sector = None;
        self.expect = Expect::Fat(sector);
        Err(Step::Read(self.vol.fat_start + sector))
    }

    /// Write a dirty FAT sector to the next FAT it has not reached.
    fn fat_commit(&mut self) -> Flow {
        let Some(sector) = self.fat.sector.filter(|_| self.fat.dirty) else { return Ok(()) };
        let copy = u32::from(self.fat.written);
        self.fat.written += 1;
        if self.fat.written >= self.vol.fats {
            self.fat.dirty = false;
            self.fat.written = 0;
        }
        self.source = Source::Fat;
        Err(Step::Write(self.vol.fat_start + copy * self.vol.fat_blocks + sector))
    }

    /// `cluster`'s FAT entry.
    fn fat_get(&mut self, cluster: u32) -> Result<u32, Step> {
        self.fat_ready(fat::sector_of(cluster))?;
        Ok(fat::entry(&self.fat.data, cluster))
    }

    /// Set `cluster`'s FAT entry, to reach the card at the next commit.
    fn fat_set(&mut self, cluster: u32, value: u32) -> Flow {
        self.fat_ready(fat::sector_of(cluster))?;
        self.work += 1;
        fat::set_entry(&mut self.fat.data, cluster, value);
        // A change after some FATs were written goes to all of them again.
        self.fat.dirty = true;
        self.fat.written = 0;
        Ok(())
    }

    /// Hold directory sector `lba`.
    fn dir_ready(&mut self, lba: u32) -> Flow {
        if self.dir.lba == Some(lba) {
            return Ok(());
        }
        self.dir.lba = None;
        self.expect = Expect::Dir { lba, scan: false };
        Err(Step::Read(lba))
    }

    /// Write the directory sector held back to the card.
    fn dir_write(&mut self) -> Step {
        self.source = Source::Dir;
        // Only called with a sector held.
        Step::Write(self.dir.lba.unwrap_or(0))
    }

    /// Write `source` to `lba`, a sector of a cluster just allocated. The directory cache never
    /// holds one, and is cleared if it does, so it holds no stale copy.
    fn write_new(&mut self, lba: u32, source: Source) -> Flow {
        if self.dir.lba == Some(lba) {
            self.dir.lba = None;
        }
        self.source = source;
        Err(Step::Write(lba))
    }

    // Opening.

    fn read_fsinfo(&mut self) -> Flow {
        self.phase = Phase::FsinfoParse;
        if !self.info.read {
            self.info.read = true;
            if let Some(lba) = self.vol.fsinfo_lba {
                self.expect = Expect::Scratch;
                return Err(Step::Read(lba));
            }
        }
        self.phase = Phase::Locate;
        if self.free.is_none() {
            self.start_count();
        }
        Ok(())
    }

    fn parse_fsinfo(&mut self) {
        if fsinfo_signed(&self.scratch) {
            self.info.valid = true;
            let free = le32(&self.scratch, fsinfo::FREE);
            self.free = (free <= self.vol.clusters).then_some(free);
            let hint = le32(&self.scratch, fsinfo::NEXT_FREE);
            if (2..self.vol.clusters + 2).contains(&hint) {
                self.cursor = hint;
            }
        }
        self.phase = Phase::Locate;
        if self.free.is_none() {
            self.start_count();
        }
    }

    fn start_count(&mut self) {
        self.count_next = 0;
        self.counted = 0;
        self.phase = Phase::Count;
    }

    /// Count the free clusters, reading the first FAT in runs.
    fn count(&mut self) -> Flow {
        let sectors = (self.vol.clusters + 2).div_ceil(fat::ENTRIES_PER_SECTOR);
        if self.count_next < sectors {
            // At most `CHUNK_BLOCKS`.
            let count = (sectors - self.count_next).min(CHUNK_BLOCKS as u32) as u16;
            self.expect = Expect::Count { sector: self.count_next, left: count };
            let lba = self.vol.fat_start + self.count_next;
            self.count_next += u32::from(count);
            return Err(Step::ReadRun { lba, count });
        }
        self.free = Some(self.counted);
        self.info.dirty = true;
        self.phase = Phase::Locate;
        Ok(())
    }

    /// Find `LOG/`, or remember it from the last open.
    fn locate(&mut self) {
        if self.log_dir.is_some() {
            return self.start_census(Phase::Decide);
        }
        self.root = Root { log: None, file: false, free: None };
        self.start_walk(Kind::Root, self.vol.root_cluster);
    }

    fn start_walk(&mut self, kind: Kind, cluster: u32) {
        self.walk = Walk { kind, cluster, sector: 0, steps: 0, scanned: false, done: false };
        self.phase = Phase::Walk;
    }

    /// Read `LOG/`, then go to `after`.
    fn start_census(&mut self, after: Phase) {
        // A delete ends in a census too, so `reclaimable` never counts a deleted file.
        self.census = Census { highest: None, oldest: None, reclaimable: 0, free: None };
        self.after_census = after;
        let log = self.log_dir.unwrap_or(0);
        self.start_walk(Kind::Census, log);
    }

    /// Read the walk's next sector, following the chain at a cluster's end.
    fn walk(&mut self) -> Flow {
        if !self.walk.done && self.walk.scanned {
            if u32::from(self.walk.sector) + 1 < u32::from(self.vol.sectors_per_cluster) {
                self.walk.sector += 1;
            } else {
                let next = self.fat_get(self.walk.cluster)?;
                if fat::is_end(next) {
                    self.walk.done = true;
                } else if !fat::is_link(next, self.vol.clusters)
                    || next == self.walk.cluster
                    || self.walk.steps >= self.vol.clusters
                {
                    return self.fault(Fault::Corrupt);
                } else {
                    self.walk.steps += 1;
                    self.walk.cluster = next;
                    self.walk.sector = 0;
                }
            }
            self.walk.scanned = false;
        }
        if self.walk.done {
            self.phase = match self.walk.kind {
                Kind::Root => Phase::RootDone,
                Kind::Census => self.after_census,
            };
            return Ok(());
        }
        let Some(start) = self.vol.cluster_lba(self.walk.cluster) else {
            return self.fault(Fault::Corrupt);
        };
        let lba = start + u32::from(self.walk.sector);
        self.dir.lba = None;
        self.expect = Expect::Dir { lba, scan: true };
        Err(Step::Read(lba))
    }

    /// Scan the directory sector at `lba`, just read into the cache, for the walk.
    fn scan(&mut self, lba: u32) {
        let open = self.file.map(|file| file.number);
        for index in 0..fat::DIR_ENTRIES_PER_SECTOR {
            self.work += DIR_ENTRY_COST;
            let Some(entry) = fat::dir_entry(&self.dir.data, index) else { break };
            // Fewer than 256 entries in a sector.
            let slot = Slot { lba, index: index as u8 };
            if entry.is_end() || entry.is_deleted() {
                let free = match self.walk.kind {
                    Kind::Root => &mut self.root.free,
                    Kind::Census => &mut self.census.free,
                };
                free.get_or_insert(slot);
                if entry.is_end() {
                    self.walk.done = true;
                    break;
                }
                continue;
            }
            if entry.is_long_name() || entry.attr & attr::VOLUME != 0 {
                continue;
            }
            match self.walk.kind {
                Kind::Root if entry.name == fat::LOG_DIR => {
                    if entry.attr & attr::DIR != 0 {
                        self.root.log = Some(entry.cluster);
                    } else {
                        self.root.file = true;
                    }
                    self.walk.done = true;
                    break;
                }
                Kind::Census if entry.attr & attr::DIR == 0 => {
                    let Some(number) = fat::parse_log_name(&entry.name) else { continue };
                    self.census.highest = self.census.highest.max(Some(number));
                    if Some(number) == open {
                        continue;
                    }
                    let clusters = self.clusters_of(&entry);
                    self.census.reclaimable = self.census.reclaimable.saturating_add(clusters);
                    if self.census.oldest.is_none_or(|oldest| number < oldest.number) {
                        self.census.oldest = Some(Named { number, slot });
                    }
                }
                _ => {}
            }
        }
        self.walk.scanned = true;
    }

    /// The clusters deleting `entry` frees at least: what its size takes, or none when it names
    /// no cluster of the volume.
    fn clusters_of(&self, entry: &DirEntry) -> u32 {
        if !fat::is_link(entry.cluster, self.vol.clusters) {
            return 0;
        }
        let cluster = u32::from(self.vol.sectors_per_cluster) * BLOCK as u32;
        entry.size.div_ceil(cluster)
    }

    fn root_done(&mut self) -> Flow {
        if self.root.file {
            return self.fault(Fault::NotADirectory);
        }
        match self.root.log {
            Some(log) if fat::is_link(log, self.vol.clusters) => {
                self.log_dir = Some(log);
                self.start_census(Phase::Decide);
                Ok(())
            }
            Some(_) => self.fault(Fault::Corrupt),
            None if self.root.free.is_some() => {
                self.start_alloc(1);
                self.phase = Phase::CreateAlloc;
                Ok(())
            }
            None => self.fault(Fault::RootFull),
        }
    }

    /// `LOG/`'s first sector: `.` and `..`.
    fn create_dot(&mut self) -> Flow {
        self.fat_commit()?;
        let cluster = self.new[0];
        let Some(lba) = self.vol.cluster_lba(cluster) else { return self.fault(Fault::Corrupt) };
        self.scratch = fat::dot_sector(cluster);
        self.phase = Phase::CreateZeros;
        self.write_new(lba, Source::Scratch)
    }

    /// The rest of `LOG/`'s cluster: zeros, so it reads as empty.
    fn create_zeros(&mut self) -> Flow {
        if self.zeros_next >= self.vol.sectors_per_cluster {
            self.phase = Phase::CreateEntry;
            return Ok(());
        }
        let Some(start) = self.vol.cluster_lba(self.new[0]) else {
            return self.fault(Fault::Corrupt);
        };
        let lba = start + u32::from(self.zeros_next);
        self.zeros_next += 1;
        self.write_new(lba, Source::Zeros)
    }

    /// `LOG`'s entry in the root, written once its cluster holds an empty directory.
    fn create_entry(&mut self) -> Flow {
        let Some(slot) = self.root.free else { return self.fault(Fault::RootFull) };
        self.dir_ready(slot.lba)?;
        let log = self.new[0];
        let entry = DirEntry::dir(fat::LOG_DIR, log);
        fat::put_dir_entry(&mut self.dir.data, usize::from(slot.index), &entry);
        self.log_dir = Some(log);
        self.start_census(Phase::Decide);
        Err(self.dir_write())
    }

    /// The new file's number, past the highest of the log's.
    fn next_number(&self) -> Option<u32> {
        let next = self.census.highest.map_or(Some(1), |highest| highest.checked_add(1));
        next.filter(|&n| n <= fat::LOG_MAX)
    }

    /// After the census: a free entry for the new file, or delete the oldest for one.
    fn decide(&mut self) -> Flow {
        if self.next_number().is_none() {
            return self.fault(Fault::Numbers);
        }
        if self.census.free.is_some() {
            self.phase = Phase::Entry;
            return Ok(());
        }
        match self.census.oldest {
            Some(oldest) => {
                self.start_delete(oldest, Phase::Decide);
                Ok(())
            }
            None => self.fault(Fault::LogDirFull),
        }
    }

    /// The new file's entry: empty, with no cluster.
    fn entry(&mut self) -> Flow {
        let (Some(slot), Some(number)) = (self.census.free, self.next_number()) else {
            return self.fault(Fault::Misuse);
        };
        let Some(name) = fat::log_name(number) else { return self.fault(Fault::Numbers) };
        self.dir_ready(slot.lba)?;
        fat::put_dir_entry(
            &mut self.dir.data,
            usize::from(slot.index),
            &DirEntry::file(name, 0, 0),
        );
        self.file = Some(Open { number, slot, first: 0, last: 0, chain: 0, size: 0 });
        self.phase = Phase::Finish;
        Err(self.dir_write())
    }

    // Allocating.

    fn start_alloc(&mut self, want: u8) {
        self.want = want;
        self.got = 0;
        self.looked = 0;
    }

    /// Take `want` free clusters forward from the cursor, marking each end-of-chain.
    fn alloc(&mut self) -> Flow {
        let end = self.vol.clusters + 2;
        while self.got < self.want {
            if self.looked >= self.vol.clusters {
                return self.fault(Fault::Full);
            }
            let sector = fat::sector_of(self.cursor);
            self.fat_ready(sector)?;
            let stop = ((sector + 1) * fat::ENTRIES_PER_SECTOR).min(end);
            while self.cursor < stop && self.got < self.want {
                let cluster = self.cursor;
                self.cursor += 1;
                self.looked += 1;
                self.work += 1;
                if fat::is_free(fat::entry(&self.fat.data, cluster)) {
                    fat::set_entry(&mut self.fat.data, cluster, fat::EOC);
                    self.fat.dirty = true;
                    self.fat.written = 0;
                    self.new[usize::from(self.got)] = cluster;
                    self.got += 1;
                    self.free = self.free.map(|free| free.saturating_sub(1));
                    self.info.dirty = true;
                }
            }
            if self.cursor >= end {
                self.cursor = 2;
            }
        }
        Ok(())
    }

    // Flushing.

    /// Before allocating, delete the oldest files until the reserve holds, when deleting the
    /// log's files can make it hold.
    fn reserve(&mut self) -> Flow {
        let want = self.plan.want;
        if want == 0 {
            self.got = 0;
            self.phase = Phase::Data;
            return Ok(());
        }
        let free = u64::from(self.free.unwrap_or(0));
        let need = u64::from(want) + u64::from(self.reserve);
        if free < need {
            let reclaimable = u64::from(self.census.reclaimable);
            return match self.census.oldest.filter(|_| free + reclaimable >= need) {
                Some(oldest) => {
                    self.start_delete(oldest, Phase::Reserve);
                    Ok(())
                }
                None => self.fault(Fault::Full),
            };
        }
        self.start_alloc(want);
        self.phase = Phase::Alloc;
        Ok(())
    }

    /// The LBA staged block `index` goes to.
    fn block_lba(&self, index: u16) -> Option<u32> {
        let file = self.file?;
        let per = u32::from(self.vol.sectors_per_cluster);
        let block = self.plan.base + u32::from(index);
        let (nth, offset) = (block / per, block % per);
        let cluster = if nth + 1 == file.chain {
            file.last
        } else {
            let new = usize::try_from(nth.checked_sub(file.chain)?).ok()?;
            *self.new[..usize::from(self.got)].get(new)?
        };
        Some(self.vol.cluster_lba(cluster)? + offset)
    }

    /// The staged blocks, in runs that are contiguous on the card. The new clusters' marks
    /// reach the card first.
    fn data(&mut self) -> Flow {
        self.fat_commit()?;
        let first = self.plan.next;
        if first >= self.plan.blocks {
            let chained = self.file.is_some_and(|file| file.chain > 0);
            self.plan.next = if chained { 0 } else { 1 };
            self.phase = Phase::Link;
            return Ok(());
        }
        let Some(lba) = self.block_lba(first) else { return self.fault(Fault::Misuse) };
        let mut count = 1;
        while first + count < self.plan.blocks
            && self.block_lba(first + count) == Some(lba + u32::from(count))
        {
            count += 1;
        }
        self.plan.next = first + count;
        Err(Step::Data { lba, first, count })
    }

    /// Link the new clusters onto the chain: the file's last to the first new, then each to
    /// the next. The last new one keeps its end-of-chain mark.
    fn link(&mut self) -> Flow {
        let Some(mut file) = self.file else { return self.fault(Fault::Misuse) };
        while self.plan.next < u16::from(self.got) {
            let k = usize::from(self.plan.next);
            let from = if k == 0 { file.last } else { self.new[k - 1] };
            self.fat_set(from, self.new[k])?;
            self.plan.next += 1;
        }
        if self.got > 0 {
            if file.chain == 0 {
                file.first = self.new[0];
            }
            file.last = self.new[usize::from(self.got) - 1];
            file.chain += u32::from(self.got);
            self.file = Some(file);
        }
        self.phase = Phase::Size;
        Ok(())
    }

    /// The file's entry, its chain on the card: its first cluster and new size.
    fn write_size(&mut self) -> Flow {
        self.fat_commit()?;
        let Some(mut file) = self.file else { return self.fault(Fault::Misuse) };
        let Some(name) = fat::log_name(file.number) else { return self.fault(Fault::Numbers) };
        self.dir_ready(file.slot.lba)?;
        let entry = DirEntry::file(name, file.first, self.plan.size);
        fat::put_dir_entry(&mut self.dir.data, usize::from(file.slot.index), &entry);
        file.size = self.plan.size;
        self.file = Some(file);
        self.phase = Phase::Finish;
        Err(self.dir_write())
    }

    // Deleting the oldest.

    fn start_delete(&mut self, doomed: Named, after: Phase) {
        self.doomed = doomed;
        self.freeing = 0;
        self.freed_steps = 0;
        self.after_census = after;
        self.phase = Phase::Forget;
    }

    /// Mark the doomed file's entry, and its long-name entries in the same sector, deleted,
    /// before freeing anything it reaches.
    fn forget(&mut self) -> Flow {
        let slot = self.doomed.slot;
        self.dir_ready(slot.lba)?;
        let index = usize::from(slot.index);
        let entry = fat::dir_entry(&self.dir.data, index);
        let ours = entry.filter(|e| {
            e.attr & (attr::DIR | attr::VOLUME) == 0
                && fat::parse_log_name(&e.name) == Some(self.doomed.number)
                && (e.cluster == 0 || fat::is_link(e.cluster, self.vol.clusters))
        });
        let Some(entry) = ours else { return self.fault(Fault::Corrupt) };
        self.dir.data[index * fat::DIR_ENTRY] = fat::DELETED;
        // Its long-name entries in this sector go with it; fewer than a sector's worth.
        let checksum = fat::lfn_checksum(&entry.name);
        for before in (0..index).rev() {
            self.work += DIR_ENTRY_COST;
            let at = before * fat::DIR_ENTRY;
            let Some(long) = fat::dir_entry(&self.dir.data, before) else { break };
            if !long.is_long_name()
                || long.is_deleted()
                || self.dir.data[at + fat::LFN_CHECKSUM] != checksum
            {
                break;
            }
            self.dir.data[at] = fat::DELETED;
        }
        self.freeing = entry.cluster;
        self.phase = Phase::Free;
        Err(self.dir_write())
    }

    /// Free the doomed chain, a FAT sector at a time.
    fn free_chain(&mut self) -> Flow {
        while self.freeing != 0 {
            let cluster = self.freeing;
            let next = self.fat_get(cluster)?;
            let linked = fat::is_link(next, self.vol.clusters);
            // A freed entry read again is a loop.
            if !(linked || fat::is_end(next)) || self.freed_steps >= self.vol.clusters {
                return self.fault(Fault::Corrupt);
            }
            self.fat_set(cluster, 0)?;
            self.freed_steps += 1;
            self.free = self.free.map(|free| (free + 1).min(self.vol.clusters));
            self.info.dirty = true;
            self.freeing = if linked { next } else { 0 };
        }
        self.phase = Phase::Freed;
        Ok(())
    }

    // Finishing.

    /// Bring FSInfo up to date when the count or cursor moved, then report done.
    fn finish(&mut self) -> Flow {
        if let Some(lba) = self.vol.fsinfo_lba.filter(|_| self.info.valid && self.info.dirty) {
            self.expect = Expect::Scratch;
            self.phase = Phase::FinishWrite;
            return Err(Step::Read(lba));
        }
        self.phase = Phase::Idle;
        Err(Step::Done(Ok(())))
    }

    fn finish_write(&mut self) -> Flow {
        self.phase = Phase::Idle;
        let (Some(lba), Some(free)) = (self.vol.fsinfo_lba, self.free) else {
            return Err(Step::Done(Ok(())));
        };
        // A sector that lost its signatures since is no FSInfo to keep.
        if !fsinfo_signed(&self.scratch) {
            self.info.valid = false;
            return Err(Step::Done(Ok(())));
        }
        put32(&mut self.scratch, fsinfo::FREE, free);
        put32(&mut self.scratch, fsinfo::NEXT_FREE, self.cursor);
        self.info.dirty = false;
        self.source = Source::Scratch;
        Err(Step::Write(lba))
    }
}
