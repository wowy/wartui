//! The C6's end of the UART link to the P4: what each packet carries up, and what the C6
//! does with what comes down.
//!
//! `wartui_proto::radio_link` has the framing. The link is two futures, and nothing ticks: the
//! reader wakes when a DMA read ends, and the writer when [`Shared`]'s `wake` says there may be
//! something to send. The writer loops until it has nothing left before it waits, so a record
//! left held or queued never waits for a new signal. They share [`Link`] through a `RefCell`, with nothing borrowed across an `.await`.
//!
//! **A read ends at `RX_BUF_LEN` bytes or when the line goes idle.** The next read is armed
//! before this one's bytes are looked at, which is why there are two buffers: DMA keeps
//! landing while the C6 works, and a flash erase holds the CPU but not the DMA. Only the moment
//! between two reads leans on the 128-byte FIFO, with RTS behind it.
//!
//! **The writer polls the DMA on a timer and never calls `wait_for_done` or `wait`.** Both flush
//! the UART, and the flush ends in a spin that does not yield until the transmitter is idle, so a
//! P4 holding CTS as the FIFO empties would stop this task and the reader joined to it. Nothing
//! needs the FIFO drained between packets: the link never turns the line around, and the next
//! write resets only the DMA channel. `firmware/p4-bridge/src/link.rs` has the rest.
//!
//! Records go up through [`Shared`]'s byte ring: the C6's lines and its status. A full ring
//! evicts its oldest record, so a stalled link keeps the newest.
//!
//! The C6 sends a hello every [`HELLO_EVERY_MS`] until it hears the P4's. The P4 tells a repeat
//! from a new life by the packet seq, which starts at 0 on every boot
//! (`wartui_proto::split`, `Pair::on_hello`). The hello carries the booted image's digest,
//! which is how the P4 decides to send its bundled image.
//!
//! **The P4's hello confirms an image on trial.** Hearing the P4 is the test, as hearing the
//! host is a node's; `firmware/common/src/ota.rs` has the trial.
//!
//! **An update arrives as [`Kind::Update`] packets** (`wartui_proto::radio_link::update`), which
//! drive [`wartui_proto::ota::Receiver`] over [`wartui_firmware::ota`], as a node's update
//! frames do. Each status goes up in the next packet, ahead of any records. A sector's flash
//! work holds the task, so what the P4 sends meanwhile waits in DMA and the FIFO, then behind
//! RTS; the P4 sends no records while it updates. `Install` on a verified session switches
//! slots and reboots, and the P4 hears the new image's hello.
//!
//! **The host stream over Bluetooth is not carried yet.** The C6 sends its Bluetooth counters
//! every [`BLE_STATS_EVERY`], zeros while no stack runs, and ignores `Down::HostBytes`. The
//! `sink-discard` feature instead credits each one at once and claims a subscriber once the
//! P4's hello arrives, so the P4 streams its host queue here; that measures the link's ceiling.

use core::cell::RefCell;
use core::fmt::{self, Write};

use embassy_futures::join::join3;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Timer};
use esp_hal::Async;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::time::Instant;
use esp_hal::uart::uhci::{UhciRx, UhciTx};
use wartui_firmware::ota::{Note, Ota};
use wartui_firmware::phase::mark;
use wartui_firmware::reboot;
use wartui_proto::bus::{UpdateState, UpdateStatus};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{LogStr, LoopPhase, QueueId, QueueStat, ShortStr};
use wartui_proto::mac::Mac;
use wartui_proto::ota::Receiver;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::radio_link::update::{Op, Received, receive};
use wartui_proto::radio_link::{
    BAUD_RATE, BODY_MAX, BleState, BleStats, Chip, Down, HEADER_LEN, HELLO_EVERY_MS, Held, Hello,
    Kind, LINK_REVISION, PAYLOAD_MAX, Packer, Packet, PacketReader, RECORD_MAX, RadioStatus,
    SeqCheck, SeqNote, Up, VerifyError, decode_down, encode_packet, encode_up, records, seal,
};
use wartui_proto::reset::ResetCause;

use crate::config::UP_QUEUE_BYTES;

