//! The node's end of the node bus, which the P4 bridge's SPI controller clocks
//! (`docs/node-bus.md`), and the node's only link to the host.
//!
//! `wartui_proto::bus` has the framing, the handshake and the messages. A heartbeat goes up
//! fire-and-forget ([`send`]): a newer one replaces one the P4 has not clocked. Sightings and
//! update statuses go up stop-and-wait ([`deliver`]), and the main loop blocks until the P4
//! acknowledges them. Records from the P4 wait in an inbox until the main loop takes them
//! ([`take`]).
//!
//! **The re-arm lives in an interrupt on CS's rising edge.** esp-hal's SPI driver does one blocking
//! transfer at a time, armed before the controller clocks, so each transfer must be followed by
//! the next arm before the controller's next one. The main loop cannot do it: it blocks for whole
//! dwells and Bluetooth scans, and esp-rtos offers no thread to hand it to. So CS is read twice,
//! by the SPI driver and by a GPIO input whose rising edge ends a transfer.
//!
//! **The handler checks every transfer, and keeps only those with records.** The P4 clocks a
//! slot it waits on every 25 ms, far more often than the main loop looks, so a queue of every
//! finished transfer would fill with idle ones and push out the P4's records. Instead the
//! handler verifies each transfer as it ends and counts what fails. A good transfer's header
//! goes to the uplink's wait ([`deliver`]) and the seq check at once. A transfer with no records
//! has nothing more to give, and its buffer goes straight back. One with records carries a
//! numbered flight, which [`FlightTaker`] takes strictly in order: the next flight waits in
//! [`Shared::received`] for the main loop ([`take`]), and a resend of the last one taken is
//! acknowledged and not queued again. A flight that finds the inbox full is dropped, counted as
//! `inbox full`, and so is every later one, counted as `out of order`, all unacknowledged. The
//! P4 sends the window again from the dropped flight once the next transfer's ack shows it
//! missing, so delivery stays in order. Until it has taken a flight since boot, the node drops
//! every flight but a `SYNC` one as `out of order`.
//!
//! **A transfer's records are acknowledged on the very next transfer.** Once it is queued, its
//! seq is the ack the handler stamps on the transfer it arms next, before the controller's next
//! clock. Then it arms that transfer: the one the main loop prepared, or an idle one it seals
//! itself. It stamps seq, ack and flags at arm time, so seq counts arms in order. Packing what
//! goes out happens in [`send`] and [`deliver`], from the main loop, and so does reading what
//! came in.
//!
//! The handler runs with interrupts masked, so what it costs is time the radio's tasks wait.
//! It and the helpers it calls live in RAM, so a flash-cache miss does not add to it; the CRC
//! is `wartui-proto`'s and runs from flash. The console line reports its longest run.
//!
//! Buffers move between the handler and the main loop through [`STATE`], under a lock held
//! only to move them.
//!
//! IRQ is high while a record waits: prepared, or in the armed transfer. The handler drops it
//! once the transfer that carried the record completes and nothing else is prepared.

use esp_hal::Blocking;
use esp_hal::dma::aligned::InternalMemory;
use esp_hal::dma::{BurstConfig, DmaDescriptor, DmaRxBuf, DmaTxBuf, descriptor_count};
use esp_hal::gpio::{Event, Input, Io, Level, Output};
use esp_hal::handler;
use esp_hal::spi::slave::dma::{SpiDma, SpiDmaTransfer};
use esp_hal::time::{Duration, Instant};
use esp_rtos::CurrentThreadHandle;
use esp_sync::NonReentrantMutex;
use static_cell::ConstStaticCell;
use wartui_proto::bus::{
    ACK_TIMEOUT_MS, AckPoll, AckTracker, AckWait, FlightTaker, HEADER_LEN, Header, PAYLOAD_MAX,
    Packer, RECORD_MAX, SeqCheck, SeqNote, TRANSFER_LEN, Take, UpType, VerifyError, seal, verify,
};
use wartui_proto::heapless::{Deque, Vec};

use crate::POLL_MS;

