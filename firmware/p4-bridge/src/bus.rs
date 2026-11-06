//! The P4's end of the node bus: two SPI masters, each with up to [`SLOTS`] nodes behind its
//! own CS (`docs/node-bus.md`).
//!
//! `wartui_proto::bus` has the framing and the handshake. The P4 checks framing only. Today the
//! records a node sends are counted, not forwarded, and what the P4 sends carries none: an idle
//! transfer with its seq and ack. The host sees the bus only as log lines.
//!
//! **Each bus runs on its own**, as a future in the main `join`, so a slow node on one never
//! holds up the other. Within a bus, transfers are one at a time, and the scheduler picks
//! the next:
//!
//! - **IRQ asserted**: a pass over the present slots, round robin. Each slot is clocked again
//!   while its transfers say [`MORE`](wartui_proto::bus::MORE), up to [`MORE_MAX`]. Absent
//!   slots are still probed on time, since a node not yet present may be the one asserting.
//!   IRQ is read again only once the node clocked last has had [`SLOT_GAP`] to re-arm, since
//!   until then its IRQ still says what the transfer just carried.
//! - **Otherwise**: each present slot every [`POLL_EVERY`], and each absent one every
//!   [`PROBE_EVERY`]. A slot is present after one good transfer, and absent after
//!   [`ABSENT_AFTER`] transfers in a row that fail the magic.
//! - **Every transfer to a slot waits [`SLOT_GAP`] after the last**, so the node has re-armed.
//!   A present slot that fails the magic is retried once after the gap and counted as a retry.
//!
//! Each slot has its own seq, so a node's gaps count only transfers meant for it.
//!
//! **IRQ is active high on the bench**, where a node's D0 reaches the P4 directly: pulled down,
//! so no node reads as no request. A carrier board inverts it ([`IRQ_ACTIVE_HIGH`]).

use core::fmt::Write;

use embassy_futures::select::{Either, select};
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Async;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{AnyPin, Event, Input, InputConfig, Level, Output, OutputConfig, Pull};
use esp_hal::peripherals::{DMA_AXI_CH0, DMA_AXI_CH1, SPI2, SPI3};
use esp_hal::spi::Mode;
use esp_hal::spi::master::{Config, Spi, SpiDma};
use esp_hal::time::Rate;
use static_cell::{ConstStaticCell, StaticCell};
use wartui_proto::bus::{
    Header, RecordType, SeqCheck, SeqNote, TRANSFER_LEN, VerifyError, seal, verify,
};
use wartui_proto::link::{LogLevel, LogStr};

use crate::Shared;

/// The bus clock. 4 MHz is the target on the carrier.
pub const BUS_HZ: u32 = 1_000_000;
/// Nodes on one bus.
pub const SLOTS: usize = 6;
/// Whether IRQ is asserted high. True for nodes wired straight to the P4; the carrier inverts
/// it.
// Chunk 4 detects the carrier and sets this at boot.
pub const IRQ_ACTIVE_HIGH: bool = true;
/// The longest a present slot goes unclocked.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// How often an absent slot is probed.
const PROBE_EVERY: Duration = Duration::from_millis(2_000);
/// Transfers in a row failing the magic before a present slot is absent.
const ABSENT_AFTER: u8 = 5;
/// The least time between the end of one transfer to a slot and the start of the next, for
/// the node to re-arm. The P4–C6 bench measured a re-arm at about 320 µs.
const SLOT_GAP: Duration = Duration::from_millis(1);
/// The most transfers to one slot in a row while it says [`MORE`](wartui_proto::bus::MORE).
const MORE_MAX: usize = 8;
/// How often each present slot's counters go to the host.
const REPORT_EVERY: Duration = Duration::from_millis(5_000);