/// How often a write looks for its end once its wire time has passed;
/// `firmware/p4-bridge/src/link.rs`, `WRITE_POLL`, has why.
const WRITE_POLL: Duration = Duration::from_micros(100);

/// How often a write looks for its end once it has waited twice its wire time;
/// `firmware/p4-bridge/src/link.rs`, `WRITE_POLL_HELD`, has why.
const WRITE_POLL_HELD: Duration = Duration::from_millis(1);

/// How often a C6 that has not heard the P4 looks again, with nothing else to wake it.
const HELLO_EVERY: Duration = Duration::from_millis(HELLO_EVERY_MS);

/// How often the C6 sends the P4 its Bluetooth counters.
const BLE_STATS_EVERY: Duration = Duration::from_secs(2);

/// The ring of encoded [`Up`] records waiting for a packet.
pub type UpQueue = RefCell<FrameRing<UP_QUEUE_BYTES>>;

/// What the link's futures share. They are joined in one task, so nothing here is borrowed
/// across an `.await`.
pub struct Shared {
    up: &'static UpQueue,
    /// Raised by whatever may have given the writer something to send, so a waiting writer
    /// looks again.
    wake: Signal<NoopRawMutex, ()>,
}

impl Shared {
    /// Nothing queued yet. The ring is static, so it is never on the stack.
    pub fn new(up: &'static UpQueue) -> Self {
        Self { up, wake: Signal::new() }
    }

    /// Queue `msg` for the P4, evicting the ring's oldest records if it is full.
    ///
    /// Out of line, with the encode buffer, to keep both out of the callers' frames.
    #[inline(never)]
    pub fn push(&self, msg: &Up<'_>) {
        let mut record = [0; RECORD_MAX];
        // Only a record longer than any `Up` fails, and the ring counts its own drops.
        if let Ok(len) = encode_up(msg, &mut record) {
            self.up.borrow_mut().push(&record[..len]);
            self.wake.signal(());
        }
    }

    /// Send the P4 a `Log` composed here. Text past a line's length is cut.
    #[inline(never)]
    pub fn log(&self, args: fmt::Arguments<'_>) {
        self.push(&Up::Log { message: text(args) });
    }

    /// Send the P4 an `Error` composed here. Text past a line's length is cut.
    #[inline(never)]
    pub fn error(&self, args: fmt::Arguments<'_>) {
        self.push(&Up::Error { message: text(args) });
    }

    /// Pass on what an update step had to say: a failure as an `Error`, anything else as a
    /// `Log`.
    pub fn note(&self, note: &Note) {
        if matches!(note, Note::Failed(..)) {
            self.error(format_args!("{note}"));
        } else {
            self.log(format_args!("{note}"));
        }
    }
}

/// The update side: the flash, the receiver an image goes through, and where this image
/// stands.
pub struct Updates {
    pub ota: Ota,
    /// Static, since it holds a sector of buffer.
    pub receiver: &'static mut Receiver,
    /// The booted image is on trial until the P4's hello confirms it.
    pub trial: bool,
    /// The booted image's digest, for the hello, or zeros when it could not be read.
    pub image: [u8; DIGEST_LEN],
    /// The status to send the P4 in the next packet.
    pub status: Option<UpdateStatus>,
}

/// UART1 through UHCI, and its buffers. All static: a packet is too large for a future's frame.
pub struct Bus<'d> {
    pub rx: UhciRx<'d, Async>,
    pub tx: UhciTx<'d, Async>,
    pub rx_bufs: [DmaRxBuf; 2],
    pub tx_buf: DmaTxBuf,
    pub reader: &'static mut PacketReader,
    /// Where a packet's body is built, before it is encoded into `tx_buf`.
    pub body: &'static mut [u8; BODY_MAX],
}