/// Receive buffers: one armed, up to [`RECEIVED_MAX`] waiting, and one being checked.
const RX_BUFS: usize = 4;
/// Transmit buffers: one armed, one prepared, and one being packed.
const TX_BUFS: usize = 3;
/// Transfers with records the handler holds for the main loop.
const RECEIVED_MAX: usize = RX_BUFS - 2;
/// How long [`deliver`] waits for its records to be armed.
///
/// With IRQ high the P4 clocks within milliseconds, so a second without an arm means nothing
/// is clocking this slot.
const ARM_WAIT_MS: u64 = ACK_TIMEOUT_MS;
/// How long the handler waits after CS rises for the DMA to report the transfer done.
const SETTLE: Duration = Duration::from_micros(50);
/// The bounds, in µs, of the settle histogram's spinning buckets.
const SETTLE_BUCKETS_US: [u32; 3] = [5, 20, 45];
/// How often [`service`] prints the counters.
const REPORT_EVERY: Duration = Duration::from_millis(10_000);

type Armed = SpiDmaTransfer<'static, Blocking, (DmaRxBuf, DmaTxBuf)>;

/// A packed transfer waiting for the next edge.
struct Prepared {
    tx: DmaTxBuf,
    /// Payload bytes.
    len: usize,
    /// Whether [`deliver`] waits on it. A heartbeat is not.
    reliable: bool,
}

/// What the handler and the main loop share.
struct Shared {
    cs: Input<'static>,
    irq: Output<'static>,
    /// The transfer the controller clocks next, and whether it carries a record.
    armed: Option<(Armed, bool)>,
    /// The SPI driver when nothing is armed: only after an arm that failed.
    spare: Option<SpiDma<'static, Blocking>>,
    /// Transfers with records, each with its payload's length, until [`take`] reads them.
    received: Deque<(DmaRxBuf, usize), RECEIVED_MAX>,
    rx_free: Vec<DmaRxBuf, RX_BUFS>,
    tx_free: Vec<DmaTxBuf, TX_BUFS>,
    /// Armed at the next edge.
    prepared: Option<Prepared>,
    /// The seq a reliable transfer was armed at, until [`deliver`] takes it.
    carried: Option<u16>,
    /// This node's seq for the next transfer armed.
    seq: u16,
    /// The P4's seq, across good transfers.
    peer: SeqCheck,
    /// The P4's last record-carrying transfer that was queued or repeated: the ack to stamp.
    tracker: AckTracker,
    /// Which flight from the P4 to take next.
    taker: FlightTaker,
    /// [`deliver`]'s wait for its ack, started when its records are armed.
    wait: AckWait,
    /// The P4 acknowledged the transfer [`Shared::wait`] waits on.
    acked: bool,
    /// Transfers completed.
    transfers: u32,
    good: u32,
    bad_magic: u32,
    bad_crc: u32,
    other_version: u32,
    bad_len: u32,
    gaps: u32,
    /// Transfers with records the handler could not keep. Nothing drops one today: the guard
    /// is counted rather than assumed.
    unread: u32,
    /// Flights dropped unacknowledged because [`Shared::received`] was full.
    inbox_full: u32,
    /// Flights dropped unacknowledged behind one not taken.
    out_of_order: u32,
    /// Resends of the flight last taken, acknowledged and not queued again.
    repeats: u32,
    /// The handler's longest run, edge to return, in µs.
    handler_max_us: u32,
    /// CS edges with the transfer not done: the controller ended one early.
    early: u32,
    /// Arms the SPI driver refused.
    arm_failed: u32,
    /// The longest wait after CS rose for the DMA to report the transfer done, in µs.
    settle_max_us: u32,
    /// Edges by that wait: done at the first check, then spun under each of
    /// [`SETTLE_BUCKETS_US`], then longer, timeouts included.
    settle: [u32; SETTLE_BUCKETS_US.len() + 2],
}

static STATE: NonReentrantMutex<Option<Shared>> = NonReentrantMutex::new(None);

