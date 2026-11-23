//! The board's microSD card: what it holds, for `Status`, a FAT32 format on a host's `FormatSd`,
//! and the writer of the bridge's diagnostics log (`src/sd_log.rs`). The layout, the reading of a
//! card and the log's FAT32 writer are `wartui_proto::sd`, tested on the host; this is the
//! conversation with the card.
//!
//! **[`run`] is one more future joined on core 0, and core 1 is never started.** esp-storage
//! stalls the other core for every flash write, wherever it is. A core 1 frozen while it holds the
//! global critical section or a scheduler lock leaves core 0 spinning on it, and on the bench
//! that panicked or hung the P4 mid-update (`docs/p4-sd-findings.md`). So the card's state sits in
//! [`Shared`] like its neighbors', in `Cell`s and a [`NoopRawMutex`] signal, and nothing is
//! borrowed across an `.await`.
//!
//! **Every transfer is at most [`TRANSFER_BLOCKS`] blocks, 16 KiB, from [`BUF`].** The driver's
//! DMA ring of four 4 KiB descriptors covers that in one fill. A longer transfer needs the SDMMC
//! interrupt mid-way to refill the ring, and a flash erase masks core 0's interrupts for about
//! 40 ms. The buffer is [`InternalMemory`], 64-byte aligned: a buffer aligned only to the `A4`
//! that `sdio` asks for is split into a bounced head and tail around its middle, which takes more
//! descriptors than the ring holds, and the refills come back.
//!
//! **GPIO45 switches the card's VDD through a P-channel MOSFET: low powers the card.** [`run`]
//! drives it low for its whole life. The driver's `with_power_enable` drives it high, which cuts
//! the card's power, so it is never used. The driver powers the card's I/O domain from LDO4
//! itself.
//!
//! **No card-detect line exists, so "no card" is an identification that failed.** [`run`]
//! identifies the card every [`EVERY`] until one answers, reads what it holds, then reads block 0
//! every [`EVERY`] to see that it is still there. Block 0 failing to read is the one sign of a
//! card gone, and sends it back to identifying, which also finds a swapped card. A card that
//! failed a read is read again in full while its block 0 reads; a read past block 0 that fails
//! leaves it `Failed`. A failed format stays `Failed` until the next format: a format cut short
//! leaves a zeroed block 0, which would read again as no FAT32 volume.
//!
//! **A format writes `Layout::ops` in order**, each a transfer tried up to [`TRIES`] times, since
//! the bus clock was measured, not proven, to be clean (`src/config.rs`, `SD_HZ`). Then the card
//! is read again, and the answer is `Done` only when it reads back as FAT32. [`request_format`]
//! marks the card `Formatting` as it passes a request on, so a `Status` sent before [`run`] wakes
//! already shows it, and answers `NoCard` and `Busy` at once, so the request signal never holds a
//! second format. Before the first identification ends, the state says nothing yet, so a request
//! is passed on unanswered: identification answers it `NoCard`, or the card found is formatted.
//!
//! **The log is the lowest-priority work on the P4.** [`serve`] wakes the writer every [`EVERY`],
//! and early when the ring fills past a mark or the setting changes. The writer appends to one
//! file through [`Appender`], a state machine that touches no card: [`drive`] carries out each of
//! its steps with the same [`read`] and [`write`] the rest of this module uses, at most
//! [`TRANSFER_BLOCKS`] blocks each, metadata through [`META`] and data through [`BUF`]. It yields
//! after every step, so every other future runs between two card transfers, and the synchronous
//! work of a step (the appender's one sector of scanning, or staging up to 16 KiB from PSRAM) is
//! timed and its longest reported as `busy_max_us`. It pauses while an update of the P4's image
//! or a node image upload is part-way in: both are a host's transfer in progress, and an update's
//! flash writes hold the executor. The ring keeps the newest records meanwhile.
//!
//! **Each flush is staged in [`BUF`]:** the file's partial last block first, so the file grows in
//! place, then an `Opened` record when the file is new, a `Dropped` record when the ring lost
//! records since the last, whole records from the ring while they fit, and a `Closed` record when
//! the file ends. The rest of the last block is zeros, which the reader skips. Records are copied
//! block by block, so one may span two.
//!
//! **A failure stops only the writer.** A failed transfer counts an error, drops the file and
//! reads block 0 at once: a card gone sends [`run`] back to identifying, and one that answers is
//! mounted again at the next tick, a new file, up to [`MOUNT_TRIES`] failures in a row. A fault
//! the appender reports, a volume it refuses or a run of failures stops the writer until the card
//! is identified again, formatted, or the setting changes. A format request ends a write in
//! progress at its next step, and records still in the ring go into the fresh card's first file.

