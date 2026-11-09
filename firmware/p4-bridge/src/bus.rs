//! The P4's end of the node bus: two SPI controllers, each with up to [`PER_BUS`] nodes behind its
//! own CS (`docs/node-bus.md`). The board `main` read at boot sets how many each drives: three,
//! five or seven on a carrier, and none without one.
//!
//! `wartui_proto::bus` has the framing, the handshake and the acknowledgment. The P4 checks
//! framing, and reads only heartbeats, which its fleet planner takes (`src/fleet.rs`). Each record
//! a node sends goes to the host as a `FromNode`, heartbeats included, and its transfer is
//! acknowledged in the next one to that slot. Each `ToNode` the host sends, and each `Assign` and
//! `Stop` the planner sends, waits in its slot's [`Downlink`] until a transfer carries it and the
//! node acknowledges it, and is answered `Delivered`, or `Undelivered` with why: no node in the
//! slot, a full queue, a body too long, or no ack after every try. The planner's answers go to it
//! and the host's to the host, split by id ([`fleet::answer`]). The [`Downlink`] keeps two flights out, numbered,
//! and tags its first to a slot, and its first after giving up, with `SYNC`, since a P4 cannot
//! know where a node's numbering stands. A slot that loses its node answers for everything it
//! held, so its next flight carries `SYNC` too. A header with no ack, from a node that has taken
//! nothing since boot, sends the oldest flight out again with `SYNC`. The host reader queues a
//! record and raises its bus's signal, so a waiting scheduler wakes. The one record the P4
//! writes, an `UpdateChunk` it builds from its node image cache for a `ToNodeChunk`
//! (`src/cache.rs`), is queued and answered the same way.
//!
//! **Each bus runs on its own**, as a future in the main `join`, so a slow node on one never
//! holds up the other. Within a bus, transfers are one at a time, and the scheduler picks
//! the next:
//!
//! - **IRQ asserted**: a pass over the present slots, round robin, one transfer each, that ends
//!   at the first transfer to bring a record, and the next pass starts after that slot. A shared
//!   line says only that some node waits, so clocking the rest once one has answered spends the
//!   bus on nodes with nothing to send, or erasing flash. Absent slots are still probed on time,
//!   since a node not yet present may be the one asserting.
//!   IRQ is read again only once the node clocked last has had [`SLOT_GAP`] to re-arm, since
//!   until then its IRQ still says what the transfer just carried.
//! - **IRQ stuck**: [`EMPTY_PASSES`] passes in a row that clock every present slot and bring no
//!   record mean the line is asserted by nothing this bus can serve, such as a node not yet
//!   present. A pass that skips a slot waiting out [`MISS_BACKOFF`] does not count, since that
//!   slot's node may be the one asserting. The bus counts a stuck line
//!   and runs the schedule below until IRQ is seen deasserted, a slot becomes present, or
//!   [`STUCK_RETRY`] has passed. The retry is needed because a shared line under that schedule
//!   may never deassert: each node holds it while its record waits for a poll, and a few nodes
//!   together can hold it without a break.
//! - **Work for a node**: a present slot with records due, or that owes its node an ack, is
//!   clocked at once. One whose records wait for the node's ack is clocked once right after the
//!   transfer that carried them, since the node stamps its ack on the very next transfer it arms
//!   (`firmware/node/src/bus.rs`). That transfer carries the next flight when one is queued, so a
//!   stream of records costs a transfer each. A header that leaves the oldest flight
//!   unacknowledged makes the window due again: at once the first time, and after
//!   `RESEND_GAP_MS` each time again. With nothing due, the slot is clocked every [`ACK_POLL`]
//!   while flights wait, and no later than the [`Downlink`]'s deadline.
//! - **Otherwise**: each present slot every [`POLL_EVERY`], and each absent one every
//!   [`PROBE_EVERY`]. A slot is present after one good transfer, and absent after
//!   [`ABSENT_AFTER`] transfers in a row that fail the magic, once [`ABSENT_SPAN`] has passed
//!   since the last that had it. A node erasing flash cannot re-arm, so it fails the magic
//!   until the erase ends, and a miss count alone would drop it mid-update. Each change of
//!   presence goes to the fleet planner, which drops a node whose slot is absent, and each
//!   `Status` tells the host which slots are present.
//! - **Every transfer to a slot waits [`SLOT_GAP`] after the last**, so the node has re-armed.
//!   A present slot that fails the magic is retried once after the gap and counted as a retry.
//!   If the retry fails too, the slot is not clocked again for [`MISS_BACKOFF`], in an IRQ pass
//!   or as due work, so the bus does not spend its transfers on a node that cannot answer yet.
//!
//! Each slot has its own seq, so a node's gaps count only transfers meant for it. Log lines name a
//! slot as the operator reads it, `N3` on a carrier ([`NodeBoard::label`]).
//!
//! **IRQ is active low.** The carrier inverts each node's D0 onto a shared line it pulls up, and
//! the P4 pulls it up too, so a bus with no node asking reads high.

