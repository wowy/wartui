//! The node's end of the node bus: an SPI slave of the P4 bridge (`docs/node-bus.md`).
//!
//! `wartui_proto::bus` has the framing and the handshake. Today the bus carries a copy of each
//! heartbeat as a node-frame record, and ESP-NOW carries everything as before. What the P4
//! sends is checked and counted, and its records are not read yet.
//!
//! **The re-arm lives in an interrupt on CS's rising edge.** esp-hal's slave does one blocking
//! transfer at a time, armed before the master clocks, so each transfer must be followed by
//! the next arm before the master's next one. The main loop cannot do it: it blocks for whole
//! dwells and Bluetooth scans, and esp-rtos offers no thread to hand it to. So CS is read twice,
//! by the slave and by a GPIO input whose rising edge ends a transfer. The handler collects
//! the finished transfer, then arms the next: the one the main loop prepared, or an idle one it
//! seals itself. It stamps seq, ack and flags at arm time, so seq counts arms in order. It
//! does nothing else. Checking what arrived and packing what goes out happen in [`service`]
//! and [`send`], from the main loop.
//!
//! Buffers move between the two through [`STATE`], under a lock held only to move them. The
//! handler keeps up to [`RECEIVED_MAX`] finished transfers for the main loop. Past that it
//! drops the oldest unread one and counts it, since a stalled main loop must not stop the
//! re-arm.
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
use esp_sync::NonReentrantMutex;
use static_cell::ConstStaticCell;
use wartui_proto::bus::{
    Header, Packer, RecordType, SeqCheck, SeqNote, TRANSFER_LEN, VerifyError, seal, verify,
};
use wartui_proto::heapless::{Deque, Vec};

/// Receive buffers: one armed, up to [`RECEIVED_MAX`] waiting, and one being checked.
const RX_BUFS: usize = 4;
/// Transmit buffers: one armed, one prepared, and one being packed.
const TX_BUFS: usize = 3;
/// Finished transfers the handler holds for the main loop.
const RECEIVED_MAX: usize = RX_BUFS - 2;
/// How long the handler waits after CS rises for the DMA to report the transfer done.
const SETTLE: Duration = Duration::from_micros(50);
/// How often [`service`] prints the counters.
const REPORT_EVERY: Duration = Duration::from_millis(10_000);

type Armed = SpiDmaTransfer<'static, Blocking, (DmaRxBuf, DmaTxBuf)>;

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
    /// A packed transfer and its payload length, armed at the next edge.
    prepared: Option<(DmaTxBuf, usize)>,
    /// This node's seq for the next transfer armed.
    seq: u16,
    /// The last good seq from the P4, as [`service`] last saw it.
    ack: u16,
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
}

static STATE: NonReentrantMutex<Option<Shared>> = NonReentrantMutex::new(None);

/// What only the main loop touches.
struct Main {
    seq: SeqCheck,
    good: u32,
    bad_magic: u32,
    bad_crc: u32,
    other_version: u32,
    bad_len: u32,
    gaps: u32,
    /// Heartbeat copies replaced by a newer one before the P4 clocked them.
    replaced: u32,
    last_report: Option<Instant>,
}

