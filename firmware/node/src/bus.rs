//! The node's end of the node bus: an SPI slave of the P4 bridge (`docs/node-bus.md`), and the
//! node's only link to the host.
//!
//! `wartui_proto::bus` has the framing, the handshake and the messages. A heartbeat goes up
//! fire-and-forget ([`send`]): a newer one replaces one the P4 has not clocked. Sightings and
//! update statuses go up stop-and-wait ([`deliver`]), and the main loop blocks until the P4
//! acknowledges them. Records from the P4 wait in an inbox until the main loop takes them
//! ([`take`]).
//!
//! **The re-arm lives in an interrupt on CS's rising edge.** esp-hal's slave does one blocking
//! transfer at a time, armed before the master clocks, so each transfer must be followed by
//! the next arm before the master's next one. The main loop cannot do it: it blocks for whole
//! dwells and Bluetooth scans, and esp-rtos offers no thread to hand it to. So CS is read twice,
//! by the slave and by a GPIO input whose rising edge ends a transfer. The handler collects
//! the finished transfer, then arms the next: the one the main loop prepared, or an idle one it
//! seals itself. It stamps seq, ack and flags at arm time, so seq counts arms in order. It
//! does nothing else. Checking what arrived and packing what goes out happen in [`service`],
//! [`send`] and [`deliver`], from the main loop.
//!
//! **The node's ack lags by up to one main-loop pass.** The handler stamps the ack [`service`]
//! last set, and the main loop calls [`service`] between dwells and scans. So a P4 transfer's
//! records are acknowledged on a transfer armed after the main loop next looks: about one
//! 125 ms dwell later, or one Bluetooth scan. The P4's
//! [`ACK_TIMEOUT_MS`](wartui_proto::bus::ACK_TIMEOUT_MS) allows for it.
//!
//! **Records are acknowledged only once they are in the inbox.** A transfer whose records find
//! the inbox full is not acknowledged, so the P4 sends them again after its timeout rather than
//! losing them.
//!
//! Buffers move between the handler and the main loop through [`STATE`], under a lock held
//! only to move them. The handler keeps up to [`RECEIVED_MAX`] finished transfers for the main
//! loop. Past that it drops the oldest unread one and counts it, since a stalled main loop
//! must not stop the re-arm.
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
    ACK_TIMEOUT_MS, AckPoll, AckTracker, AckWait, Header, PAYLOAD_MAX, Packer, RECORD_MAX,
    SeqCheck, SeqNote, TRANSFER_LEN, Transfer, UpType, VerifyError, seal, verify,
};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::heapless::{Deque, Vec};

use crate::POLL_MS;

/// Receive buffers: one armed, up to [`RECEIVED_MAX`] waiting, and one being checked.
const RX_BUFS: usize = 4;
/// Transmit buffers: one armed, one prepared, and one being packed.
const TX_BUFS: usize = 3;
/// Finished transfers the handler holds for the main loop.
const RECEIVED_MAX: usize = RX_BUFS - 2;
/// Payloads from the P4 the inbox holds for the main loop.
const INBOX: usize = 2;
/// The inbox's bytes: [`INBOX`] full payloads, each behind the ring's two-byte length.
const INBOX_BYTES: usize = INBOX * (2 + PAYLOAD_MAX);
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
    /// The transfer the master clocks next, and whether it carries a record.
    armed: Option<(Armed, bool)>,
    /// The slave when nothing is armed: only after an arm that failed.
    spare: Option<SpiDma<'static, Blocking>>,
    received: Deque<DmaRxBuf, RECEIVED_MAX>,
    rx_free: Vec<DmaRxBuf, RX_BUFS>,
    tx_free: Vec<DmaTxBuf, TX_BUFS>,
    /// Armed at the next edge.
    prepared: Option<Prepared>,
    /// The seq a reliable transfer was armed at, until [`deliver`] takes it.
    carried: Option<u16>,
    /// This node's seq for the next transfer armed.
    seq: u16,
    /// The ack to stamp, as [`service`] last set it.
    ack: Option<u16>,
    /// Transfers completed.
    transfers: u32,
    /// Finished transfers dropped before [`service`] read them.
    unread: u32,
    /// CS edges with the transfer not done: the master ended one early.
    early: u32,
    /// Arms the slave refused.
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
    seq: SeqCheck,
    /// The P4's last record-carrying transfer whose records reached the inbox.
    tracker: AckTracker,
    /// [`deliver`]'s wait for its ack.
    wait: AckWait,
    /// The P4 acknowledged the transfer [`Main::wait`] waits on.
    acked: bool,
    inbox: FrameRing<INBOX_BYTES>,
    good: u32,
    bad_magic: u32,
    bad_crc: u32,
    other_version: u32,
    bad_len: u32,
    gaps: u32,
    /// Heartbeats replaced by a newer one before the P4 clocked them, or dropped behind a
    /// reliable transfer.
    replaced: u32,
    /// P4 transfers left unacknowledged because the inbox was full.
    inbox_full: u32,
    /// Records [`deliver`] sent again after [`ACK_TIMEOUT_MS`].
    resent: u32,
    /// Records [`deliver`] gave up on.
    undelivered: u32,
    last_report: Option<Instant>,
}