/// What only the main loop touches.
struct Main {
    /// Heartbeats replaced by a newer one before the P4 clocked them, or dropped behind a
    /// reliable transfer.
    replaced: u32,
    /// Records [`deliver`] sent again after [`ACK_TIMEOUT_MS`].
    resent: u32,
    /// Records [`deliver`] gave up on.
    undelivered: u32,
    last_report: Option<Instant>,
}

static MAIN: NonReentrantMutex<Main> =
    NonReentrantMutex::new(Main { replaced: 0, resent: 0, undelivered: 0, last_report: None });

/// A DMA buffer of one transfer, with its descriptors, in static internal RAM. Each use has
/// its own statics.
macro_rules! dma_buf {
    ($ty:ident) => {{
        const DESCRIPTORS: usize =
            descriptor_count(TRANSFER_LEN, BurstConfig::DEFAULT.max_compatible_chunk_size());
        static BUFFER: ConstStaticCell<InternalMemory<[u8; TRANSFER_LEN]>> =
            ConstStaticCell::new(InternalMemory::new([0; TRANSFER_LEN]));
        static DESCRIPTOR_LIST: ConstStaticCell<InternalMemory<[DmaDescriptor; DESCRIPTORS]>> =
            ConstStaticCell::new(InternalMemory::new([DmaDescriptor::EMPTY; DESCRIPTORS]));
        let mut buf =
            $ty::new(DESCRIPTOR_LIST.take().get_mut().unsize(), BUFFER.take().get_mut().unsize())
                .expect("DMA buffer");
        buf.set_length(TRANSFER_LEN);
        buf
    }};
}

/// Arm the first transfer and hand the bus to the CS interrupt.
///
/// `spi` takes `cs.peripheral_input()` as its CS, so the SPI driver and the edge read one pin.
/// Called once: the buffers are statics.
pub fn start(
    spi: SpiDma<'static, Blocking>,
    mut cs: Input<'static>,
    mut irq: Output<'static>,
    io: &mut Io<'_>,
) {
    let rx = [dma_buf!(DmaRxBuf), dma_buf!(DmaRxBuf), dma_buf!(DmaRxBuf), dma_buf!(DmaRxBuf)];
    let tx = [dma_buf!(DmaTxBuf), dma_buf!(DmaTxBuf), dma_buf!(DmaTxBuf)];
    irq.set_low();
    cs.listen(Event::RisingEdge);
    let mut shared = Shared {
        cs,
        irq,
        armed: None,
        spare: None,
        received: Deque::new(),
        rx_free: Vec::new(),
        tx_free: Vec::new(),
        prepared: None,
        carried: None,
        seq: 0,
        peer: SeqCheck::new(),
        tracker: AckTracker::new(),
        taker: FlightTaker::new(),
        wait: AckWait::new(),
        acked: false,
        transfers: 0,
        good: 0,
        bad_magic: 0,
        bad_crc: 0,
        other_version: 0,
        bad_len: 0,
        gaps: 0,
        unread: 0,
        inbox_full: 0,
        out_of_order: 0,
        repeats: 0,
        handler_max_us: 0,
        early: 0,
        arm_failed: 0,
        settle_max_us: 0,
        settle: [0; SETTLE_BUCKETS_US.len() + 2],
    };
    for buf in rx {
        // RX_BUFS fits RX_BUFS.
        let _ = shared.rx_free.push(buf);
    }
    for buf in tx {
        let _ = shared.tx_free.push(buf);
    }
    shared.arm(spi);
    STATE.with(|state| *state = Some(shared));
    // Last, so the first edge finds the state in place.
    io.set_interrupt_handler(on_cs);
}

#[esp_hal::ram]
#[handler]
fn on_cs() {
    let edge = Instant::now();
    STATE.with(|state| {
        let Some(shared) = state.as_mut() else { return };
        shared.cs.clear_interrupt();
        shared.on_edge(edge);
        let us = u32::try_from((Instant::now() - edge).as_micros()).unwrap_or(u32::MAX);
        shared.handler_max_us = shared.handler_max_us.max(us);
    });
}