/// The link's state.
pub struct Link {
    boot: Instant,
    mac: Mac,
    /// Why this life started, and where the last one stopped, for the hello.
    cause: ResetCause,
    phase: LoopPhase,
    /// This end's seq, +1 per packet sent. From 0 at every boot, which is how the P4 tells a
    /// restart from a repeated hello.
    seq: u32,
    p4_seq: SeqCheck,
    /// The P4's hello has arrived. Until then this end sends only its hello.
    heard_p4: bool,
    /// When this end last sent its hello.
    hello_sent: Option<Instant>,
    /// P4 packets that failed their CRC.
    crc_fail: u32,
    /// P4 packets without the magic.
    bad_magic: u32,
    /// Breaks in the P4's seq.
    seq_gap: u32,
    /// Line errors on this end's UART.
    uart_errors: u32,
    /// P4 records that did not decode, and how many of them have been reported.
    undecoded: u32,
    undecoded_said: u32,
    /// `Down::HostBytes` bytes taken since boot, wrapping: what `Up::BleCredit` reports.
    drained: u32,
    /// The Bluetooth counters `Up::BleStats` reports.
    ble_stats: BleStats,
    /// A record from the ring that did not fit the last packet. Static, since it holds
    /// up to [`RECORD_MAX`] bytes.
    held: &'static mut Held,
    updates: Updates,
}

impl Link {
    pub fn new(
        boot: Instant,
        mac: Mac,
        cause: ResetCause,
        phase: LoopPhase,
        held: &'static mut Held,
        updates: Updates,
    ) -> Self {
        Self {
            boot,
            mac,
            cause,
            phase,
            seq: 0,
            p4_seq: SeqCheck::new(),
            heard_p4: false,
            hello_sent: None,
            crc_fail: 0,
            bad_magic: 0,
            seq_gap: 0,
            uart_errors: 0,
            undecoded: 0,
            undecoded_said: 0,
            drained: 0,
            ble_stats: BleStats::default(),
            held,
            updates,
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            link_rev: LINK_REVISION,
            chip: Chip::Esp32C6,
            mac: self.mac,
            fw_version: ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
            reset_cause: self.cause,
            last_phase: self.phase,
            uptime_ms: self.boot.elapsed().as_millis() as u32,
            image: self.updates.image,
        }
    }

    /// Act on what the reader made of a packet from the P4.
    fn take(&mut self, packet: Result<Packet<'_>, VerifyError>, shared: &Shared) {
        let packet = match packet {
            Ok(packet) => packet,
            Err(VerifyError::BadMagic) => {
                self.bad_magic = self.bad_magic.wrapping_add(1);
                return;
            }
            Err(VerifyError::BadCrc) => {
                self.crc_fail = self.crc_fail.wrapping_add(1);
                return;
            }
            // Another core, a bad `len` or an unknown kind, each under a good CRC: no P4
            // built from this tree sends one, and `RadioStatus` has no counter for them.
            Err(VerifyError::OtherCore(_) | VerifyError::BadLen | VerifyError::UnknownKind(_)) => {
                return;
            }
        };
        match self.p4_seq.note(packet.seq) {
            SeqNote::Gap => self.seq_gap = self.seq_gap.wrapping_add(1),
            SeqNote::First | SeqNote::InOrder => {}
        }
        match packet.kind {
            Kind::Hello => {
                if Hello::decode(packet.payload).is_some() {
                    let first = !self.heard_p4;
                    self.heard_p4 = true;
                    // What queued behind the hello exchange can go now.
                    shared.wake.signal(());
                    self.confirm(shared);
                    if first {
                        claim_subscriber(shared);
                    }
                }
            }
            Kind::Messages => {
                mark(LoopPhase::Command);
                for record in records(packet.payload) {
                    self.act(record, shared);
                }
            }
            Kind::Update => {
                mark(LoopPhase::Command);
                if let Some(op) = Op::decode(packet.payload) {
                    self.update(op, shared);
                }
            }
        }
        mark(LoopPhase::RadioLink);
        // Once per change, so a P4 sending garbage costs one line per packet at most.
        if self.undecoded != self.undecoded_said {
            self.undecoded_said = self.undecoded;
            shared.error(format_args!("a P4 command did not decode"));
        }
    }

    /// End this image's trial, if it is on one. A failure leaves it on trial, and the P4's
    /// next hello tries again.
    fn confirm(&mut self, shared: &Shared) {
        if !self.updates.trial {
            return;
        }
        match self.updates.ota.confirm() {
            Ok(()) => {
                self.updates.trial = false;
                shared.log(format_args!("image confirmed by the P4"));
            }
            Err(err) => shared.error(format_args!("could not confirm the image: {err}")),
        }
    }