use core::cell::{Cell, RefCell};
use core::fmt::Write;

use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
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
    AckTracker, Downlink, Header, NodeBoard, SeqCheck, SeqNote, Slot as BusSlot, TRANSFER_LEN,
    UpType, VerifyError, verify,
};
use wartui_proto::link::{BridgeToHost, FromNodeBody, LogStr, QueueId, QueueStat, Undelivered};

use crate::config::NODE_DOWN_BYTES;
use crate::{Shared, fleet};

/// The bus clock, measured on the carrier.
pub const BUS_HZ: u32 = 8_000_000;
/// Slots on one bus, as many as any board drives.
pub const PER_BUS: usize = wartui_proto::bus::PER_BUS as usize;
/// Slots across both buses.
const DRIVEN: usize = 2 * PER_BUS;
/// The longest a present slot goes unclocked.
const POLL_EVERY: Duration = Duration::from_millis(250);
/// How often a slot whose records wait for the node's ack is clocked, once the transfer right
/// after theirs brought no ack. The node stamps its ack only once it has queued the records, so
/// the ack arrives on a later transfer than the records: normally the next one, which
/// [`SLOT_GAP`] spaces, and otherwise one of these.
const ACK_POLL: Duration = Duration::from_millis(25);
/// How often an absent slot is probed.
const PROBE_EVERY: Duration = Duration::from_millis(2_000);
/// Transfers in a row failing the magic before a present slot is absent.
const ABSENT_AFTER: u8 = 5;
/// The least time from a present slot's last transfer with the magic to its absence. It outlasts
/// a node's 64 KB flash block erase, its longest time deaf to the bus, whose timings are in
/// `wartui_proto::ota`'s "Erase is lazy". A lost supply still takes every slot absent well inside
/// `wartui_proto::carrier::DROP_WINDOW_MS` of the others: they all start missing at once, and
/// each goes within a [`POLL_EVERY`] either side of this after the loss.
const ABSENT_SPAN: Duration = Duration::from_millis(1_000);
/// How long a present slot whose transfer and retry both failed the magic waits before it is
/// clocked again: about half a node's sector erase, its shortest, so a node erasing flash is
/// clocked a few times through an erase rather than on every pass.
const MISS_BACKOFF: Duration = Duration::from_millis(20);
/// The least time between the end of one transfer to a slot and the start of the next, for
/// the node to re-arm. The P4–C6 bench measured a re-arm at about 320 µs.
const SLOT_GAP: Duration = Duration::from_millis(1);
/// IRQ passes in a row, each clocking every present slot, with no record before the line counts
/// as stuck. Two, because the first pass after a node queues a record clocks the transfer it
/// armed before, which is empty.
const EMPTY_PASSES: u8 = 2;
/// How long a stuck bus waits before trying IRQ service again. A line that really is held costs
/// [`EMPTY_PASSES`] empty passes per retry, about 1.5% of the bus at 8 MHz with five nodes.
const STUCK_RETRY: Duration = Duration::from_millis(1_000);
/// How often each present slot's counters go to the host.
const REPORT_EVERY: Duration = Duration::from_millis(5_000);

/// One bus's pins, as `main` hands them over.
pub struct Pins {
    pub sck: AnyPin<'static>,
    pub mosi: AnyPin<'static>,
    pub miso: AnyPin<'static>,
    pub irq: AnyPin<'static>,
    /// Slot 1 first: a pin for each slot the board drives, then `None`.
    pub cs: [Option<AnyPin<'static>>; PER_BUS],
}

/// One slot's queue toward its node.
type NodeDown = Downlink<NODE_DOWN_BYTES>;

/// Every driven slot's queue, bus A's first. Static: each holds two flights and a queue.
static DOWNLINKS: ConstStaticCell<[NodeDown; DRIVEN]> =
    ConstStaticCell::new([const { Downlink::new() }; DRIVEN]);

/// What the host reader and both buses share: which slots hold a node, each slot's queue toward
/// it, and the counts of what came up.
pub struct Nodes {
    /// Slots per bus the board drives.
    per_bus: u8,
    /// Indexed by [`Nodes::driven`].
    downlinks: RefCell<&'static mut [NodeDown; DRIVEN]>,
    /// Bit `k` is set while driven slot `k` holds a node. `k` is the slot's [`BusSlot`] byte.
    present: Cell<u32>,
    /// One per bus, raised when the host queues a record for one of its slots.
    queued: [Signal<NoopRawMutex, ()>; 2],
    /// Node records forwarded to the host.
    rx_count: Cell<u32>,
    /// Node records longer than [`MAX_NODE_PAYLOAD`](wartui_proto::link::MAX_NODE_PAYLOAD),
    /// dropped.
    rx_oversize: Cell<u32>,
}

impl Nodes {
    /// No node yet, on a board driving `per_bus` slots on each bus. Called once: the queues are
    /// a static.
    pub fn new(per_bus: u8) -> Self {
        Self {
            per_bus,
            downlinks: RefCell::new(DOWNLINKS.take()),
            present: Cell::new(0),
            queued: [Signal::new(), Signal::new()],
            rx_count: Cell::new(0),
            rx_oversize: Cell::new(0),
        }
    }