impl Shared {
    /// Collect the transfer CS just ended at `edge`, check it, and arm the next.
    #[esp_hal::ram]
    fn on_edge(&mut self, edge: Instant) {
        let Some((transfer, carries)) = self.armed.take() else {
            // Only after an arm that failed: try again.
            if let Some(spi) = self.spare.take() {
                self.arm(spi);
            }
            return;
        };
        let until = edge + SETTLE;
        let mut spun = false;
        while !transfer.is_done() {
            spun = true;
            if Instant::now() >= until {
                // Left armed: the SPI driver finishes it on the controller's next transfer, which
                // then fails its checks at both ends, and the edge after that re-arms.
                self.note_settle(edge, spun);
                self.early = self.early.wrapping_add(1);
                self.armed = Some((transfer, carries));
                return;
            }
        }
        self.note_settle(edge, spun);
        let (spi, (rx, tx)) = transfer.wait();
        self.transfers = self.transfers.wrapping_add(1);
        let _ = self.tx_free.push(tx);
        self.take_transfer(rx);
        self.arm(spi);
    }

    /// Check a finished transfer: count it, give a good one's header to the seq check and the
    /// uplink's wait, and queue it when it carries the next flight and there is room, which is
    /// what it is acknowledged for. A repeat of the last flight is acknowledged too. Anything not
    /// queued goes straight back to the free buffers.
    #[esp_hal::ram]
    fn take_transfer(&mut self, rx: DmaRxBuf) {
        // This transfer's own seq: `seq` advances only on a successful arm, and this transfer
        // is the one that arm made.
        let carried_in = self.seq.wrapping_sub(1);
        let queued = match verify(rx_bytes(&rx)) {
            Ok(transfer) => {
                self.good = self.good.wrapping_add(1);
                if self.peer.note(transfer.header.seq) == SeqNote::Gap {
                    self.gaps = self.gaps.wrapping_add(1);
                }
                if self.wait.on_header(&transfer.header, carried_in) {
                    self.acked = true;
                }
                if transfer.carries_records() {
                    let header = transfer.header;
                    match self.taker.take(&header, !self.received.is_full()) {
                        Take::Queue => Some((header, transfer.payload.len())),
                        Take::Repeat => {
                            self.repeats = self.repeats.wrapping_add(1);
                            self.tracker.note_records(header.seq);
                            None
                        }
                        Take::Full => {
                            self.inbox_full = self.inbox_full.wrapping_add(1);
                            None
                        }
                        Take::OutOfOrder => {
                            self.out_of_order = self.out_of_order.wrapping_add(1);
                            None
                        }
                    }
                } else {
                    None
                }
            }
            Err(VerifyError::BadMagic) => {
                self.bad_magic = self.bad_magic.wrapping_add(1);
                None
            }
            Err(VerifyError::BadCrc) => {
                self.bad_crc = self.bad_crc.wrapping_add(1);
                None
            }
            Err(VerifyError::OtherVersion(_)) => {
                self.other_version = self.other_version.wrapping_add(1);
                None
            }
            Err(VerifyError::BadLen) => {
                self.bad_len = self.bad_len.wrapping_add(1);
                None
            }
        };
        let Some((header, len)) = queued else {
            let _ = self.rx_free.push(rx);
            return;
        };
        // Committed only once queued, so a flight the inbox refused is taken on its resend.
        match self.received.push_back((rx, len)) {
            Ok(()) => {
                self.taker.commit(&header);
                self.tracker.note_records(header.seq);
            }
            Err((rx, _)) => {
                self.unread = self.unread.wrapping_add(1);
                let _ = self.rx_free.push(rx);
            }
        }
    }

    /// Keep the longest wait since `edge` for the transfer to report done, and count it in
    /// its bucket. `spun` is whether the first check found it not done.
    #[esp_hal::ram]
    fn note_settle(&mut self, edge: Instant, spun: bool) {
        let us = u32::try_from((Instant::now() - edge).as_micros()).unwrap_or(u32::MAX);
        self.settle_max_us = self.settle_max_us.max(us);
        let bucket = if spun {
            1 + SETTLE_BUCKETS_US.iter().take_while(|&&bound| us >= bound).count()
        } else {
            0
        };
        self.settle[bucket] = self.settle[bucket].wrapping_add(1);
    }