/// One bus's pins, as `main` hands them over.
pub struct Pins {
    pub sck: AnyPin<'static>,
    pub mosi: AnyPin<'static>,
    pub miso: AnyPin<'static>,
    pub irq: AnyPin<'static>,
    /// Slot 1 first.
    pub cs: [AnyPin<'static>; SLOTS],
}

/// Each bus's slots and master, in statics so its future stays small.
static SLOTS_A: ConstStaticCell<[Slot; SLOTS]> =
    ConstStaticCell::new([const { Slot::new() }; SLOTS]);
static SLOTS_B: ConstStaticCell<[Slot; SLOTS]> =
    ConstStaticCell::new([const { Slot::new() }; SLOTS]);
static MASTER_A: StaticCell<Master> = StaticCell::new();
static MASTER_B: StaticCell<Master> = StaticCell::new();

/// Build both buses. Called once: the buffers and masters are statics.
///
/// Every CS line goes high before either master is built, so no node sees a transfer start.
/// Out of line, so what it builds is on this frame rather than `main`'s.
#[inline(never)]
pub fn build(
    (spi2, dma_a, a): (SPI2<'static>, DMA_AXI_CH0<'static>, Pins),
    (spi3, dma_b, b): (SPI3<'static>, DMA_AXI_CH1<'static>, Pins),
) -> (&'static mut Master, &'static mut Master) {
    let high = |pin| Output::new(pin, Level::High, OutputConfig::default());
    let cs_a = a.cs.map(high);
    let cs_b = b.cs.map(high);
    let pulled_up = |pin| Input::new(pin, InputConfig::default().with_pull(Pull::Up));
    let irq_pull = if IRQ_ACTIVE_HIGH { Pull::Down } else { Pull::Up };
    let irq = |pin| Input::new(pin, InputConfig::default().with_pull(irq_pull));
    let config = Config::default().with_frequency(Rate::from_hz(BUS_HZ)).with_mode(Mode::_0);

    let miso_a = pulled_up(a.miso);
    let spi_a = Spi::new(spi2, config)
        .expect("SPI2")
        .with_sck(a.sck)
        .with_mosi(a.mosi)
        .with_miso(miso_a.peripheral_input())
        .with_dma(dma_a)
        .into_async();
    let master_a = MASTER_A.init(Master {
        name: 'A',
        cs: cs_a,
        irq: irq(a.irq),
        _miso: miso_a,
        wire: Some(Wire {
            spi: spi_a,
            rx: dma_buf!(DmaRxBuf, TRANSFER_LEN),
            tx: dma_buf!(DmaTxBuf, TRANSFER_LEN),
        }),
        slots: SLOTS_A.take(),
        last_end: None,
        irq_since: None,
    });

    let miso_b = pulled_up(b.miso);
    let spi_b = Spi::new(spi3, config)
        .expect("SPI3")
        .with_sck(b.sck)
        .with_mosi(b.mosi)
        .with_miso(miso_b.peripheral_input())
        .with_dma(dma_b)
        .into_async();
    let master_b = MASTER_B.init(Master {
        name: 'B',
        cs: cs_b,
        irq: irq(b.irq),
        _miso: miso_b,
        wire: Some(Wire {
            spi: spi_b,
            rx: dma_buf!(DmaRxBuf, TRANSFER_LEN),
            tx: dma_buf!(DmaTxBuf, TRANSFER_LEN),
        }),
        slots: SLOTS_B.take(),
        last_end: None,
        irq_since: None,
    });
    (master_a, master_b)
}

/// The master and the buffers it hands back after each transfer.
struct Wire {
    spi: SpiDma<'static, Async>,
    rx: DmaRxBuf,
    tx: DmaTxBuf,
}

/// One slot's state and counters.
struct Slot {
    present: bool,
    /// Transfers in a row that failed the magic.
    misses: u8,
    /// The P4's seq for this slot's next transfer.
    seq: u16,
    /// The node's seq.
    peer: SeqCheck,
    /// When the last transfer to this slot started and ended.
    last_start: Option<Instant>,
    last_end: Option<Instant>,
    ok: u32,
    magic: u32,
    crc: u32,
    version: u32,
    len: u32,
    gaps: u32,
    retries: u32,
    records: u32,
    /// From IRQ asserted to the start of the transfer that carried a record.
    irq_us_sum: u64,
    irq_us_max: u64,
    irq_n: u32,
}

impl Slot {
    /// Absent and never clocked.
    const fn new() -> Self {
        Self {
            present: false,
            misses: 0,
            seq: 0,
            peer: SeqCheck::new(),
            last_start: None,
            last_end: None,
            ok: 0,
            magic: 0,
            crc: 0,
            version: 0,
            len: 0,
            gaps: 0,
            retries: 0,
            records: 0,
            irq_us_sum: 0,
            irq_us_max: 0,
            irq_n: 0,
        }
    }

    /// Whether this slot is owed a poll or a probe at `now`.
    fn due(&self, now: Instant) -> bool {
        self.last_start.is_none_or(|last| now >= last + self.every())
    }