    /// Hand an update op to the receiver, keeping its status for the next packet.
    ///
    /// A `begin` is refused while this image is on trial, and on a table without a `trial`
    /// sector or a second app slot, as a node refuses one ([`receive`]).
    fn update(&mut self, op: Op<'_>, shared: &Shared) {
        let updates = &mut self.updates;
        let received =
            receive(&mut *updates.receiver, op, updates.ota.slot().as_mut(), updates.trial);
        let status = match received {
            Received::Status(status) => status,
            Received::Install(session) => self.install(session, shared),
        };
        if status.is_some() {
            self.updates.status = status;
            shared.wake.signal(());
        }
    }

    /// Switch to the verified image and reboot into it. Returns only when the session is
    /// not verified, or the switch failed, with the status to send.
    fn install(&mut self, session: u8, shared: &Shared) -> Option<UpdateStatus> {
        let receiver = &mut *self.updates.receiver;
        if !receiver.is_verified() || receiver.status().session != session {
            return Some(receiver.status());
        }
        match self.updates.ota.install() {
            // The notes cannot reach the P4 before the reboot; the new image's hello says
            // the rest.
            Ok(_) => reboot(),
            Err(err) => {
                shared.error(format_args!("could not switch to the new image: {err}"));
                receiver.reset();
                Some(UpdateStatus { session, state: UpdateState::FlashError, next_offset: 0 })
            }
        }
    }

    /// Carry out one record from the P4.
    fn act(&mut self, record: &[u8], shared: &Shared) {
        let Ok(down) = decode_down(record) else {
            self.undecoded = self.undecoded.wrapping_add(1);
            return;
        };
        match down {
            Down::GetRadioStatus => shared.push(&Up::RadioStatus(self.status(shared))),
            Down::HostBytes(bytes) => self.host_bytes(bytes, shared),
        }
    }

    /// Take the host stream's next bytes: credited at once in the discard sink, ignored
    /// otherwise.
    fn host_bytes(&mut self, bytes: &[u8], shared: &Shared) {
        if cfg!(feature = "sink-discard") {
            self.drained = self.drained.wrapping_add(bytes.len() as u32);
            shared.push(&Up::BleCredit { drained: self.drained });
        }
    }

    /// The counters the P4 folds into the host's `Status`.
    fn status(&self, shared: &Shared) -> RadioStatus {
        let up = shared.up.borrow();
        let mut queues = Vec::new();
        let _ = queues.push(QueueStat {
            queue: QueueId::RadioUp,
            capacity: UP_QUEUE_BYTES as u32,
            high_water: up.high_water() as u32,
            dropped: up.dropped(),
        });
        RadioStatus {
            queues,
            crc_fail: self.crc_fail,
            bad_magic: self.bad_magic,
            seq_gap: self.seq_gap,
            uart_errors: self.uart_errors,
        }
    }

    /// Build the next packet to the P4 in `body` and return its length, or `None` when there
    /// is nothing to send.
    fn prepare(&mut self, body: &mut [u8; BODY_MAX], shared: &Shared) -> Option<usize> {
        let payload = &mut body[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX];
        let (kind, len) = self.fill(payload, shared)?;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(seal(body, kind, seq, len))
    }

    /// Fill a payload: the hello, when one is due and the P4's has not arrived, then the update
    /// status, then as many queued records as fit.
    fn fill(&mut self, payload: &mut [u8], shared: &Shared) -> Option<(Kind, usize)> {
        if !self.heard_p4 {
            let due =
                self.hello_sent.is_none_or(|sent| sent.elapsed().as_millis() >= HELLO_EVERY_MS);
            if !due {
                return None;
            }
            self.hello_sent = Some(Instant::now());
            return Some((Kind::Hello, self.hello().encode(payload)));
        }
        if let Some(status) = self.updates.status.take() {
            return Some((Kind::Update, Op::Status(status).encode(payload)));
        }
        let mut packer = Packer::new(payload);
        let mut up = shared.up.borrow_mut();
        self.held.pack(&mut packer, |out| up.read(out).0);
        (!packer.is_empty()).then(|| (Kind::Messages, packer.len()))
    }
}