    /// The queues toward the nodes, for `Status`: one slot's capacity, the deepest any slot has
    /// been, and every slot's refusals.
    pub fn stat(&self) -> QueueStat {
        let downlinks = self.downlinks.borrow();
        QueueStat {
            queue: QueueId::NodeDown,
            capacity: NODE_DOWN_BYTES as u32,
            high_water: downlinks.iter().map(|d| d.high_water() as u32).max().unwrap_or(0),
            dropped: downlinks.iter().fold(0, |sum: u32, d| sum.saturating_add(d.refused())),
        }
    }

    /// Node records forwarded, and dropped for their length, since boot.
    pub fn rx_counts(&self) -> (u32, u32) {
        (self.rx_count.get(), self.rx_oversize.get())
    }

    /// Bit `k` is set while driven slot `k` holds a node. `k` is the slot's [`BusSlot`] byte.
    pub fn present(&self) -> u32 {
        self.present.get()
    }

    fn is_present(&self, k: usize) -> bool {
        self.present.get() & (1 << k) != 0
    }

    fn set_present(&self, k: usize, present: bool) {
        let bits = self.present.get();
        self.present.set(if present { bits | 1 << k } else { bits & !(1 << k) });
    }

    /// The index of `slot` among the driven slots, or `None` for one the board does not drive.
    fn driven(&self, slot: BusSlot) -> Option<usize> {
        (slot.index() < self.per_bus).then(|| usize::from(slot.as_u8()))
    }
}

/// Queue one record from the host for the node in `slot`, or answer at once why not.
pub fn to_node(shared: &Shared, slot: BusSlot, id: u16, kind: u8, body: &[u8]) {
    if let Err(reason) = push_down(shared, slot, id, kind, body) {
        shared.push_host(&BridgeToHost::Undelivered { id, reason });
    }
}

/// Queue one record for the node in `slot` and wake its bus, or say why not.
pub fn push_down(
    shared: &Shared,
    slot: BusSlot,
    id: u16,
    kind: u8,
    body: &[u8],
) -> Result<(), Undelivered> {
    let nodes = &shared.nodes;
    match nodes.driven(slot) {
        Some(k) if nodes.is_present(k) => nodes.downlinks.borrow_mut()[k].push(id, kind, body)?,
        _ => return Err(Undelivered::NoNode),
    }
    nodes.queued[usize::from(slot.bus())].signal(());
    Ok(())
}

/// Each bus's slots and controller, in statics so its future stays small.
static SLOTS_A: ConstStaticCell<[Slot; PER_BUS]> =
    ConstStaticCell::new([const { Slot::new() }; PER_BUS]);
static SLOTS_B: ConstStaticCell<[Slot; PER_BUS]> =
    ConstStaticCell::new([const { Slot::new() }; PER_BUS]);
static CONTROLLER_A: StaticCell<Controller> = StaticCell::new();
static CONTROLLER_B: StaticCell<Controller> = StaticCell::new();

/// Build both buses for `board`, each driving its first `per_bus` slots.
/// Called once: the buffers and controllers are statics.
///
/// Every CS line goes high before either controller is built, so no node sees a transfer start.
/// Out of line, so what it builds is on this frame rather than `main`'s.
#[inline(never)]
pub fn build(
    board: NodeBoard,
    per_bus: u8,
    (spi2, dma_a, a): (SPI2<'static>, DMA_AXI_CH0<'static>, Pins),
    (spi3, dma_b, b): (SPI3<'static>, DMA_AXI_CH1<'static>, Pins),
) -> (&'static mut Controller, &'static mut Controller) {
    let high = |pin: Option<AnyPin<'static>>| {
        pin.map(|pin| Output::new(pin, Level::High, OutputConfig::default()))
    };
    let per_bus = usize::from(per_bus);
    debug_assert!([&a.cs, &b.cs].iter().all(|cs| cs[..per_bus].iter().all(Option::is_some)));
    let cs_a = a.cs.map(high);
    let cs_b = b.cs.map(high);
    let pulled_up = |pin| Input::new(pin, InputConfig::default().with_pull(Pull::Up));
    let config = Config::default().with_frequency(Rate::from_hz(BUS_HZ)).with_mode(Mode::_0);