    /// Arm the prepared transfer, or an idle one, and set IRQ to match. IRQ goes low when
    /// nothing could be armed, since no transfer can then carry a record. Arming a reliable
    /// transfer starts [`deliver`]'s wait, so the P4's ack on the transfer after can never
    /// come before it.
    #[esp_hal::ram]
    fn arm(&mut self, spi: SpiDma<'static, Blocking>) {
        let prepared = match self.prepared.take() {
            Some(prepared) => prepared,
            None => match self.tx_free.pop() {
                Some(tx) => Prepared { tx, len: 0, reliable: false },
                None => {
                    self.spare = Some(spi);
                    self.irq.set_low();
                    return;
                }
            },
        };
        let Some(rx) = self.rx_free.pop() else {
            self.restore(prepared);
            self.spare = Some(spi);
            self.irq.set_low();
            return;
        };
        let Prepared { mut tx, len, reliable } = prepared;
        let carries = len > 0;
        let header = Header::new(self.seq, self.tracker.ack());
        seal(tx_bytes(&mut tx), header, len);
        match spi.transfer(TRANSFER_LEN, rx, TRANSFER_LEN, tx) {
            Ok(transfer) => {
                if reliable {
                    self.carried = Some(self.seq);
                    self.wait.sent(self.seq, now_ms());
                }
                self.seq = self.seq.wrapping_add(1);
                self.armed = Some((transfer, carries));
                self.irq.set_level(Level::from(carries || self.prepared.is_some()));
            }
            Err((_, spi, rx, tx)) => {
                self.arm_failed = self.arm_failed.wrapping_add(1);
                let _ = self.rx_free.push(rx);
                self.restore(Prepared { tx, len, reliable });
                self.spare = Some(spi);
                self.irq.set_low();
            }
        }
    }

    /// Put back a transmit buffer that was not armed.
    #[esp_hal::ram]
    fn restore(&mut self, prepared: Prepared) {
        if prepared.len > 0 {
            self.prepared = Some(prepared);
        } else {
            let _ = self.tx_free.push(prepared.tx);
        }
    }

    /// Make `prepared` the next transfer armed, raising IRQ, and arm it now if a failed arm
    /// left nothing armed.
    fn prepare(&mut self, prepared: Prepared) {
        self.prepared = Some(prepared);
        self.irq.set_high();
        if let Some(spi) = self.spare.take() {
            self.arm(spi);
        }
    }

    /// Take back a reliable transfer not yet armed, and return the seq one was armed at since
    /// it was placed.
    fn withdraw(&mut self) -> Option<u16> {
        if let Some(prepared) = self.prepared.take_if(|p| p.reliable) {
            let _ = self.tx_free.push(prepared.tx);
            let carrying = self.armed.as_ref().is_some_and(|(_, carries)| *carries);
            self.irq.set_level(Level::from(carrying));
        }
        self.carried.take()
    }
}

/// Pack one record of `kind` into a free transmit buffer, outside the lock. `None` when no
/// buffer is free or the body does not fit a transfer.
fn pack(kind: UpType, body: &[u8], reliable: bool) -> Option<Prepared> {
    let mut tx = STATE.with(|state| state.as_mut().and_then(|s| s.tx_free.pop()))?;
    let mut packer = Packer::new(tx_bytes(&mut tx));
    if !packer.push(kind.as_u8(), body) {
        release(Some(Prepared { tx, len: 0, reliable }));
        return None;
    }
    let len = packer.len();
    Some(Prepared { tx, len, reliable })
}

