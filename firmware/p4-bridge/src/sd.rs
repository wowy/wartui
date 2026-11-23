//! The board's microSD card: what it holds, for `Status`, and a FAT32 format on a host's
//! `FormatSd`. The layout and the reading of a card are `wartui_proto::sd`, tested on the host;
//! this is the conversation with the card.
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
//! every [`EVERY`] to see that it is still there. A read that fails sends it back to identifying,
//! which also finds a swapped card.
//!
//! **A format writes `Layout::ops` in order**, each a transfer tried up to [`TRIES`] times, since
//! the bus clock was measured, not proven, to be clean (`src/config.rs`, `SD_HZ`). Then the card
//! is read again, and the answer is `Done` only when it reads back as FAT32. [`request_format`]
//! answers `NoCard` and `Busy` at once, so the request signal never holds a second format.

use core::cell::Cell;
use core::fmt::{self, Write};
use core::future::pending;

use aligned::{A4, Aligned};
use block_device_driver::BlockDevice as _;
use embassy_futures::select::{Either, select};
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
use wartui_proto::link::{BridgeToHost, LogStr, SdFormatOutcome, SdState, SdStatus};
use wartui_proto::sd::format::{Layout, Op, plan};
use wartui_proto::sd::{BLOCK, inspect, mib};

use crate::Shared;
use crate::config::SD_HZ;

/// How often [`run`] identifies a card while none answers, and checks one that did.
const EVERY: Duration = Duration::from_secs(5);
/// How long the card's supply settles after GPIO45 switches it on.
const POWER_UP: Duration = Duration::from_millis(100);
/// The most blocks one transfer carries: 16 KiB, the DMA ring's one fill.
const TRANSFER_BLOCKS: usize = 32;
/// Tries at each transfer before it counts as failed.
const TRIES: u8 = 3;

type Block = Aligned<A4, [u8; BLOCK]>;
type Card = sdio::DefaultBlockDevice<sdio::sd::Card, Slot<'static, 0, Async>, Delay>;

/// Every transfer's buffer: internal RAM, cache-line aligned, so the driver DMAs it in place.
static BUF: ConstStaticCell<InternalMemory<[Block; TRANSFER_BLOCKS]>> =
    ConstStaticCell::new(InternalMemory::new([const { Aligned([0; BLOCK]) }; TRANSFER_BLOCKS]));
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
    /// Raised by a format request that [`request_format`] passed on.
    format: Signal<NoopRawMutex, ()>,
}

impl Sd {
    pub const fn new() -> Self {
        let none = SdStatus { state: SdState::NoCard, card_mb: 0, free_mb: None };
        Self { status: Cell::new(none), format: Signal::new() }
    }
}

/// The card, for `Status`. The slot is always on this board, so never `None`.
pub fn status(shared: &Shared) -> Option<SdStatus> {
    Some(shared.sd.status.get())
}