/// In the discard sink, tell the P4 a central has subscribed, so it streams its host queue
/// here. A hello's arrival is the first moment the P4 reads records, and it reads this one
/// after the new life it saw in the hello.
fn claim_subscriber(shared: &Shared) {
    if cfg!(feature = "sink-discard") {
        shared.push(&Up::BleState(BleState {
            subscribed: true,
            mtu: 0,
            phy: 0,
            interval_us: 0,
            conn: 0,
        }));
    }
}

/// Run the link forever: the reader, the writer and the Bluetooth counters.
pub async fn link_loop(bus: Bus<'_>, link: &RefCell<Link>, shared: &Shared) -> ! {
    let Bus { rx, tx, rx_bufs, tx_buf, reader, body } = bus;
    join3(
        read_packets(rx, rx_bufs, reader, link, shared),
        write_packets(tx, tx_buf, body, link, shared),
        report_ble(link, shared),
    )
    .await
    .0
}

/// Send the Bluetooth counters every [`BLE_STATS_EVERY`] once the P4 has said hello.
async fn report_ble(link: &RefCell<Link>, shared: &Shared) -> ! {
    loop {
        Timer::after(BLE_STATS_EVERY).await;
        let (heard, stats) = {
            let link = link.borrow();
            (link.heard_p4, link.ble_stats)
        };
        if heard {
            shared.push(&Up::BleStats(stats));
        }
    }
}

/// Read forever: arm the next read, hand the last one's bytes to the link, wait for it to end.
async fn read_packets(
    mut rx: UhciRx<'_, Async>,
    [mut armed, mut last]: [DmaRxBuf; 2],
    reader: &'static mut PacketReader,
    link: &RefCell<Link>,
    shared: &Shared,
) -> ! {
    let mut last_len = 0;
    loop {
        let mut read = rx.read(armed).map_err(|(err, ..)| err).expect("arm a UART read");
        mark(LoopPhase::RadioLink);
        for &byte in &last.as_slice()[..last_len] {
            if let Some(packet) = reader.push(byte) {
                link.borrow_mut().take(packet, shared);
            }
        }
        read.wait_for_done().await;
        let (result, rx_back, done) = read.wait();
        rx = rx_back;
        armed = core::mem::replace(&mut last, done);
        last_len = last.number_of_received_bytes();
        // The UART keeps line errors until they are read, so any since the last read are here.
        if result.is_err() || rx.uart_rx.check_for_errors().is_err() {
            let mut link = link.borrow_mut();
            link.uart_errors = link.uart_errors.wrapping_add(1);
        }
    }
}

/// Write forever: every packet the link has to send, then wait for a reason to look again.
async fn write_packets(
    mut tx: UhciTx<'_, Async>,
    mut buf: DmaTxBuf,
    body: &'static mut [u8; BODY_MAX],
    link: &RefCell<Link>,
    shared: &Shared,
) -> ! {
    loop {
        let prepared = link.borrow_mut().prepare(body, shared);
        let Some(len) = prepared else {
            // Its next hello is due with or without a signal.
            if !link.borrow().heard_p4 {
                select(shared.wake.wait(), Timer::after(HELLO_EVERY)).await;
            } else {
                shared.wake.wait().await;
            }
            continue;
        };
        let wire = encode_packet(&body[..len], buf.as_mut_slice());
        buf.set_length(wire);
        mark(LoopPhase::RadioLink);
        let write = tx.write(buf).map_err(|(err, ..)| err).expect("start a UART write");
        // Ten bits a byte: 8N1.
        let wire_time = Duration::from_micros(wire as u64 * 10 * 1_000_000 / BAUD_RATE as u64);
        Timer::after(wire_time).await;
        let mut waited = wire_time;
        // Neither call reports a DMA error, so `uart_errors` holds only what a read finds.
        while !write.is_done() {
            let poll = if waited < wire_time * 2 { WRITE_POLL } else { WRITE_POLL_HELD };
            Timer::after(poll).await;
            waited += poll;
        }
        (tx, buf) = write.cancel();
    }
}

/// `args` as a line, cut at a line's length.
fn text(args: fmt::Arguments<'_>) -> LogStr {
    let mut message = LogStr::new();
    let _ = message.write_fmt(args);
    message
}