    let miso_a = pulled_up(a.miso);
    let spi_a = Spi::new(spi2, config)
        .expect("SPI2")
        .with_sck(a.sck)
        .with_mosi(a.mosi)
        .with_miso(miso_a.peripheral_input())
        .with_dma(dma_a)
        .into_async();
    let controller_a = CONTROLLER_A.init(Controller {
        bus: 0,
        board,
        per_bus,
        cs: cs_a,
        irq: pulled_up(a.irq),
        _miso: miso_a,
        wire: Some(Wire {
            spi: spi_a,
            rx: dma_buf!(DmaRxBuf, TRANSFER_LEN),
            tx: dma_buf!(DmaTxBuf, TRANSFER_LEN),
        }),
        slots: SLOTS_A.take(),
        last_end: None,
        irq_since: None,
        empty_passes: 0,
        stuck_at: None,
        stuck: 0,
        irq_us_sum: 0,
        irq_us_max: 0,
        irq_n: 0,
    });

    let miso_b = pulled_up(b.miso);
    let spi_b = Spi::new(spi3, config)
        .expect("SPI3")
        .with_sck(b.sck)
        .with_mosi(b.mosi)
        .with_miso(miso_b.peripheral_input())
        .with_dma(dma_b)
        .into_async();
    let controller_b = CONTROLLER_B.init(Controller {
        bus: 1,
        board,
        per_bus,
        cs: cs_b,
        irq: pulled_up(b.irq),
        _miso: miso_b,
        wire: Some(Wire {
            spi: spi_b,
            rx: dma_buf!(DmaRxBuf, TRANSFER_LEN),
            tx: dma_buf!(DmaTxBuf, TRANSFER_LEN),
        }),
        slots: SLOTS_B.take(),
        last_end: None,
        irq_since: None,
        empty_passes: 0,
        stuck_at: None,
        stuck: 0,
        irq_us_sum: 0,
        irq_us_max: 0,
        irq_n: 0,
    });
    (controller_a, controller_b)
}

/// The controller and the buffers it hands back after each transfer.
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
    /// When the last transfer with the magic ended.
    heard: Option<Instant>,
    /// The P4's seq for this slot's next transfer.
    seq: u16,
    /// The node's seq.
    peer: SeqCheck,
    /// The node's last record-carrying transfer, for the ack.
    tracker: AckTracker,
    /// The node's last good transfer carried records, and no transfer since has carried the
    /// ack.
    ack_owed: bool,
    /// The last transfer carried a flight, so the next, which the node arms with its ack, is
    /// due as soon as [`SLOT_GAP`] allows rather than after [`ACK_POLL`].
    fetch_ack: bool,
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
    xfers: Xfers,
    /// The longest span this report period, in ms, that the slot went without a transfer with the
    /// magic while present: from the end of the last before a run of misses to the end of the
    /// next. A transfer with a bad CRC, length or version has the magic, so it ends a span. A span
    /// is recorded when the node answers again: one still open at a report lands in the next
    /// period, and one that ends with the slot going absent shows only as its `gone` line. Reset
    /// after each report, so a long span in one period does not hide later ones.
    deaf_ms: u32,
}

impl Slot {
    /// Absent and never clocked.
    const fn new() -> Self {
        Self {
            present: false,
            misses: 0,
            heard: None,
            seq: 0,
            peer: SeqCheck::new(),
            tracker: AckTracker::new(),
            ack_owed: false,
            fetch_ack: false,
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
            xfers: Xfers::new(),
            deaf_ms: 0,
        }
    }

    /// When this slot is next owed a transfer, given its queue toward the node.
    fn next_due(&self, downlink: &NodeDown) -> Instant {
        let Some(last) = self.last_start else { return Instant::MIN };
        if !self.present {
            return last + PROBE_EVERY;
        }
        let due = if self.ack_owed || downlink.has_work() {
            Instant::MIN
        } else {
            match downlink.deadline_ms() {
                Some(_) if self.fetch_ack => Instant::MIN,
                Some(deadline) => (last + ACK_POLL).min(Instant::from_millis(deadline)),
                None => last + POLL_EVERY,
            }
        };
        due.max(self.held_until())
    }

    /// Until when this slot waits out a miss: [`MISS_BACKOFF`] after a present slot's last
    /// transfer failed the magic, retry included, and otherwise [`Instant::MIN`].
    fn held_until(&self) -> Instant {
        match self.last_end {
            Some(end) if self.present && self.misses > 0 => end + MISS_BACKOFF,
            _ => Instant::MIN,
        }
    }