static MAIN: NonReentrantMutex<Main> = NonReentrantMutex::new(Main {
    seq: SeqCheck::new(),
    good: 0,
    bad_magic: 0,
    bad_crc: 0,
    other_version: 0,
    bad_len: 0,
    gaps: 0,
    replaced: 0,
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
        seq: 0,
        ack: 0,
        transfers: 0,
        unread: 0,
        early: 0,
        arm_failed: 0,
        settle_max_us: 0,
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
        while !transfer.is_done() {
            if Instant::now() >= until {
                // Left armed: the slave finishes it on the master's next transfer, which
                // then fails its checks at both ends, and the edge after that re-arms.
                self.note_settle(edge);
                self.early = self.early.wrapping_add(1);
                self.armed = Some((transfer, carries));
                return;
            }
        }
        self.note_settle(edge);
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

    /// Keep the longest wait since `edge` for the transfer to report done.
    fn note_settle(&mut self, edge: Instant) {
        let us = u32::try_from((Instant::now() - edge).as_micros()).unwrap_or(u32::MAX);
        self.settle_max_us = self.settle_max_us.max(us);
    }

    /// Arm the prepared transfer, or an idle one, and set IRQ to match. IRQ goes low when
    /// nothing could be armed, since no transfer can then carry a record.
    fn arm(&mut self, spi: SpiDma<'static, Blocking>) {
        let (mut tx, len, carries) = match self.prepared.take() {
            Some((tx, len)) => (tx, len, true),
            None => match self.tx_free.pop() {
                Some(tx) => (tx, 0, false),
                None => {
                    self.spare = Some(spi);
                    self.irq.set_low();
                    return;
                }
            },
        };
        let Some(rx) = self.rx_free.pop() else {
            self.restore(tx, len, carries);
            self.spare = Some(spi);
            self.irq.set_low();
            return;
        };
        // Nothing queues behind the prepared transfer, so `MORE` stays clear.
        let header = Header { flags: 0, seq: self.seq, ack: self.ack };
        seal(tx_bytes(&mut tx), header, len);
        match spi.transfer(TRANSFER_LEN, rx, TRANSFER_LEN, tx) {
            Ok(transfer) => {
                self.seq = self.seq.wrapping_add(1);
                self.armed = Some((transfer, carries));
                self.irq.set_level(Level::from(carries || self.prepared.is_some()));
            }
            Err((_, spi, rx, tx)) => {
                self.arm_failed = self.arm_failed.wrapping_add(1);
                let _ = self.rx_free.push(rx);
                self.restore(tx, len, carries);
                self.spare = Some(spi);
                self.irq.set_low();
            }
        }
    }

    /// Put back a transmit buffer that was not armed.
    fn restore(&mut self, tx: DmaTxBuf, len: usize, carries: bool) {
        if carries {
            self.prepared = Some((tx, len));
        } else {
            let _ = self.tx_free.push(tx);
        }
    }
}

/// Queue `frame` for the P4 as a node-frame record, replacing a copy it has not clocked yet.
///
/// Packed outside the lock into a free buffer, then swapped in under one lock, so a CS edge
/// sees either the old prepared transfer or the new one and IRQ never drops between them.
pub fn send(frame: &[u8]) {
    let Some(mut tx) = STATE.with(|state| state.as_mut().and_then(|s| s.tx_free.pop())) else {
        return;
    };
    let mut packer = Packer::new(tx_bytes(&mut tx));
    // Every node frame fits an empty transfer.
    packer.push(RecordType::NodeFrame, frame);
    let len = packer.len();
    let replaced = STATE.with(|state| {
        let shared = state.as_mut()?;
        let old = shared.prepared.replace((tx, len));
        let replaced = old.is_some();
        if let Some((old, _)) = old {
            let _ = shared.tx_free.push(old);
        }
        shared.irq.set_high();
        // A failed arm left nothing armed: arm this one now.
        if let Some(spi) = shared.spare.take() {
            shared.arm(spi);
        }
        Some(replaced)
    });
    if replaced == Some(true) {
        MAIN.with(|main| main.replaced = main.replaced.wrapping_add(1));
    }
}

/// Check every transfer the P4 has sent since the last call, and print the counters every
/// [`REPORT_EVERY`].
pub fn service() {
    while let Some(rx) = STATE.with(|state| state.as_mut().and_then(|s| s.received.pop_front())) {
        // Checked outside both locks, which hold off every interrupt.
        let checked = verify(rx_bytes(&rx)).map(|transfer| transfer.header.seq);
        let ack = MAIN.with(|main| main.count(checked));
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
    /// Count one received transfer by what [`verify`] made of it, its seq when good, and
    /// return the ack to send back. The P4 sends no records yet.
    fn count(&mut self, checked: Result<u16, VerifyError>) -> u16 {
        match checked {
            Ok(seq) => {
                self.good = self.good.wrapping_add(1);
                if self.seq.note(seq) == SeqNote::Gap {
                    self.gaps = self.gaps.wrapping_add(1);
                }
            }
            Err(VerifyError::BadMagic) => self.bad_magic = self.bad_magic.wrapping_add(1),
            Err(VerifyError::BadCrc) => self.bad_crc = self.bad_crc.wrapping_add(1),
            Err(VerifyError::OtherVersion(_)) => {
                self.other_version = self.other_version.wrapping_add(1);
            }
            Err(VerifyError::BadLen) => self.bad_len = self.bad_len.wrapping_add(1),
        }
        self.seq.ack()
    }
}

/// Print one line of counters.
fn report() {
    let isr = STATE.with(|state| {
        state.as_ref().map(|s| (s.transfers, s.unread, s.early, s.arm_failed, s.settle_max_us))
    });
    let Some((transfers, unread, early, arm_failed, settle_max_us)) = isr else { return };
    let (good, magic, crc, version, len, gaps, replaced) = MAIN
        .with(|m| (m.good, m.bad_magic, m.bad_crc, m.other_version, m.bad_len, m.gaps, m.replaced));
    note!(
        "bus: {} transfers, {} good, {} magic, {} crc, {} version, {} len, {} gaps, {} unread, \
         {} early, {} arm failed, {} replaced, {} us settle max",
        transfers,
        good,
        magic,
        crc,
        version,
        len,
        gaps,
        unread,
        early,
        arm_failed,
        replaced,
        settle_max_us
    );
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