use core::cell::Cell;
use core::fmt::{self, Write};
use core::future::pending;

use aligned::{A4, Aligned};
use block_device_driver::BlockDevice as _;
use embassy_futures::select::{Either3, select3};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Delay, Duration, Instant, Timer};
use esp_hal::Async;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::peripherals::{GPIO39, GPIO40, GPIO41, GPIO42, GPIO43, GPIO44, GPIO45, SDHOST};
use esp_hal::sdmmc::{self, ConfigError, SdHostController, Slot, SlotConfig};
use sdio::MmcError;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_proto::link::{BridgeToHost, LogStr, SdFormatOutcome, SdLogState, SdState, SdStatus};
use wartui_proto::sd::append::{Appender, CHUNK_BLOCKS, Fault, Policy, Step};
use wartui_proto::sd::format::{Layout, Op, plan};
use wartui_proto::sd::inspect::Volume;
use wartui_proto::sd::log::{self as records, CloseReason};
use wartui_proto::sd::{BLOCK, inspect, mib};

use crate::config::{SD_HZ, SD_LOG_BYTES, SD_LOG_FILE_MAX, SD_LOG_KEEP_FREE_MB};
use crate::sd_log::{self, Ring};
use crate::{Shared, now_ms};

/// How often [`run`] identifies a card while none answers, and checks one that did.
const EVERY: Duration = Duration::from_secs(5);
/// How long the card's supply settles after GPIO45 switches it on.
const POWER_UP: Duration = Duration::from_millis(100);
/// The most blocks one transfer carries: 16 KiB, the DMA ring's one fill.
const TRANSFER_BLOCKS: usize = 32;
/// Tries at each transfer before it counts as failed.
const TRIES: u8 = 3;
/// Failures in a row after which the log's writer stops until it starts afresh.
const MOUNT_TRIES: u8 = 3;
/// How often a paused writer looks again for the update it gives way to to end.
const GIVE_WAY: Duration = Duration::from_millis(100);
/// The bytes one flush stages.
const CHUNK_BYTES: usize = TRANSFER_BLOCKS * BLOCK;
/// The most flushes one wake runs: a full ring's worth, and two to spare.
const FLUSHES_MAX: usize = SD_LOG_BYTES / CHUNK_BYTES + 2;
/// Room for an `Opened`, `Dropped` or `Closed` record.
const SMALL_RECORD: usize = 256;
/// How large a log file grows, and the free space the log keeps.
const POLICY: Policy = Policy { file_max: SD_LOG_FILE_MAX, keep_free_mb: SD_LOG_KEEP_FREE_MB };

const _: () = assert!(TRANSFER_BLOCKS == CHUNK_BLOCKS, "a flush is one transfer's blocks");

type Block = Aligned<A4, [u8; BLOCK]>;
type Card = sdio::DefaultBlockDevice<sdio::sd::Card, Slot<'static, 0, Async>, Delay>;

/// Every transfer's buffer: internal RAM, cache-line aligned, so the driver DMAs it in place.
static BUF: ConstStaticCell<InternalMemory<[Block; TRANSFER_BLOCKS]>> =
    ConstStaticCell::new(InternalMemory::new([const { Aligned([0; BLOCK]) }; TRANSFER_BLOCKS]));
/// The block the log's metadata goes through: a FAT or directory sector, FSInfo, or block 0's
/// check after a failure. Aligned as [`BUF`] is.
static META: ConstStaticCell<InternalMemory<[Block; 1]>> =
    ConstStaticCell::new(InternalMemory::new([const { Aligned([0; BLOCK]) }; 1]));
/// The log's writer, with its caches: static, to keep it out of [`run`]'s future.
static APPENDER: ConstStaticCell<Appender> = ConstStaticCell::new(Appender::new());
/// The SDMMC host, which the slot borrows for its life.
static CONTROLLER: StaticCell<SdHostController<'static>> = StaticCell::new();

/// The SDMMC host and slot 0's IO_MUX pins, and GPIO45, the card's power switch.
pub struct Pins {
    pub sdhost: SDHOST<'static>,
    pub clk: GPIO43<'static>,
    pub cmd: GPIO44<'static>,
    pub d0: GPIO39<'static>,
    pub d1: GPIO40<'static>,
    pub d2: GPIO41<'static>,
    pub d3: GPIO42<'static>,
    pub power: GPIO45<'static>,
}

/// The card as the host reader and [`run`] share it.
pub struct Sd {
    /// What `Status` reports.
    status: Cell<SdStatus>,
    /// Whether the first identification has ended and `status` holds what it found.
    identified: Cell<bool>,
    /// Raised by a format request that [`request_format`] passed on.
    format: Signal<NoopRawMutex, ()>,
}