    /// Whether an IRQ pass may clock this slot at `now`: present, and not waiting out a miss.
    fn clockable(&self, now: Instant) -> bool {
        self.present && self.held_until() <= now
    }
}

/// Why a transfer ran, for its slot's `xfer` counts, each counted under the first that applies.
/// A retry after a miss counts as the transfer it retries.
#[derive(Clone, Copy)]
enum Xfer {
    /// Clocked in an IRQ pass.
    Pass,
    /// Carried a flight toward the node, outside a pass.
    Carry,
    /// Fetched the node's ack right after a flight, carrying none.
    Fetch,
    /// Owed the node an ack.
    Owed,
    /// The periodic poll, a poll before a flight's deadline, or an absent slot's probe.
    Poll,
}

/// A slot's transfers by why they ran, since boot.
struct Xfers {
    pass: u32,
    carry: u32,
    fetch: u32,
    owed: u32,
    poll: u32,
}

impl Xfers {
    const fn new() -> Self {
        Self { pass: 0, carry: 0, fetch: 0, owed: 0, poll: 0 }
    }

    fn note(&mut self, why: Xfer) {
        let count = match why {
            Xfer::Pass => &mut self.pass,
            Xfer::Carry => &mut self.carry,
            Xfer::Fetch => &mut self.fetch,
            Xfer::Owed => &mut self.owed,
            Xfer::Poll => &mut self.poll,
        };
        *count = count.wrapping_add(1);
    }
}

/// What one transfer found.
enum Outcome {
    /// A good transfer, and whether it carried a record.
    Good { records: bool },
    /// Nothing there: the magic failed.
    Empty,
    /// Something there, but corrupted, or not this build's.
    Bad,
}

/// One bus and its slots.
pub struct Controller {
    /// 0 for A, 1 for B.
    bus: u8,
    /// What carries the nodes, which names the slots in log lines.
    board: NodeBoard,
    /// Slots this bus drives, from slot 1.
    per_bus: usize,
    /// One per driven slot, high.
    cs: [Option<Output<'static>>; PER_BUS],
    /// Pulled up, and asserted low.
    irq: Input<'static>,
    /// Held so its pull stays: an empty slot then reads constant bytes, which fail the magic.
    _miso: Input<'static>,
    /// Out only while a transfer runs.
    wire: Option<Wire>,
    slots: &'static mut [Slot; PER_BUS],
    /// When the last transfer on this bus ended.
    last_end: Option<Instant>,
    /// When IRQ was first seen asserted, until a record arrives or [`Controller::unlatch`].
    irq_since: Option<Instant>,
    /// IRQ passes in a row that brought nothing.
    empty_passes: u8,
    /// When IRQ was found stuck, until [`Controller::unlatch`].
    stuck_at: Option<Instant>,
    /// Times IRQ was found stuck. A line that stays held latches again after each retry, so
    /// this grows about once every [`STUCK_RETRY`].
    stuck: u32,
    /// From IRQ first seen asserted to the start of the transfer that carried a record.
    irq_us_sum: u64,
    irq_us_max: u64,
    irq_n: u32,
}

/// Run one bus forever. A bus that drives no slot, with no carrier or a broken one, waits forever.
pub async fn run(controller: &'static mut Controller, shared: &Shared) -> ! {
    if controller.per_bus == 0 {
        core::future::pending().await
    }
    let per_bus = controller.per_bus;
    let mut rr = 0;
    let mut next_report = Instant::now() + REPORT_EVERY;
    let queued = &shared.nodes.queued[usize::from(controller.bus)];
    loop {
        if let Some(end) = controller.last_end {
            Timer::at(end + SLOT_GAP).await;
        }
        let any_present = controller.slots.iter().any(|s| s.present);
        let asserted = controller.irq.is_low();
        let retry = controller.stuck_at.is_some_and(|at| Instant::now() >= at + STUCK_RETRY);
        if !asserted || retry {
            controller.unlatch();
        }
        let serving = any_present && asserted && controller.stuck_at.is_none();
        let held = serving.then(|| controller.all_held(Instant::now())).flatten();
        if serving && held.is_none() {
            controller.irq_since.get_or_insert_with(Instant::now);
            let mut brought = false;
            // A present slot waiting out a miss, whose node may be the one asserting.
            let mut skipped = false;
            for k in 0..per_bus {
                let i = (rr + k) % per_bus;
                if !controller.slots[i].clockable(Instant::now()) {
                    skipped |= controller.slots[i].present;
                    continue;
                }
                let outcome = controller.exchange(i, true, shared).await;
                if let Outcome::Good { records: true } = outcome {
                    brought = true;
                    rr = (i + 1) % per_bus;
                    break;
                }
            }
            if brought {
                controller.empty_passes = 0;
            } else {
                rr = (rr + 1) % per_bus;
                if !skipped {
                    controller.empty_passes = controller.empty_passes.saturating_add(1);
                    if controller.empty_passes >= EMPTY_PASSES {
                        controller.stuck_at = Some(Instant::now());
                        controller.stuck = controller.stuck.wrapping_add(1);
                    }
                }
            }
            let now = Instant::now();
            for i in 0..per_bus {
                if !controller.slots[i].present && controller.due(i, now, shared) {
                    controller.exchange(i, false, shared).await;
                }
            }
        } else {
            let mut due = (0..per_bus)
                .map(|i| controller.next_due(i, shared))
                .min()
                .unwrap_or(next_report)
                .min(next_report);
            if let Some(at) = controller.stuck_at {
                due = due.min(at + STUCK_RETRY);
            }
            // With IRQ already asserted and nobody present, only the timer can wake this, and
            // the host queues nothing for an absent slot. Stuck, it waits for the deassert or
            // the retry instead.
            if let Some(until) = held {
                // Every present slot waits out a miss under an asserted IRQ, so only the timer.
                Timer::at(due.min(until)).await;
            } else if any_present {
                // Asserted, or deasserted when stuck.
                let level =
                    if controller.stuck_at.is_some() { Event::HighLevel } else { Event::LowLevel };
                let woke = select3(controller.irq.wait_for(level), Timer::at(due), queued.wait());
                if let Either3::First(()) = woke.await {
                    continue;
                }
            } else {
                Timer::at(due).await;
            }
            let now = Instant::now();
            for i in 0..per_bus {
                if controller.due(i, now, shared) {
                    controller.exchange(i, false, shared).await;
                }
            }
        }
        let now = Instant::now();
        if now >= next_report {
            next_report = now + REPORT_EVERY;
            controller.report(shared);
        }
    }
}

impl Controller {
    /// The index of slot `i` of this bus among the driven slots.
    fn driven(&self, i: usize) -> usize {
        usize::from(self.bus) * PER_BUS + i
    }