    /// When this slot is next owed one.
    fn next_due(&self) -> Instant {
        self.last_start.map_or(Instant::MIN, |last| last + self.every())
    }

    const fn every(&self) -> Duration {
        if self.present { POLL_EVERY } else { PROBE_EVERY }
    }
}

/// What one transfer found.
enum Outcome {
    /// A good transfer, and whether it says [`MORE`](wartui_proto::bus::MORE).
    Good { more: bool },
    /// Nothing there: the magic failed.
    Empty,
    /// Something there, but corrupted, or not this build's.
    Bad,
}

/// One bus and its slots.
pub struct Master {
    /// `'A'` or `'B'`, for the log lines.
    name: char,
    /// One per slot, high.
    cs: [Output<'static>; SLOTS],
    /// Pulled to its inactive level.
    irq: Input<'static>,
    /// Held so its pull stays: an empty slot then reads constant bytes, which fail the magic.
    _miso: Input<'static>,
    /// Out only while a transfer runs.
    wire: Option<Wire>,
    slots: &'static mut [Slot; SLOTS],
    /// When the last transfer on this bus ended.
    last_end: Option<Instant>,
    /// When IRQ was first seen asserted, until it is seen deasserted.
    irq_since: Option<Instant>,
}

/// Run one bus forever.
pub async fn run(master: &'static mut Master, shared: &Shared) -> ! {
    let mut rr = 0;
    let mut next_report = Instant::now() + REPORT_EVERY;
    loop {
        if let Some(end) = master.last_end {
            Timer::at(end + SLOT_GAP).await;
        }
        let any_present = master.slots.iter().any(|s| s.present);
        let asserted = master.irq.is_high() == IRQ_ACTIVE_HIGH;
        if !asserted {
            master.irq_since = None;
        }
        if any_present && asserted {
            let since = *master.irq_since.get_or_insert_with(Instant::now);
            for k in 0..SLOTS {
                let i = (rr + k) % SLOTS;
                if !master.slots[i].present {
                    continue;
                }
                for _ in 0..MORE_MAX {
                    let outcome = master.exchange(i, Some(since), shared).await;
                    if !matches!(outcome, Outcome::Good { more: true }) {
                        break;
                    }
                }
            }
            rr = (rr + 1) % SLOTS;
            let now = Instant::now();
            for i in 0..SLOTS {
                if !master.slots[i].present && master.slots[i].due(now) {
                    master.exchange(i, None, shared).await;
                }
            }
        } else {
            let due = master
                .slots
                .iter()
                .map(Slot::next_due)
                .min()
                .unwrap_or(next_report)
                .min(next_report);
            // With IRQ already asserted and nobody present, only the timer can wake this.
            if any_present {
                let level = if IRQ_ACTIVE_HIGH { Event::HighLevel } else { Event::LowLevel };
                if let Either::First(()) = select(master.irq.wait_for(level), Timer::at(due)).await
                {
                    continue;
                }
            } else {
                Timer::at(due).await;
            }
            let now = Instant::now();
            for i in 0..SLOTS {
                if master.slots[i].due(now) {
                    master.exchange(i, None, shared).await;
                }
            }
        }
        let now = Instant::now();
        if now >= next_report {
            next_report = now + REPORT_EVERY;
            master.report(shared);
        }
    }
}

impl Master {
    /// One transfer with slot `i` after its gap, counted, and retried once after the gap when
    /// a present slot fails the magic. `irq_since` is when IRQ was first seen asserted, if
    /// this transfer answers it.
    async fn exchange(&mut self, i: usize, irq_since: Option<Instant>, shared: &Shared) -> Outcome {
        let mut retried = false;
        loop {
            if let Some(end) = self.slots[i].last_end {
                Timer::at(end + SLOT_GAP).await;
            }
            // Only this takes the wire, and it is put back below.
            let Some(Wire { spi, rx, mut tx }) = self.wire.take() else { return Outcome::Bad };
            let slot = &mut self.slots[i];
            let header = Header { flags: 0, seq: slot.seq, ack: slot.peer.ack() };
            seal(tx_bytes(&mut tx), header, 0);
            slot.seq = slot.seq.wrapping_add(1);
            let start = Instant::now();
            self.cs[i].set_low();
            // Bound before the await, so the `Result` is not held across it.
            let started = spi.transfer_buffers(TRANSFER_LEN, rx, TRANSFER_LEN, tx);
            let (wire, clocked) = match started {
                Ok(mut transfer) => {
                    transfer.wait_for_done().await;
                    let (spi, (rx, tx)) = transfer.wait();
                    (Wire { spi, rx, tx }, true)
                }
                // esp-hal refuses only a transfer past its DMA limit, which this is not.
                Err((_, spi, rx, tx)) => (Wire { spi, rx, tx }, false),
            };
            self.cs[i].set_high();
            let end = Instant::now();
            self.last_end = Some(end);
            self.slots[i].last_start = Some(start);
            self.slots[i].last_end = Some(end);
            let outcome = if clocked {
                self.count(i, rx_bytes(&wire.rx), start, irq_since, shared)
            } else {
                Outcome::Bad
            };
            self.wire = Some(wire);
            if matches!(outcome, Outcome::Empty) && self.slots[i].present && !retried {
                retried = true;
                self.slots[i].retries = self.slots[i].retries.wrapping_add(1);
                continue;
            }
            return outcome;
        }
    }