impl Sd {
    /// No card found yet.
    pub const fn new() -> Self {
        Self {
            status: Cell::new(SdStatus::card(SdState::NoCard, 0, None)),
            identified: Cell::new(false),
            format: Signal::new(),
        }
    }
}

/// The card and its log, for `Status`. The slot is always on this board, so never `None`.
pub fn status(shared: &Shared) -> Option<SdStatus> {
    Some(SdStatus { log: sd_log::report(shared), ..shared.sd.status.get() })
}

/// Take the host's request to format the card: mark it `Formatting` and pass it to [`run`], or
/// answer at once with no card, or with a format already running or asked for. Before the first
/// identification ends, pass it on as it is.
pub fn request_format(shared: &Shared) {
    let sd = &shared.sd;
    let status = sd.status.get();
    match status.state {
        _ if sd.format.signaled() => answer(shared, SdFormatOutcome::Busy),
        _ if !sd.identified.get() => sd.format.signal(()),
        SdState::NoCard => answer(shared, SdFormatOutcome::NoCard),
        SdState::Formatting => answer(shared, SdFormatOutcome::Busy),
        _ => {
            set(shared, SdStatus { state: SdState::Formatting, free_mb: None, ..status });
            sd.format.signal(());
        }
    }
}

#[inline(never)]
fn answer(shared: &Shared, outcome: SdFormatOutcome) {
    shared.push_host(&BridgeToHost::SdFormatted { outcome });
}

/// Send `args` to the host as a `Log` line.
fn log(shared: &Shared, args: fmt::Arguments<'_>) {
    let mut line = LogStr::new();
    let _ = line.write_fmt(args);
    shared.push_log(&line);
}

/// Report `status`. Each one is a finding, so the first ends the first identification.
fn set(shared: &Shared, status: SdStatus) {
    shared.sd.status.set(status);
    shared.sd.identified.set(true);
}

/// Report no card, and answer a format asked for meanwhile.
fn no_card(shared: &Shared) {
    set(shared, SdStatus::card(SdState::NoCard, 0, None));
    if shared.sd.format.try_take().is_some() {
        answer(shared, SdFormatOutcome::NoCard);
    }
}

/// Power the card, then identify it, report what it holds, check it is still there and format
/// it on request, forever. An SDMMC controller that refuses its configuration leaves the card
/// `NoCard`.
pub async fn run(pins: Pins, shared: &Shared) -> ! {
    let _power = Output::new(pins.power, Level::Low, OutputConfig::default());
    Timer::after(POWER_UP).await;
    let Pins { sdhost, clk, cmd, d0, d1, d2, d3, .. } = pins;
    let slot = match slot(sdhost, clk, cmd, d0, d1, d2, d3) {
        Ok(slot) => slot,
        Err(e) => {
            log(shared, format_args!("sd: the SDMMC controller refused its configuration: {e:?}"));
            no_card(shared);
            pending().await
        }
    };
    let mut card: Card = sdio::BlockDevice::new_uninit_sd_card(slot, Delay);
    let mut buf = BUF.take().get_mut();
    let buf: &mut [Block; TRANSFER_BLOCKS] = &mut buf;
    let mut writer = Writer::new(APPENDER.take(), META.take().get_mut().into_inner(), shared);
    loop {
        // Whatever the last card held is forgotten before the next is identified.
        writer.restart(shared);
        let blocks = identify(&mut card, shared).await;
        let card_mb = mib(blocks);
        let khz = card.freq() / 1000;
        let volume = match inspect_card(&mut card, buf, blocks).await {
            Ok((status, volume)) => {
                set(shared, status);
                say_found(shared, khz, status);
                volume
            }
            Err(e) => {
                set(shared, failed(card_mb));
                log(shared, format_args!("sd: the {card_mb} MiB card failed a read: {e:?}"));
                None
            }
        };
        let card_at = Served { blocks, khz };
        let gone = serve(&mut card, buf, &mut writer, volume, shared, card_at).await;
        log(shared, format_args!("sd: the card stopped answering: {gone:?}"));
    }
}

/// Slot 0 on its pins, async. The controller lives in [`CONTROLLER`], since the slot borrows it.
fn slot(
    sdhost: SDHOST<'static>,
    clk: GPIO43<'static>,
    cmd: GPIO44<'static>,
    d0: GPIO39<'static>,
    d1: GPIO40<'static>,
    d2: GPIO41<'static>,
    d3: GPIO42<'static>,
) -> Result<Slot<'static, 0, Async>, ConfigError> {
    let controller = CONTROLLER.init(SdHostController::new(sdhost, sdmmc::Config::default())?);
    let slot = controller.slot::<0>(SlotConfig::default())?;
    Ok(slot
        .with_clk(clk)
        .with_cmd(cmd)
        .with_data0(d0)
        .with_data1(d1)
        .with_data2(d2)
        .with_data3(d3)
        .into_async())
}