    /// The slot id of slot `i` of this bus, as the host names it.
    fn slot_id(&self, i: usize) -> BusSlot {
        // `i` is below `per_bus`, which is at most PER_BUS.
        BusSlot::new(self.bus, i as u8).expect("a driven slot")
    }

    /// The CS line of slot `i`, which every driven slot has.
    fn cs(&mut self, i: usize) -> &mut Output<'static> {
        self.cs[i].as_mut().expect("a driven slot's CS")
    }

    /// Clear the stuck latch and start IRQ service afresh, so a line still held latches again
    /// after [`EMPTY_PASSES`] empty passes.
    fn unlatch(&mut self) {
        self.irq_since = None;
        self.empty_passes = 0;
        self.stuck_at = None;
    }

    /// When every present slot waits out a miss at `now`, the earliest any may be clocked again.
    fn all_held(&self, now: Instant) -> Option<Instant> {
        let mut present = self.slots[..self.per_bus].iter().filter(|s| s.present);
        present.try_fold(Instant::MAX, |at, s| (!s.clockable(now)).then(|| at.min(s.held_until())))
    }

    /// When slot `i` is next owed a transfer.
    fn next_due(&self, i: usize, shared: &Shared) -> Instant {
        let downlinks = shared.nodes.downlinks.borrow();
        self.slots[i].next_due(&downlinks[self.driven(i)])
    }

    /// Whether slot `i` is owed a transfer at `now`.
    fn due(&self, i: usize, now: Instant, shared: &Shared) -> bool {
        now >= self.next_due(i, shared)
    }