static MAIN: NonReentrantMutex<Main> = NonReentrantMutex::new(Main {
    seq: SeqCheck::new(),
    tracker: AckTracker::new(),
    wait: AckWait::new(),
    acked: false,
    inbox: FrameRing::new(),
    good: 0,
    bad_magic: 0,
    bad_crc: 0,
    other_version: 0,
    bad_len: 0,
    gaps: 0,
    replaced: 0,
    inbox_full: 0,
    resent: 0,
    undelivered: 0,
    last_report: None,
});

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
/// `spi` takes `cs.peripheral_input()` as its CS, so the slave and the edge read one pin.
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
        ack: None,
        transfers: 0,
        unread: 0,
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

#[handler]
fn on_cs() {
    STATE.with(|state| {
        let Some(shared) = state.as_mut() else { return };
        shared.cs.clear_interrupt();
        shared.on_edge();
    });
}

impl Shared {
    /// Collect the transfer CS just ended, and arm the next.
    fn on_edge(&mut self) {
        let Some((transfer, carries)) = self.armed.take() else {
            // Only after an arm that failed: try again.
            if let Some(spi) = self.spare.take() {
                self.arm(spi);
            }
            return;
        };
        let edge = Instant::now();
        let until = edge + SETTLE;
        let mut spun = false;
        while !transfer.is_done() {
            spun = true;
            if Instant::now() >= until {
                // Left armed: the slave finishes it on the master's next transfer, which
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
        if self.received.is_full()
            && let Some(old) = self.received.pop_front()
        {
            self.unread = self.unread.wrapping_add(1);
            let _ = self.rx_free.push(old);
        }
        let _ = self.received.push_back(rx);
        let _ = self.tx_free.push(tx);
        self.arm(spi);
    }

    /// Keep the longest wait since `edge` for the transfer to report done, and count it in
    /// its bucket. `spun` is whether the first check found it not done.
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
    /// nothing could be armed, since no transfer can then carry a record.
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
        // Nothing queues behind the prepared transfer, so `MORE` stays clear.
        let header = Header::new(self.seq, self.ack, false);
        seal(tx_bytes(&mut tx), header, len);
        match spi.transfer(TRANSFER_LEN, rx, TRANSFER_LEN, tx) {
            Ok(transfer) => {
                if reliable {
                    self.carried = Some(self.seq);
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
    /// The records were armed in transfer `seq`.
    Armed(u16),
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
    MAIN.with(|main| {
        main.wait.cancel();
        main.acked = false;
    });
    let delivered = loop {
        let seq = match stage(kind, body) {
            Staged::Armed(seq) => seq,
            Staged::Acked => break true,
            Staged::Failed => break false,
        };
        MAIN.with(|main| main.wait.sent(seq, now_ms()));
        let poll = loop {
            service();
            let poll =
                MAIN.with(|main| if main.acked { None } else { Some(main.wait.poll(now_ms())) });
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
    MAIN.with(|main| {
        main.wait.cancel();
        main.acked = false;
        if !delivered {
            main.undelivered = main.undelivered.wrapping_add(1);
        }
    });
    delivered
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
        if MAIN.with(|main| main.acked) {
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
        if placed && let Some(seq) = STATE.with(|state| state.as_mut()?.carried.take()) {
            return Staged::Armed(seq);
        }
        if now_ms() >= until {
            release(held);
            return withdraw().map_or(Staged::Failed, Staged::Armed);
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
    MAIN.with(|main| {
        // `out` holds any payload, so one read takes it whole.
        let (len, whole) = main.inbox.read(out);
        whole.then_some(len)
    })
}

/// Check every transfer the P4 has sent since the last call, and print the counters every
/// [`REPORT_EVERY`].
pub fn service() {
    while let Some(rx) = STATE.with(|state| state.as_mut().and_then(|s| s.received.pop_front())) {
        // Checked outside both locks, which hold off every interrupt.
        let checked = verify(rx_bytes(&rx));
        let ack = MAIN.with(|main| main.take_transfer(checked));
        STATE.with(|state| {
            if let Some(shared) = state.as_mut() {
                shared.ack = ack;
                let _ = shared.rx_free.push(rx);
            }
        });
    }
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

impl Main {
    /// Count one received transfer by what [`verify`] made of it, give a good one's header to
    /// the wait and its records to the inbox, and return the ack to send back.
    fn take_transfer(&mut self, checked: Result<Transfer<'_>, VerifyError>) -> Option<u16> {
        match checked {
            Ok(transfer) => {
                self.good = self.good.wrapping_add(1);
                if self.seq.note(transfer.header.seq) == SeqNote::Gap {
                    self.gaps = self.gaps.wrapping_add(1);
                }
                if self.wait.on_header(&transfer.header) {
                    self.acked = true;
                }
                if transfer.carries_records() {
                    // Checked first, since a full ring evicts rather than refuses, and an
                    // evicted payload would be acknowledged and lost.
                    let room = INBOX_BYTES - self.inbox.len_bytes();
                    if room >= 2 + transfer.payload.len() && self.inbox.push(transfer.payload) {
                        self.tracker.note_records(transfer.header.seq);
                    } else {
                        self.inbox_full = self.inbox_full.wrapping_add(1);
                    }
                }
            }
            Err(VerifyError::BadMagic) => self.bad_magic = self.bad_magic.wrapping_add(1),
            Err(VerifyError::BadCrc) => self.bad_crc = self.bad_crc.wrapping_add(1),
            Err(VerifyError::OtherVersion(_)) => {
                self.other_version = self.other_version.wrapping_add(1);
            }
            Err(VerifyError::BadLen) => self.bad_len = self.bad_len.wrapping_add(1),
        }
        self.tracker.ack()
    }
}

/// Print one line of counters.
fn report() {
    let isr = STATE.with(|state| {
        state
            .as_ref()
            .map(|s| (s.transfers, s.unread, s.early, s.arm_failed, s.settle_max_us, s.settle))
    });
    let Some((transfers, unread, early, arm_failed, settle_max_us, settle)) = isr else { return };
    let (good, magic, crc, version, len, gaps) =
        MAIN.with(|m| (m.good, m.bad_magic, m.bad_crc, m.other_version, m.bad_len, m.gaps));
    let (replaced, inbox_full, resent, undelivered) =
        MAIN.with(|m| (m.replaced, m.inbox_full, m.resent, m.undelivered));
    note!(
        "bus: {} transfers, {} good, {} magic, {} crc, {} version, {} len, {} gaps, {} unread, \
         {} inbox full, {} early, {} arm failed, {} replaced, {} resent, {} undelivered, \
         {} us settle max, settle 0/5/20/45/+: {}/{}/{}/{}/{}",
        transfers,
        good,
        magic,
        crc,
        version,
        len,
        gaps,
        unread,
        inbox_full,
        early,
        arm_failed,
        replaced,
        resent,
        undelivered,
        settle_max_us,
        settle[0],
        settle[1],
        settle[2],
        settle[3],
        settle[4]
    );
}

/// Milliseconds since boot, the clock [`AckWait`] reads.
fn now_ms() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

/// Wait one [`POLL_MS`], letting the radio's tasks run.
fn pause() {
    CurrentThreadHandle::get().delay(Duration::from_millis(POLL_MS));
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