/// Identify the card, every [`EVERY`] until one answers, and return its size in blocks. The
/// state is `NoCard` meanwhile, and the first failure is logged. A format asked for before a
/// failure is answered `NoCard`.
async fn identify(card: &mut Card, shared: &Shared) -> u64 {
    let mut said = false;
    loop {
        match card.reacquire(SD_HZ).await {
            Ok(()) => return card.card().csd.block_count(),
            Err(e) => {
                no_card(shared);
                if !said {
                    log(shared, format_args!("sd: no card answered: {e:?}"));
                    said = true;
                }
            }
        }
        Timer::after(EVERY).await;
    }
}

/// A card of `card_mb` that failed a read or a format.
const fn failed(card_mb: u32) -> SdStatus {
    SdStatus::card(SdState::Failed, card_mb, None)
}

/// Say what a card that answered holds, and the clock it runs at.
fn say_found(shared: &Shared, khz: u32, status: SdStatus) {
    let card_mb = status.card_mb;
    match (status.state, status.free_mb) {
        (SdState::Ready, Some(free)) => log(
            shared,
            format_args!("sd: a {card_mb} MiB card at {khz} kHz: FAT32, {free} MiB free"),
        ),
        (SdState::Ready, None) => log(
            shared,
            format_args!("sd: a {card_mb} MiB card at {khz} kHz: FAT32, free space unknown"),
        ),
        _ => log(shared, format_args!("sd: a {card_mb} MiB card at {khz} kHz: no FAT32 volume")),
    }
}

/// What the card of `card_blocks` holds: its block 0, then the volume's boot sector and FSInfo
/// sector, when it has one, and the volume, for the log's writer. A table that points past the
/// card's end, or a volume that runs past it, is no FAT32 volume.
async fn inspect_card(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    card_blocks: u64,
) -> Result<(SdStatus, Option<Volume>), MmcError> {
    let card_mb = mib(card_blocks);
    let none = (SdStatus::card(SdState::NoFat32, card_mb, None), None);
    let on_card = |lba: u32| u64::from(lba) < card_blocks;
    read(card, 0, &mut buf[..1]).await?;
    let Some(start) = inspect::partition(&buf[0]).filter(|&lba| on_card(lba)) else {
        return Ok(none);
    };
    read(card, start, &mut buf[..1]).await?;
    let Some(volume) = inspect::volume(&buf[0], start).filter(|v| v.fits(card_blocks)) else {
        return Ok(none);
    };
    let free_mb = match volume.fsinfo_lba {
        Some(lba) => {
            read(card, lba, &mut buf[..1]).await?;
            volume.free_mb(&buf[0])
        }
        None => None,
    };
    Ok((SdStatus::card(SdState::Ready, card_mb, free_mb), Some(volume)))
}

/// The card [`serve`] serves: its size in blocks, and its clock in kHz, for the log lines.
#[derive(Clone, Copy)]
struct Served {
    blocks: u64,
    khz: u32,
}

/// Format the card on request, write the log every [`EVERY`] and when urged, and check every
/// [`EVERY`] that the card still answers, reading a card that failed a read again in full. A card
/// a format left `Failed` is not read again. `volume` is what the card holds, for the writer.
/// Returns the error of the check that found the card gone.
async fn serve(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    writer: &mut Writer,
    mut volume: Option<Volume>,
    shared: &Shared,
    at: Served,
) -> MmcError {
    // A card that failed a read is read again. One a format left `Failed` stays so: a format cut
    // short would read as `NoFat32` within 5 s.
    let mut retry_failed = true;
    // A deadline rather than a fresh timer each pass, so urges cannot put the check off.
    let mut tick = Instant::now() + EVERY;
    loop {
        let woke =
            select3(shared.sd.format.wait(), Timer::at(tick), shared.sd_log.urge.wait()).await;
        match woke {
            Either3::First(()) => {
                // No last flush: the fresh card's first file takes what the ring holds.
                writer.restart(shared);
                let (failed, found) = format(card, buf, shared, at.blocks).await;
                retry_failed = !failed;
                volume = found;
            }
            Either3::Second(()) => {
                tick = Instant::now() + EVERY;
                if let Err(e) = log_tick(card, buf, writer, volume.as_ref(), shared).await {
                    return e;
                }
                // Only block 0 failing means the card is gone; any other read that fails leaves
                // it `Failed`, so a card with one bad block is not identified every 5 s.
                if let Err(e) = read(card, 0, &mut buf[..1]).await {
                    return e;
                }
                if !retry_failed || shared.sd.status.get().state != SdState::Failed {
                    continue;
                }
                if let Ok((status, found)) = inspect_card(card, buf, at.blocks).await {
                    set(shared, status);
                    say_found(shared, at.khz, status);
                    volume = found;
                }
            }
            Either3::Third(()) => {
                if let Err(e) = log_tick(card, buf, writer, volume.as_ref(), shared).await {
                    return e;
                }
            }
        }
    }
}