    /// One transfer with slot `i` after its gap, counted, and retried once after the gap when
    /// a present slot fails the magic. `answers_irq` is whether it is part of an IRQ pass.
    async fn exchange(&mut self, i: usize, answers_irq: bool, shared: &Shared) -> Outcome {
        let mut retried = false;
        let mut why = None;
        loop {
            if let Some(end) = self.slots[i].last_end {
                Timer::at(end + SLOT_GAP).await;
            }
            // Only this takes the wire, and it is put back below.
            let Some(Wire { spi, rx, mut tx }) = self.wire.take() else { return Outcome::Bad };
            let seq = self.slots[i].seq;
            let filled = self.prepare(i, tx_bytes(&mut tx), shared);
            let slot = &mut self.slots[i];
            let why = *why.get_or_insert(if answers_irq {
                Xfer::Pass
            } else if filled > 0 {
                Xfer::Carry
            } else if slot.fetch_ack {
                Xfer::Fetch
            } else if slot.ack_owed {
                Xfer::Owed
            } else {
                Xfer::Poll
            });
            slot.xfers.note(why);
            slot.seq = seq.wrapping_add(1);
            let start = Instant::now();
            self.cs(i).set_low();
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
            self.cs(i).set_high();
            let end = Instant::now();
            self.last_end = Some(end);
            self.slots[i].last_start = Some(start);
            self.slots[i].last_end = Some(end);
            let outcome = if clocked {
                // A transfer with the magic carried the ack, whatever the node made of it, and
                // brought the node's, if it had stamped one. One without it reached no armed
                // node, so both stay owed while the node is present.
                let owed = (self.slots[i].ack_owed, self.slots[i].fetch_ack);
                self.slots[i].ack_owed = false;
                self.slots[i].fetch_ack = false;
                let outcome =
                    self.count(i, seq, rx_bytes(&wire.rx), (start, end), answers_irq, shared);
                if matches!(outcome, Outcome::Empty) && self.slots[i].present {
                    (self.slots[i].ack_owed, self.slots[i].fetch_ack) = owed;
                }
                // Bad magic: the node was not armed and took nothing, so the flight stays due
                // and the retry carries it. Anything else may have reached the node.
                if filled > 0 && !matches!(outcome, Outcome::Empty) {
                    let k = self.driven(i);
                    shared.nodes.downlinks.borrow_mut()[k].sent(seq, crate::now_ms());
                    self.slots[i].fetch_ack = true;
                }
                outcome
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

    /// Fill the next transfer to slot `i` in `buf` and seal it: answer for the records its queue
    /// gave up on, then carry what the queue has due. Returns the payload's length.
    #[inline(never)]
    fn prepare(&self, i: usize, buf: &mut [u8; TRANSFER_LEN], shared: &Shared) -> usize {
        let slot = &self.slots[i];
        let header = Header::new(slot.seq, slot.tracker.ack());
        let (gave_up, len) = {
            let mut downlinks = shared.nodes.downlinks.borrow_mut();
            let downlink = &mut downlinks[self.driven(i)];
            let gave_up = downlink.poll(crate::now_ms());
            (gave_up, downlink.fill(buf, header))
        };
        for id in gave_up.into_iter().flatten() {
            fleet::answer(shared, id, fleet::Outcome::Undelivered(Undelivered::Timeout));
        }
        len
    }

    /// Take what slot `i` sent in transfer `seq`, which ran from `start` to `end`, and note its
    /// arrival or departure.
    fn count(
        &mut self,
        i: usize,
        seq: u16,
        buf: &[u8; TRANSFER_LEN],
        (start, end): (Instant, Instant),
        answers_irq: bool,
        shared: &Shared,
    ) -> Outcome {
        let k = self.driven(i);
        let id = self.slot_id(i);
        let slot = &mut self.slots[i];
        let (was_present, prev_misses, prev_heard) = (slot.present, slot.misses, slot.heard);
        let outcome = match verify(buf) {
            Ok(transfer) => {
                slot.ok = slot.ok.wrapping_add(1);
                slot.misses = 0;
                slot.heard = Some(end);
                if !slot.present {
                    // A node that arrives may be a new life, whose seq restarts.
                    slot.tracker = AckTracker::new();
                    slot.peer = SeqCheck::new();
                    slot.ack_owed = false;
                }
                if slot.peer.note(transfer.header.seq) == SeqNote::Gap {
                    slot.gaps = slot.gaps.wrapping_add(1);
                }
                let delivered =
                    shared.nodes.downlinks.borrow_mut()[k].on_header(&transfer.header, seq);
                for id in delivered.into_iter().flatten() {
                    fleet::answer(shared, id, fleet::Outcome::Delivered);
                }
                // On the P4's clock, wrapping after about 71 minutes. The host only compares.
                let rx_us = end.as_micros() as u32;
                let mut records = 0;
                for record in transfer.records() {
                    forward(shared, id, rx_us, record.kind, record.body);
                    records += 1;
                }
                if transfer.carries_records() {
                    slot.tracker.note_records(transfer.header.seq);
                    slot.ack_owed = true;
                }
                slot.records = slot.records.wrapping_add(records);
                if records > 0
                    && answers_irq
                    && let Some(since) = self.irq_since.take()
                {
                    let us = start.saturating_duration_since(since).as_micros();
                    self.irq_us_sum = self.irq_us_sum.saturating_add(us);
                    self.irq_us_max = self.irq_us_max.max(us);
                    self.irq_n = self.irq_n.wrapping_add(1);
                }
                Outcome::Good { records: records > 0 }
            }
            Err(VerifyError::BadMagic) => {
                slot.magic = slot.magic.wrapping_add(1);
                slot.misses = slot.misses.saturating_add(1);
                Outcome::Empty
            }
            // These had the magic, so something is there.
            Err(VerifyError::BadCrc) => {
                slot.crc = slot.crc.wrapping_add(1);
                slot.misses = 0;
                slot.heard = Some(end);
                Outcome::Bad
            }
            Err(VerifyError::OtherVersion(_)) => {
                slot.version = slot.version.wrapping_add(1);
                slot.misses = 0;
                slot.heard = Some(end);
                Outcome::Bad
            }
            Err(VerifyError::BadLen) => {
                slot.len = slot.len.wrapping_add(1);
                slot.misses = 0;
                slot.heard = Some(end);
                Outcome::Bad
            }
        };
        if was_present
            && prev_misses > 0
            && slot.misses == 0
            && let Some(heard) = prev_heard
        {
            let ms =
                u32::try_from(end.saturating_duration_since(heard).as_millis()).unwrap_or(u32::MAX);
            slot.deaf_ms = slot.deaf_ms.max(ms);
        }
        if !slot.present && matches!(outcome, Outcome::Good { .. }) {
            slot.present = true;
            slot.deaf_ms = 0;
            shared.nodes.set_present(k, true);
            crate::carrier::presence(shared, shared.nodes.present());
            fleet::on_presence(shared, shared.nodes.present());
            // The node asserting a stuck line is usually the one that just arrived.
            self.unlatch();
            self.line(shared, Some(i), format_args!("appeared"));
        } else if slot.present
            && slot.misses >= ABSENT_AFTER
            && slot.heard.is_none_or(|heard| end.saturating_duration_since(heard) >= ABSENT_SPAN)
        {
            slot.present = false;
            let misses = slot.misses;
            shared.nodes.set_present(k, false);
            crate::carrier::presence(shared, shared.nodes.present());
            shared.nodes.downlinks.borrow_mut()[k].drain(|id| {
                fleet::answer(shared, id, fleet::Outcome::Undelivered(Undelivered::NoNode));
            });
            fleet::on_presence(shared, shared.nodes.present());
            self.line(shared, Some(i), format_args!("gone after {misses} transfers without magic"));
        }
        outcome
    }

    /// Send the bus's IRQ counters, then each present slot's, to the host, and start each slot's
    /// next `deaf` period. Nothing while no slot is present, since IRQ is served only for present
    /// slots.
    fn report(&mut self, shared: &Shared) {
        if !self.slots.iter().any(|s| s.present) {
            return;
        }
        let avg = self.irq_us_sum.checked_div(u64::from(self.irq_n)).unwrap_or(0);
        self.line(
            shared,
            None,
            format_args!(
                "irq_us={}/{} irq_n={} stuck={}",
                avg, self.irq_us_max, self.irq_n, self.stuck
            ),
        );
        for (i, slot) in self.slots.iter().enumerate() {
            if !slot.present {
                continue;
            }
            self.line(
                shared,
                Some(i),
                format_args!(
                    "ok={} magic={} crc={} ver={} len={} gap={} retry={} rec={}",
                    slot.ok,
                    slot.magic,
                    slot.crc,
                    slot.version,
                    slot.len,
                    slot.gaps,
                    slot.retries,
                    slot.records
                ),
            );
            let x = &slot.xfers;
            self.line(
                shared,
                Some(i),
                format_args!(
                    "xfer pass={} carry={} fetch={} owed={} poll={} deaf={}",
                    x.pass, x.carry, x.fetch, x.owed, x.poll, slot.deaf_ms
                ),
            );
        }
        for slot in self.slots.iter_mut() {
            slot.deaf_ms = 0;
        }
    }

    /// One log line about the bus, or about slot `i` of it, cut short rather than dropped when
    /// it is too long.
    #[inline(never)]
    fn line(&self, shared: &Shared, i: Option<usize>, what: core::fmt::Arguments<'_>) {
        let mut message = LogStr::new();
        let name = if self.bus == 0 { 'A' } else { 'B' };
        let _ = match i {
            Some(i) => write!(message, "{} {}", self.board.label(self.slot_id(i)), what),
            None => write!(message, "bus {name} {what}"),
        };
        shared.push_log(&message);
    }
}

/// Send one record from the node in `slot` to the host, or count it dropped when it is longer
/// than any node of this build sends. A heartbeat goes to the fleet planner too, and to the host
/// unchanged.
///
/// Out of line: a `FromNode` carries a kilobyte.
#[inline(never)]
fn forward(shared: &Shared, slot: BusSlot, rx_us: u32, kind: u8, body: &[u8]) {
    let nodes = &shared.nodes;
    if kind == UpType::Heartbeat.as_u8() {
        fleet::on_heartbeat(shared, slot, body);
    }
    let Ok(body) = FromNodeBody::from_slice(body) else {
        nodes.rx_oversize.set(nodes.rx_oversize.get().wrapping_add(1));
        return;
    };
    shared.push_host(&BridgeToHost::FromNode { slot, rx_us, kind, body });
    nodes.rx_count.set(nodes.rx_count.get().wrapping_add(1));
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