/// Queue a heartbeat for the P4, replacing one it has not clocked yet.
///
/// Packed outside the lock into a free buffer, then swapped in under one lock, so a CS edge
/// sees either the old prepared transfer or the new one and IRQ never drops between them. A
/// reliable transfer prepared by [`deliver`] is never replaced: the heartbeat is dropped and
/// counted instead.
pub fn send(kind: UpType, body: &[u8]) {
    let Some(prepared) = pack(kind, body, false) else { return };
    let replaced = STATE.with(|state| {
        let shared = state.as_mut()?;
        if shared.prepared.as_ref().is_some_and(|p| p.reliable) {
            let _ = shared.tx_free.push(prepared.tx);
            return Some(true);
        }
        let old = shared.prepared.take();
        let replaced = old.is_some();
        if let Some(old) = old {
            let _ = shared.tx_free.push(old.tx);
        }
        shared.prepare(prepared);
        Some(replaced)
    });
    if replaced == Some(true) {
        MAIN.with(|main| main.replaced = main.replaced.wrapping_add(1));
    }
}

/// How [`stage`] ended.
enum Staged {
    /// The records were armed.
    Armed,
    /// The P4 acknowledged the previous try while this one waited.
    Acked,
    /// Nothing armed them within [`ARM_WAIT_MS`], or they do not fit a transfer.
    Failed,
}

/// Send one record of `kind` to the P4 and wait for the P4 to acknowledge it, servicing the
/// bus meanwhile. Returns whether it was acknowledged.
///
/// Blocks the main loop: up to [`ARM_WAIT_MS`] for the records to be armed, behind any
/// heartbeat already prepared, then [`ACK_TIMEOUT_MS`] for the ack. Each timeout sends the same
/// records again in a new transfer, up to [`SEND_TRIES`](wartui_proto::bus::SEND_TRIES) in
/// all. On failure, records not yet armed are withdrawn; ones already armed go out when the P4
/// next clocks, unwaited.
pub fn deliver(kind: UpType, body: &[u8]) -> bool {
    with_shared(|shared| {
        shared.wait.cancel();
        shared.acked = false;
    });
    let delivered = loop {
        match stage(kind, body) {
            Staged::Armed => {}
            Staged::Acked => break true,
            Staged::Failed => break false,
        }
        let poll = loop {
            service();
            let poll =
                with_shared(
                    |shared| {
                        if shared.acked { None } else { Some(shared.wait.poll(now_ms())) }
                    },
                )
                .flatten();
            match poll {
                Some(AckPoll::Idle | AckPoll::Waiting) => pause(),
                other => break other,
            }
        };
        match poll {
            None => break true,
            Some(AckPoll::Resend) => MAIN.with(|main| main.resent = main.resent.wrapping_add(1)),
            _ => break false,
        }
    };
    withdraw();
    with_shared(|shared| {
        shared.wait.cancel();
        shared.acked = false;
    });
    if !delivered {
        MAIN.with(|main| main.undelivered = main.undelivered.wrapping_add(1));
    }
    delivered
}

/// Run `f` on the shared state under its lock, or return `None` before [`start`].
fn with_shared<T>(f: impl FnOnce(&mut Shared) -> T) -> Option<T> {
    STATE.with(|state| state.as_mut().map(f))
}

/// Prepare the records once any heartbeat ahead of them is armed, then wait for their own arm.
fn stage(kind: UpType, body: &[u8]) -> Staged {
    if body.len() > RECORD_MAX {
        return Staged::Failed;
    }
    let until = now_ms() + ARM_WAIT_MS;
    // Packed and not yet in the prepared slot, since a heartbeat holds it.
    let mut held: Option<Prepared> = None;
    let mut placed = false;
    loop {
        service();
        if with_shared(|shared| shared.acked) == Some(true) {
            release(held);
            withdraw();
            return Staged::Acked;
        }
        if !placed {
            if held.is_none() {
                held = pack(kind, body, true);
            }
            if let Some(next) = held.take() {
                held = STATE.with(|state| match state.as_mut() {
                    Some(shared) if shared.prepared.is_none() => {
                        shared.carried = None;
                        shared.prepare(next);
                        None
                    }
                    _ => Some(next),
                });
                placed = held.is_none();
            }
        }
        if placed && with_shared(|shared| shared.carried.take()).flatten().is_some() {
            return Staged::Armed;
        }
        if now_ms() >= until {
            release(held);
            return withdraw().map_or(Staged::Failed, |_| Staged::Armed);
        }
        pause();
    }
}