/// Format the card of `card_blocks` FAT32, read it back, and answer the host. The state may be
/// `Formatting` already, from [`request_format`]. Returns whether it leaves the card `Failed`, and
/// the volume it holds after.
async fn format(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    shared: &Shared,
    card_blocks: u64,
) -> (bool, Option<Volume>) {
    let card_mb = mib(card_blocks);
    set(shared, SdStatus::card(SdState::Formatting, card_mb, None));
    log(shared, format_args!("sd: formatting the {card_mb} MiB card"));
    let start = Instant::now();
    // Any value names the volume; the clock gives each format its own.
    let volume_id = start.as_micros() as u32;
    let layout = match plan(card_blocks, volume_id) {
        Ok(layout) => layout,
        Err(why) => {
            set(shared, failed(card_mb));
            log(shared, format_args!("sd: no FAT32 layout fits the {card_mb} MiB card: {why:?}"));
            answer(shared, SdFormatOutcome::Failed);
            return (true, None);
        }
    };
    if let Err((lba, e)) = write_layout(card, buf, &layout).await {
        set(shared, failed(card_mb));
        log(shared, format_args!("sd: a format write at block {lba} failed {TRIES} times: {e:?}"));
        answer(shared, SdFormatOutcome::Failed);
        return (true, None);
    }
    let ms = start.elapsed().as_millis();
    let mut volume = None;
    let outcome = match inspect_card(card, buf, card_blocks).await {
        Ok((status, found)) if status.state == SdState::Ready => {
            volume = found;
            set(shared, status);
            match status.free_mb {
                Some(free) => {
                    log(shared, format_args!("sd: formatted in {ms} ms, {free} MiB free"))
                }
                None => log(shared, format_args!("sd: formatted in {ms} ms, free space unknown")),
            }
            SdFormatOutcome::Done
        }
        Ok((status, _)) => {
            set(shared, status);
            log(shared, format_args!("sd: formatted in {ms} ms, but it reads as no FAT32 volume"));
            SdFormatOutcome::Failed
        }
        Err(e) => {
            set(shared, failed(card_mb));
            log(shared, format_args!("sd: formatted in {ms} ms, then failed a read: {e:?}"));
            SdFormatOutcome::Failed
        }
    };
    answer(shared, outcome);
    (shared.sd.status.get().state == SdState::Failed, volume)
}

/// Carry out `layout`'s writes in order: a run of zeros from the zeroed buffer, a block from the
/// buffer's first. Stops at the first transfer that fails [`TRIES`] times, with its first block.
async fn write_layout(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    layout: &Layout,
) -> Result<(), (u32, MmcError)> {
    for op in layout.ops() {
        match &op {
            Op::Zeros { .. } => buf.iter_mut().for_each(|block| block.fill(0)),
            Op::Block { data, .. } => buf[0].copy_from_slice(data),
        }
        for (lba, count) in op.transfers(TRANSFER_BLOCKS as u32) {
            let blocks = &buf[..count as usize];
            write(card, lba, blocks).await.map_err(|e| (lba, e))?;
        }
    }
    Ok(())
}

/// Read `blocks.len()` blocks from `lba`, tried up to [`TRIES`] times.
async fn read(card: &mut Card, lba: u32, blocks: &mut [Block]) -> Result<(), MmcError> {
    let mut tries = 1;
    loop {
        match card.read(lba, blocks).await {
            Err(_) if tries < TRIES => tries += 1,
            result => return result,
        }
    }
}

/// Write `blocks` from `lba`, tried up to [`TRIES`] times.
async fn write(card: &mut Card, lba: u32, blocks: &[Block]) -> Result<(), MmcError> {
    let mut tries = 1;
    loop {
        match card.write(lba, blocks).await {
            Err(_) if tries < TRIES => tries += 1,
            result => return result,
        }
    }
}