/// Take the host's request to format the card: pass it to [`run`], or answer at once with no
/// card, or with a format already running or asked for.
pub fn request_format(shared: &Shared) {
    let sd = &shared.sd;
    match sd.status.get().state {
        SdState::NoCard => answer(shared, SdFormatOutcome::NoCard),
        SdState::Formatting => answer(shared, SdFormatOutcome::Busy),
        _ if sd.format.signaled() => answer(shared, SdFormatOutcome::Busy),
        _ => sd.format.signal(()),
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

fn set(shared: &Shared, status: SdStatus) {
    shared.sd.status.set(status);
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
            pending().await
        }
    };
    let mut card: Card = sdio::BlockDevice::new_uninit_sd_card(slot, Delay);
    let mut buf = BUF.take().get_mut();
    let buf: &mut [Block; TRANSFER_BLOCKS] = &mut buf;
    loop {
        let blocks = identify(&mut card, shared).await;
        let card_mb = mib(blocks);
        let khz = card.freq() / 1000;
        match inspect_card(&mut card, buf, blocks).await {
            Ok(status) => {
                set(shared, status);
                say_found(shared, khz, status);
            }
            Err(e) => {
                set(shared, failed(card_mb));
                log(shared, format_args!("sd: the {card_mb} MiB card failed a read: {e:?}"));
            }
        }
        let gone = serve(&mut card, buf, shared, blocks).await;
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
/// state is `NoCard` meanwhile, and the first failure is logged. A format asked for before the
/// card went is answered `NoCard`.
async fn identify(card: &mut Card, shared: &Shared) -> u64 {
    let mut said = false;
    loop {
        match card.reacquire(SD_HZ).await {
            Ok(()) => return card.card().csd.block_count(),
            Err(e) => {
                set(shared, SdStatus { state: SdState::NoCard, card_mb: 0, free_mb: None });
                if !said {
                    log(shared, format_args!("sd: no card answered: {e:?}"));
                    said = true;
                }
                if shared.sd.format.try_take().is_some() {
                    answer(shared, SdFormatOutcome::NoCard);
                }
            }
        }
        Timer::after(EVERY).await;
    }
}

/// A card of `card_mb` that failed a read or a format.
const fn failed(card_mb: u32) -> SdStatus {
    SdStatus { state: SdState::Failed, card_mb, free_mb: None }
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
/// sector. A table or a boot sector that points past the card's end is no FAT32 volume.
async fn inspect_card(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    card_blocks: u64,
) -> Result<SdStatus, MmcError> {
    let card_mb = mib(card_blocks);
    let none = SdStatus { state: SdState::NoFat32, card_mb, free_mb: None };
    let on_card = |lba: u32| u64::from(lba) < card_blocks;
    read(card, 0, &mut buf[..1]).await?;
    let Some(start) = inspect::partition(&buf[0]).filter(|&lba| on_card(lba)) else {
        return Ok(none);
    };
    read(card, start, &mut buf[..1]).await?;
    let Some(volume) = inspect::volume(&buf[0], start).filter(|v| on_card(v.fsinfo_lba)) else {
        return Ok(none);
    };
    read(card, volume.fsinfo_lba, &mut buf[..1]).await?;
    Ok(SdStatus { state: SdState::Ready, card_mb, free_mb: volume.free_mb(&buf[0]) })
}

/// Format the card on request, and check every [`EVERY`] that it still answers. Returns the
/// error of the check that found it gone.
async fn serve(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    shared: &Shared,
    card_blocks: u64,
) -> MmcError {
    loop {
        match select(shared.sd.format.wait(), Timer::after(EVERY)).await {
            Either::First(()) => format(card, buf, shared, card_blocks).await,
            Either::Second(()) => {
                if let Err(e) = read(card, 0, &mut buf[..1]).await {
                    return e;
                }
            }
        }
    }
}

/// Format the card of `card_blocks` FAT32, read it back, and answer the host.
async fn format(
    card: &mut Card,
    buf: &mut [Block; TRANSFER_BLOCKS],
    shared: &Shared,
    card_blocks: u64,
) {
    let card_mb = mib(card_blocks);
    set(shared, SdStatus { state: SdState::Formatting, card_mb, free_mb: None });
    log(shared, format_args!("sd: formatting the {card_mb} MiB card"));
    let start = Instant::now();
    // Any value names the volume; the clock gives each format its own.
    let volume_id = start.as_micros() as u32;
    let layout = match plan(card_blocks, volume_id) {
        Ok(layout) => layout,
        Err(why) => {
            set(shared, failed(card_mb));
            log(shared, format_args!("sd: no FAT32 layout fits the {card_mb} MiB card: {why:?}"));
            return answer(shared, SdFormatOutcome::Failed);
        }
    };
    if let Err((lba, e)) = write_layout(card, buf, &layout).await {
        set(shared, failed(card_mb));
        log(shared, format_args!("sd: a format write at block {lba} failed {TRIES} times: {e:?}"));
        return answer(shared, SdFormatOutcome::Failed);
    }
    let ms = start.elapsed().as_millis();
    let outcome = match inspect_card(card, buf, card_blocks).await {
        Ok(status) if status.state == SdState::Ready => {
            set(shared, status);
            match status.free_mb {
                Some(free) => {
                    log(shared, format_args!("sd: formatted in {ms} ms, {free} MiB free"))
                }
                None => log(shared, format_args!("sd: formatted in {ms} ms, free space unknown")),
            }
            SdFormatOutcome::Done
        }
        Ok(status) => {
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
