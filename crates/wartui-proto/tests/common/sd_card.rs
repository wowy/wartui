//! A sparse in-memory SD card, the stream `fatfs` reads a volume on it through, and a driver that
//! carries out an `Appender`'s steps on it the way the bridge does. Each test binary uses part of
//! it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom, Write};

use wartui_proto::sd::BLOCK;
use wartui_proto::sd::append::{Appender, CHUNK_BLOCKS, Fault, SCAN_MAX, Step};
use wartui_proto::sd::format::{Layout, Op};

/// A card of `blocks`, holding only the blocks written non-zero: a 128 GB card fits in memory.
#[derive(Clone)]
pub struct Card {
    pub blocks: u64,
    written: HashMap<u64, [u8; BLOCK]>,
}

impl Card {
    pub fn new(blocks: u64) -> Self {
        Self { blocks, written: HashMap::new() }
    }

    pub fn block(&self, lba: u64) -> [u8; BLOCK] {
        self.written.get(&lba).copied().unwrap_or([0; BLOCK])
    }

    pub fn put(&mut self, lba: u64, data: [u8; BLOCK]) {
        assert!(lba < self.blocks, "block {lba} is past the card's end");
        if data == [0; BLOCK] {
            self.written.remove(&lba);
        } else {
            self.written.insert(lba, data);
        }
    }

    pub fn apply(&mut self, op: &Op) {
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
    pub fn used(blocks: u64, layout: &Layout) -> Self {
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

    pub fn formatted(layout: &Layout) -> Self {
        let mut card = Self::used(u64::from(layout.card_blocks), layout);
        for op in layout.ops() {
            card.apply(&op);
        }
        card
    }

    /// The partition at `start` as a stream from its first byte, as `fatfs` reads a volume.
    pub fn partition(&mut self, start: u32) -> Partition<'_> {
        let len = (self.blocks - u64::from(start)) * BLOCK as u64;
        Partition { card: self, base: u64::from(start) * BLOCK as u64, len, pos: 0 }
    }
}

pub struct Partition<'a> {
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

/// The most steps one task may take before the driver calls it a hang.
const STEP_LIMIT: usize = 2_000_000;

/// Carries out an `Appender`'s steps on a card, the way the bridge does: one staging buffer of
/// `CHUNK_BLOCKS`, metadata through `block()`. It checks every call's scanning against
/// `SCAN_MAX`, records the steps of the last task and every block written, and keeps what each
/// file was given.
pub struct Driver {
    pub card: Card,
    staged: Vec<[u8; BLOCK]>,
    /// The last task's steps, its `Done` included.
    pub steps: Vec<Step>,
    /// Every block written, in order.
    pub writes: Vec<(u64, [u8; BLOCK])>,
    /// The most any one call scanned.
    pub max_scanned: u16,
    /// The bytes each file number was flushed, in order.
    pub contents: HashMap<u32, Vec<u8>>,
}

impl Driver {
    pub fn new(card: Card) -> Self {
        Self {
            card,
            staged: vec![[0; BLOCK]; CHUNK_BLOCKS],
            steps: Vec::new(),
            writes: Vec::new(),
            max_scanned: 0,
            contents: HashMap::new(),
        }
    }

    fn write(&mut self, lba: u32, data: [u8; BLOCK]) {
        self.card.put(u64::from(lba), data);
        self.writes.push((u64::from(lba), data));
    }

    fn scanned(&mut self, app: &Appender) {
        let scanned = app.scanned();
        assert!(scanned <= SCAN_MAX, "one call scanned {scanned}");
        self.max_scanned = self.max_scanned.max(scanned);
    }

    /// Run the task begun to its `Done`.
    pub fn run(&mut self, app: &mut Appender) -> Result<(), Fault> {
        self.steps.clear();
        for _ in 0..STEP_LIMIT {
            let step = app.next();
            self.scanned(app);
            self.steps.push(step);
            match step {
                Step::Read(lba) => {
                    let block = self.card.block(u64::from(lba));
                    app.fed(&block);
                    self.scanned(app);
                }
                Step::ReadRun { lba, count } => {
                    let count = usize::from(count);
                    assert!((1..=CHUNK_BLOCKS).contains(&count), "a run of {count}");
                    for (i, staged) in self.staged[..count].iter_mut().enumerate() {
                        *staged = self.card.block(u64::from(lba) + i as u64);
                    }
                    for i in 0..count {
                        let block = self.staged[i];
                        app.fed(&block);
                        self.scanned(app);
                    }
                }
                Step::Write(lba) => {
                    let block = *app.block();
                    self.write(lba, block);
                }
                Step::Data { lba, first, count } => {
                    let (first, count) = (usize::from(first), usize::from(count));
                    assert!(count >= 1 && first + count <= CHUNK_BLOCKS, "{first}+{count}");
                    for i in 0..count {
                        let block = self.staged[first + i];
                        self.write(lba + i as u32, block);
                    }
                }
                Step::Done(result) => return result,
            }
        }
        panic!("no Done after {STEP_LIMIT} steps");
    }

    pub fn open(&mut self, app: &mut Appender) -> Result<(), Fault> {
        app.open();
        self.run(app)
    }

    /// Stage the tail and `bytes`, as the bridge does, and flush them.
    pub fn flush(&mut self, app: &mut Appender, bytes: &[u8]) -> Result<(), Fault> {
        assert!(bytes.len() <= app.room(), "{} bytes, room for {}", bytes.len(), app.room());
        let mut buf = vec![0u8; CHUNK_BLOCKS * BLOCK];
        let tail = app.tail().to_vec();
        buf[..tail.len()].copy_from_slice(&tail);
        let len = tail.len() + bytes.len();
        buf[tail.len()..len].copy_from_slice(bytes);
        for (staged, chunk) in self.staged.iter_mut().zip(buf.chunks(BLOCK)) {
            staged.copy_from_slice(chunk);
        }
        let last = self.staged[len.max(1).div_ceil(BLOCK) - 1];
        let file = app.file();
        app.flush(len, &last);
        let result = self.run(app);
        if let (Ok(()), Some(file)) = (result, file) {
            self.contents.entry(file).or_default().extend_from_slice(bytes);
        }
        result
    }
}