/// The log's writer, as [`serve`] drives it. The appender and the metadata block are statics.
struct Writer {
    app: &'static mut Appender,
    meta: &'static mut [Block; 1],
    /// Failed transfers in a row; at [`MOUNT_TRIES`] the writer stops.
    failures: u8,
    /// Stopped on a fault, a refused volume or [`MOUNT_TRIES`] failures, until it starts afresh.
    stopped: bool,
    /// The setting's changes as of the last wake.
    toggles: u32,
    /// The part of the file whose `Opened` record the next flush leads with.
    owed: Option<u16>,
    /// Files opened this boot: the next one's part.
    parts: u16,
    /// The log's drops as of the last `Dropped` record that reached the card.
    dropped_said: u32,
}

impl Writer {
    fn new(app: &'static mut Appender, meta: &'static mut [Block; 1], shared: &Shared) -> Self {
        Self {
            app,
            meta,
            failures: 0,
            stopped: false,
            toggles: sd_log::toggles(shared),
            owed: None,
            parts: 0,
            dropped_said: 0,
        }
    }

    /// Forget the card and any file, and run again: for a card identified again, a format and a
    /// change of the setting. Nothing is written.
    fn restart(&mut self, shared: &Shared) {
        self.app.reset();
        self.failures = 0;
        self.stopped = false;
        self.owed = None;
        sd_log::update(shared, |stats| {
            stats.state = SdLogState::Waiting;
            stats.file = None;
        });
    }

    /// Drop the file after a failure; the next wake mounts again.
    fn drop_file(&mut self, shared: &Shared) {
        self.app.reset();
        self.owed = None;
        sd_log::update(shared, |stats| stats.file = None);
    }

    /// Stop until the writer starts afresh, in `state`.
    fn stop(&mut self, shared: &Shared, state: SdLogState) {
        self.drop_file(shared);
        self.stopped = true;
        sd_log::update(shared, |stats| stats.state = state);
    }
}

/// How a run of the appender's steps ended, short of a failed transfer.
enum Ended {
    /// The appender is done: the task succeeded, or stopped at a fault.
    Done(Result<(), Fault>),
    /// A format was asked for, which ends the task where it stands.
    Format,
}

/// Whether the writer gives way: an update of the P4's own image or a node image upload is
/// part-way in.
fn give_way(shared: &Shared) -> bool {
    crate::update::receiving(shared) || crate::cache::receiving(shared)
}

/// One wake of the log's writer: follow the setting, mount and open a file when there is none,
/// then flush what the ring holds. Returns the error of a failed transfer that found the card
/// gone; any other failure stops only the writer.
async fn log_tick(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    volume: Option<&Volume>,
    shared: &Shared,
) -> Result<(), MmcError> {
    if give_way(shared) {
        return Ok(());
    }
    let toggles = sd_log::toggles(shared);
    if toggles != w.toggles {
        w.toggles = toggles;
        if w.app.file().is_some() {
            close(card, buf, w, shared).await?;
        }
        w.restart(shared);
    }
    if !sd_log::on(shared) {
        sd_log::clear(shared);
        return Ok(());
    }
    if w.stopped {
        return Ok(());
    }
    if w.app.file().is_none() && !mount(card, buf, w, volume, shared).await? {
        return Ok(());
    }
    for _ in 0..FLUSHES_MAX {
        if w.app.wants_rotate() && !rotate(card, buf, w, shared).await? {
            return Ok(());
        }
        let owed = w.owed.is_some() || sd_log::dropped(shared) != w.dropped_said;
        if !owed && !sd_log::pending(shared) {
            break;
        }
        if flush(card, buf, w, shared, true, None).await?.is_none() {
            return Ok(());
        }
    }
    Ok(())
}

/// Mount `volume` and open the next file, when the card is ready. Returns whether a file is open.
async fn mount(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    volume: Option<&Volume>,
    shared: &Shared,
) -> Result<bool, MmcError> {
    let ready = shared.sd.status.get().state == SdState::Ready;
    let Some(volume) = volume.filter(|_| ready) else {
        sd_log::update(shared, |stats| stats.state = SdLogState::Waiting);
        return Ok(false);
    };
    if let Err(why) = w.app.mount(volume, POLICY) {
        w.stop(shared, SdLogState::Refused);
        log(shared, format_args!("sd log: the card's volume is refused: {why:?}"));
        return Ok(false);
    }
    open(card, buf, w, shared).await
}

/// Open the next file, and owe its `Opened` record. Returns whether it opened.
async fn open(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
) -> Result<bool, MmcError> {
    w.app.open();
    if !task(card, buf, w, shared, 0).await? {
        return Ok(false);
    }
    w.owed = Some(w.parts);
    w.parts = w.parts.saturating_add(1);
    sd_log::ask_snapshot(shared);
    Ok(true)
}