/// Take back a reliable transfer not yet armed, and return the seq one was armed at since
/// [`stage`] placed it.
fn withdraw() -> Option<u16> {
    STATE.with(|state| state.as_mut().and_then(Shared::withdraw))
}

/// Return a packed transfer's buffer that was never prepared.
fn release(held: Option<Prepared>) {
    let Some(held) = held else { return };
    STATE.with(|state| {
        if let Some(shared) = state.as_mut() {
            let _ = shared.tx_free.push(held.tx);
        }
    });
}

/// Copy the oldest payload the P4 sent into `out`, and return its length, if any.
pub fn take(out: &mut [u8; PAYLOAD_MAX]) -> Option<usize> {
    let (rx, len) = with_shared(|shared| shared.received.pop_front()).flatten()?;
    // Copied outside the lock, which holds off every interrupt.
    out[..len].copy_from_slice(&rx_bytes(&rx)[HEADER_LEN..HEADER_LEN + len]);
    with_shared(|shared| {
        let _ = shared.rx_free.push(rx);
    });
    Some(len)
}

/// Print the counters every [`REPORT_EVERY`]. The handler checks what arrives, and [`take`]
/// reads it.
pub fn service() {
    let now = Instant::now();
    let due = MAIN.with(|main| {
        let due = main.last_report.is_none_or(|last| now - last >= REPORT_EVERY);
        if due {
            main.last_report = Some(now);
        }
        due
    });
    if due {
        report();
    }
}

/// What the handler counted, for one console line.
struct Counts {
    transfers: u32,
    good: u32,
    magic: u32,
    crc: u32,
    version: u32,
    len: u32,
    gaps: u32,
    unread: u32,
    inbox_full: u32,
    out_of_order: u32,
    repeats: u32,
    early: u32,
    arm_failed: u32,
    handler_max_us: u32,
    settle_max_us: u32,
    settle: [u32; SETTLE_BUCKETS_US.len() + 2],
}

/// Print one line of counters.
fn report() {
    let counts = with_shared(|s| Counts {
        transfers: s.transfers,
        good: s.good,
        magic: s.bad_magic,
        crc: s.bad_crc,
        version: s.other_version,
        len: s.bad_len,
        gaps: s.gaps,
        unread: s.unread,
        inbox_full: s.inbox_full,
        out_of_order: s.out_of_order,
        repeats: s.repeats,
        early: s.early,
        arm_failed: s.arm_failed,
        handler_max_us: s.handler_max_us,
        settle_max_us: s.settle_max_us,
        settle: s.settle,
    });
    let Some(c) = counts else { return };
    let (replaced, resent, undelivered) = MAIN.with(|m| (m.replaced, m.resent, m.undelivered));
    note!(
        "bus: {} transfers, {} good, {} magic, {} crc, {} version, {} len, {} gaps, {} unread, \
         {} inbox full, {} out of order, {} repeats, {} early, {} arm failed, {} replaced, \
         {} resent, {} undelivered, {} us handler max, {} us settle max, \
         settle 0/5/20/45/+: {}/{}/{}/{}/{}",
        c.transfers,
        c.good,
        c.magic,
        c.crc,
        c.version,
        c.len,
        c.gaps,
        c.unread,
        c.inbox_full,
        c.out_of_order,
        c.repeats,
        c.early,
        c.arm_failed,
        replaced,
        resent,
        undelivered,
        c.handler_max_us,
        c.settle_max_us,
        c.settle[0],
        c.settle[1],
        c.settle[2],
        c.settle[3],
        c.settle[4]
    );
}

/// Milliseconds since boot, the clock [`AckWait`] reads.
#[esp_hal::ram]
fn now_ms() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

/// Wait one [`POLL_MS`], letting the radio's tasks run.
fn pause() {
    CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
}

#[esp_hal::ram]
fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

#[esp_hal::ram]
fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