    /// Count what slot `i` sent, and note its arrival or departure.
    fn count(
        &mut self,
        i: usize,
        buf: &[u8; TRANSFER_LEN],
        start: Instant,
        irq_since: Option<Instant>,
        shared: &Shared,
    ) -> Outcome {
        let slot = &mut self.slots[i];
        let outcome = match verify(buf) {
            Ok(transfer) => {
                slot.ok = slot.ok.wrapping_add(1);
                slot.misses = 0;
                if slot.peer.note(transfer.header.seq) == SeqNote::Gap {
                    slot.gaps = slot.gaps.wrapping_add(1);
                }
                // Counted, not forwarded, until the bus carries the fleet.
                let records = transfer
                    .records()
                    .filter(|r| r.record_type() == Some(RecordType::NodeFrame))
                    .count();
                slot.records = slot.records.wrapping_add(records as u32);
                if records > 0
                    && let Some(since) = irq_since
                {
                    let us = start.saturating_duration_since(since).as_micros();
                    slot.irq_us_sum = slot.irq_us_sum.saturating_add(us);
                    slot.irq_us_max = slot.irq_us_max.max(us);
                    slot.irq_n = slot.irq_n.wrapping_add(1);
                }
                Outcome::Good { more: transfer.header.more() }
            }
            Err(VerifyError::BadMagic) => {
                slot.magic = slot.magic.wrapping_add(1);
                slot.misses = slot.misses.saturating_add(1);
                Outcome::Empty
            }
            Err(VerifyError::BadCrc) => {
                slot.crc = slot.crc.wrapping_add(1);
                Outcome::Bad
            }
            Err(VerifyError::OtherVersion(_)) => {
                slot.version = slot.version.wrapping_add(1);
                Outcome::Bad
            }
            Err(VerifyError::BadLen) => {
                slot.len = slot.len.wrapping_add(1);
                Outcome::Bad
            }
        };
        if !slot.present && matches!(outcome, Outcome::Good { .. }) {
            slot.present = true;
            self.line(shared, i, format_args!("appeared"));
        } else if slot.present && slot.misses >= ABSENT_AFTER {
            slot.present = false;
            self.line(shared, i, format_args!("gone after {ABSENT_AFTER} transfers without magic"));
        }
        outcome
    }

    /// Send each present slot's counters to the host.
    fn report(&self, shared: &Shared) {
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.present {
                continue;
            }
            let avg = slot.irq_us_sum.checked_div(u64::from(slot.irq_n)).unwrap_or(0);
            self.line(
                shared,
                i,
                format_args!(
                    "ok={} magic={} crc={} ver={} len={} gap={} retry={} rec={} irq_us={}/{}",
                    slot.ok,
                    slot.magic,
                    slot.crc,
                    slot.version,
                    slot.len,
                    slot.gaps,
                    slot.retries,
                    slot.records,
                    avg,
                    slot.irq_us_max
                ),
            );
        }
    }

    /// One log line about slot `i`, cut short rather than dropped when it is too long.
    #[inline(never)]
    fn line(&self, shared: &Shared, i: usize, what: core::fmt::Arguments<'_>) {
        let mut message = LogStr::new();
        let _ = write!(message, "bus {}{} {}", self.name, i + 1, what);
        shared.push_line(Some(LogLevel::Info), message);
    }
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