/// End the file at its size with a `Closed` record, and open the next. Returns whether one opened.
async fn rotate(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
) -> Result<bool, MmcError> {
    if flush(card, buf, w, shared, false, Some(CloseReason::Rotation)).await?.is_none() {
        return Ok(false);
    }
    open(card, buf, w, shared).await
}

/// The setting went off: flush what the ring holds, then end the file with a `Closed` record.
async fn close(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
) -> Result<(), MmcError> {
    for _ in 0..FLUSHES_MAX {
        let staged = flush(card, buf, w, shared, true, Some(CloseReason::SettingOff)).await?;
        if staged.is_none_or(|staged| staged.closed) {
            break;
        }
    }
    Ok(())
}

/// What one flush staged.
#[derive(Clone, Copy)]
struct Staged {
    /// Bytes from staged block 0, the tail included.
    len: usize,
    /// Records taken from the ring.
    taken: u32,
    /// The `Closed` record went in.
    closed: bool,
    /// The log's drops its `Dropped` record counts up to.
    dropped: u32,
}

/// Stage a flush and carry it out: records from the ring when `from_ring`, then `close`'s
/// `Closed` record once nothing is left to take. Returns what it staged, or `None` when the
/// writer stopped or gave way to a format.
async fn flush(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
    from_ring: bool,
    close: Option<CloseReason>,
) -> Result<Option<Staged>, MmcError> {
    let start = Instant::now();
    let tail = w.app.tail().len();
    let staged = stage(buf, w, shared, from_ring, close);
    let last = staged.len.saturating_sub(1) / BLOCK;
    w.app.flush(staged.len, &buf[last]);
    sd_log::note_busy(shared, start);
    if !task(card, buf, w, shared, staged.taken).await? {
        return Ok(None);
    }
    w.dropped_said = staged.dropped;
    let grew = (staged.len - tail) as u64;
    sd_log::update(shared, |stats| stats.written = stats.written.saturating_add(grew));
    Ok(Some(staged))
}

/// Fill `buf` for a flush: the file's tail, the `Opened` record owed, a `Dropped` record when the
/// log lost records since the last, whole ring records while they fit when `from_ring`, then
/// `close`'s record when the ring has nothing left to give. Zeros pad the last block.
#[inline(never)]
fn stage(
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
    from_ring: bool,
    close: Option<CloseReason>,
) -> Staged {
    let mut at = 0;
    put(buf, &mut at, w.app.tail());
    let mut record = [0; SMALL_RECORD];
    if let Some(part) = w.owed.take() {
        let file = w.app.file().unwrap_or(0);
        let card_mb = shared.sd.status.get().card_mb;
        let free_mb = w.app.free_mb();
        if let Some(n) = sd_log::encode_opened(shared, file, part, card_mb, free_mb, &mut record) {
            put(buf, &mut at, &record[..n]);
        }
    }
    let ms = now_ms() as u32;
    let dropped = sd_log::dropped(shared);
    let lost = dropped.wrapping_sub(w.dropped_said);
    if lost > 0
        && let Some(n) = records::encode_dropped(lost, ms, &mut record)
    {
        put(buf, &mut at, &record[..n]);
    }
    let taken = if from_ring {
        sd_log::with_ring(shared, |ring| take_records(ring, buf, &mut at)).unwrap_or(0)
    } else {
        0
    };
    let mut closed = false;
    if let Some(reason) = close
        && !(from_ring && sd_log::pending(shared))
        && let Some(n) = records::encode_closed(reason, ms, &mut record)
    {
        closed = put(buf, &mut at, &record[..n]);
    }
    let end = at.div_ceil(BLOCK) * BLOCK;
    if end > at {
        buf[at / BLOCK][at % BLOCK..].fill(0);
    }
    Staged { len: at, taken, closed, dropped }
}

/// Copy `bytes` into the staged blocks at `at`, block by block, and move `at` past them. Returns
/// whether they fit; nothing is copied when they do not.
fn put(buf: &mut [Block; TRANSFER_BLOCKS], at: &mut usize, bytes: &[u8]) -> bool {
    if *at + bytes.len() > CHUNK_BYTES {
        return false;
    }
    let mut rest = bytes;
    while !rest.is_empty() {
        let offset = *at % BLOCK;
        let n = rest.len().min(BLOCK - offset);
        buf[*at / BLOCK][offset..offset + n].copy_from_slice(&rest[..n]);
        *at += n;
        rest = &rest[n..];
    }
    true
}

/// Move whole records from `ring` into the staged blocks at `at` while the next fits. Returns how
/// many. Nothing here pushes to the host or logs: `ring` is borrowed.
fn take_records(ring: &mut Ring, buf: &mut [Block; TRANSFER_BLOCKS], at: &mut usize) -> u32 {
    let mut taken = 0;
    while let Some(len) = ring.peek_len() {
        if *at + len > CHUNK_BYTES {
            break;
        }
        loop {
            let offset = *at % BLOCK;
            // Past the last block only for an empty record, which reads nothing.
            let out = buf.get_mut(*at / BLOCK).map_or(&mut [][..], |block| &mut block[offset..]);
            let (n, done) = ring.read(out);
            *at += n;
            if done {
                break;
            }
            if n == 0 {
                return taken;
            }
        }
        taken += 1;
    }
    taken
}

/// Carry out the appender's task to its end, and take what it ended in. Returns whether it
/// succeeded; `taken` ring records were staged for it, and are counted lost when it did not.
async fn task(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
    taken: u32,
) -> Result<bool, MmcError> {
    let ended = drive(card, buf, w, shared).await;
    if !matches!(ended, Ok(Ended::Done(Ok(())))) {
        sd_log::lose(shared, taken);
    }
    match ended {
        Ok(Ended::Done(Ok(()))) => {
            w.failures = 0;
            let file = w.app.file();
            sd_log::update(shared, |stats| {
                stats.state = SdLogState::Writing;
                stats.file = file;
            });
            let status = shared.sd.status.get();
            if let Some(free_mb) = w.app.free_mb().filter(|_| status.state == SdState::Ready) {
                shared.sd.status.set(SdStatus { free_mb: Some(free_mb), ..status });
            }
            Ok(true)
        }
        Ok(Ended::Done(Err(fault))) => {
            let state = if fault == Fault::Full { SdLogState::Full } else { SdLogState::Failed };
            w.stop(shared, state);
            log(shared, format_args!("sd log: stopped: {fault:?}"));
            Ok(false)
        }
        Ok(Ended::Format) => {
            w.drop_file(shared);
            Ok(false)
        }
        Err(e) => {
            failed_transfer(card, w, shared, e).await?;
            Ok(false)
        }
    }
}

/// A transfer failed [`TRIES`] times: count it, drop the file, and read block 0 to see whether
/// the card is still there. Returns that read's error when it is gone.
async fn failed_transfer(
    card: &mut Card,
    w: &mut Writer,
    shared: &Shared,
    e: MmcError,
) -> Result<(), MmcError> {
    sd_log::update(shared, |stats| stats.errors = stats.errors.wrapping_add(1));
    w.drop_file(shared);
    log(shared, format_args!("sd log: a card transfer failed {TRIES} times: {e:?}"));
    read(card, 0, &mut w.meta[..]).await?;
    w.failures = w.failures.saturating_add(1);
    if w.failures >= MOUNT_TRIES {
        w.stop(shared, SdLogState::Failed);
        log(shared, format_args!("sd log: stopped after {MOUNT_TRIES} failures in a row"));
    } else {
        sd_log::update(shared, |stats| stats.state = SdLogState::Waiting);
    }
    Ok(())
}

/// Carry out the appender's steps until it is done, yielding after each, so every other future
/// runs between two card transfers. Each step's synchronous work is timed. Pauses while
/// [`give_way`], and ends at a format request.
async fn drive(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    w: &mut Writer,
    shared: &Shared,
) -> Result<Ended, MmcError> {
    loop {
        if shared.sd.format.signaled() {
            return Ok(Ended::Format);
        }
        while give_way(shared) {
            Timer::after(GIVE_WAY).await;
            if shared.sd.format.signaled() {
                return Ok(Ended::Format);
            }
        }
        let start = Instant::now();
        let step = w.app.next();
        if let Step::Write(_) = step {
            w.meta[0].copy_from_slice(w.app.block());
        }
        sd_log::note_busy(shared, start);
        match step {
            Step::Done(result) => return Ok(Ended::Done(result)),
            Step::Read(lba) => {
                read(card, lba, &mut w.meta[..]).await?;
                let start = Instant::now();
                w.app.fed(&w.meta[0]);
                sd_log::note_busy(shared, start);
            }
            Step::ReadRun { lba, count } => {
                let count = usize::from(count).min(TRANSFER_BLOCKS);
                read(card, lba, &mut buf[..count]).await?;
                for block in &buf[..count] {
                    let start = Instant::now();
                    w.app.fed(block);
                    sd_log::note_busy(shared, start);
                    yield_now().await;
                }
            }
            Step::Write(lba) => write(card, lba, &w.meta[..]).await?,
            Step::Data { lba, first, count } => {
                let first = usize::from(first);
                let Some(blocks) = buf.get(first..first + usize::from(count)) else {
                    return Ok(Ended::Done(Err(Fault::Misuse)));
                };
                write(card, lba, blocks).await?;
            }
        }
        yield_now().await;
    }
}
