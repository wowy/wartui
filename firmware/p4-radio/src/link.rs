//! The C6's end of the UART link to the P4: what each packet carries up, and what the C6
//! does with what comes down.
//!
//! `wartui_proto::spi` has the framing. The link is three futures, and nothing ticks: the
//! reader wakes when a DMA read ends, the results pump when a radio job finishes, and the writer
//! when [`Shared`]'s `wake` says there may be something to send. The writer loops until it has
//! nothing left before it waits, so a record left held or queued never waits for a new signal.
//! They share [`Link`] through a `RefCell`, with nothing borrowed across an `.await`.
//!
//! **A read ends at `RX_BUF_LEN` bytes or when the line goes idle.** The next read is armed
//! before this one's bytes are looked at, which is why there are two buffers: DMA keeps
//! landing while the C6 works, and a flash erase holds the CPU but not the DMA. Only the moment
//! between two reads leans on the 128-byte FIFO, with RTS behind it.
//!
//! Records go up through [`Shared`]'s two byte rings. `hear_loop` in `main.rs` fills the
//! bulk ring with received frames, and this link fills the priority ring with replies,
//! lines and radio results. A full ring evicts its oldest record: the P4 wants the newest
//! frames when it falls behind, as the host does. The priority ring keeps a backlog of
//! frames from evicting a send result or the C6's status, and drains first, between
//! records, as the P4's own queue toward the host does (`wartui_proto::host_queue`).
//!
//! The C6 sends a hello every [`HELLO_EVERY_MS`] until it hears the P4's. The P4 tells a repeat
//! from a new life by the packet seq, which starts at 0 on every boot
//! (`wartui_proto::split`, `Pair::on_hello`). The hello carries the booted image's digest,
//! which is how the P4 decides to send its bundled image.
//!
//! **The P4's hello confirms an image on trial.** Hearing the P4 is the test, as hearing the
//! host is a node's; `firmware/common/src/ota.rs` has the trial.
//!
//! **An update arrives as [`Kind::Update`] packets** (`wartui_proto::spi::update`), which
//! drive [`wartui_proto::ota::Receiver`] over [`wartui_firmware::ota`], as a node's update
//! frames do. Each status goes up in the next packet, ahead of any records. A sector's flash
//! work holds the task, so what the P4 sends meanwhile waits in DMA and the FIFO, then behind
//! RTS; the P4 sends no radio records while it updates. `Install` on a verified session
//! switches slots and reboots, and the P4 hears the new image's hello.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};

use embassy_futures::join::join3;
use embassy_futures::select::select;
use embassy_time::{Duration, Timer};
use esp_hal::Async;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::time::Instant;
use esp_hal::uart::uhci::{UhciRx, UhciTx};
use esp_radio::esp_now::EspNowManager;
use wartui_firmware::ota::{Note, Ota};
use wartui_firmware::phase::mark;
use wartui_firmware::radio::{
    self, Command, Done, Dones, Full, Jobs, Payloads, Said, SendDone, Wake,
};
use wartui_firmware::reboot;
use wartui_proto::air::{OtaState, OtaStatusMsg};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BridgeToHost, Chip, HostToBridge, LogLevel, LogStr, LoopPhase, QueueId, QueueStat, ShortStr,
};
use wartui_proto::mac::Mac;
use wartui_proto::ota::Receiver;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::update::{Op, Received, receive};
use wartui_proto::spi::{
    BODY_MAX, Down, HEADER_LEN, HELLO_EVERY_MS, Held, Hello, Kind, LINK_REVISION, PAYLOAD_MAX,
    Packer, Packet, PacketReader, RECORD_MAX, RadioStatus, SeqCheck, SeqNote, Up, VerifyError,
    decode_down, encode_packet, encode_up, records, seal,
};

use crate::config::{RADIO_JOBS, SEND_JOBS, UP_PRIORITY_BYTES, UP_QUEUE_BYTES};

/// How long a write waits before `wait_for_done`, so the DMA has put bytes in the TX FIFO;
/// `firmware/p4-bridge/src/link.rs`, `FIFO_FILL`, has why.
const FIFO_FILL: Duration = Duration::from_micros(20);

/// How often a C6 that has not heard the P4 looks again, with nothing else to wake it.
const HELLO_EVERY: Duration = Duration::from_millis(HELLO_EVERY_MS);

/// The ring of encoded received frames waiting for a packet.
pub type UpQueue = RefCell<FrameRing<UP_QUEUE_BYTES>>;

/// The ring of every other encoded [`Up`] record ([`Up::is_priority`]) waiting for a
/// packet.
pub type UpPriority = RefCell<FrameRing<UP_PRIORITY_BYTES>>;

/// What the radio side and the link share. They are futures joined in one task, so
/// nothing here is borrowed across an `.await`.
pub struct Shared {
    up: &'static UpQueue,
    priority: &'static UpPriority,
    /// Frames received and queued for the P4.
    rx_count: Cell<u32>,
    /// Received frames too long to be a node's, dropped.
    rx_oversize: Cell<u32>,
    /// Raised by whatever may have given the writer something to send, so a waiting writer
    /// looks again.
    wake: Wake,
}

impl Shared {
    /// Nothing counted yet. The rings are static, since `up` is the firmware's largest
    /// buffer.
    pub fn new(up: &'static UpQueue, priority: &'static UpPriority) -> Self {
        Self { up, priority, rx_count: Cell::new(0), rx_oversize: Cell::new(0), wake: Wake::new() }
    }

    /// Queue `msg` for the P4 in its ring, evicting that ring's oldest records if it is
    /// full.
    ///
    /// Out of line, with the encode buffer, to keep both out of the callers' frames.
    #[inline(never)]
    pub fn push(&self, msg: &Up) {
        // No `Up` comes near `RECORD_MAX`; the largest is a forwarded 1024-byte frame.
        let mut record = [0; RECORD_MAX];
        // Only a record longer than any `Up` fails, and the ring counts its own drops.
        if let Ok(len) = encode_up(msg, &mut record) {
            if msg.is_priority() {
                self.priority.borrow_mut().push(&record[..len]);
            } else {
                self.up.borrow_mut().push(&record[..len]);
            }
            self.wake.signal(());
        }
    }

    /// Pass on what a radio call had to say.
    pub fn say(&self, said: Said) {
        let message = match said {
            Said::Nothing => return,
            Said::Log(level, text) => {
                BridgeToHost::Log { level, message: LogStr::try_from(text).unwrap_or_default() }
            }
            Said::Error(text) => {
                BridgeToHost::Error { message: LogStr::try_from(text).unwrap_or_default() }
            }
        };
        self.push(&Up::Message(message));
    }

    /// Count a frame queued for the P4.
    pub fn note_rx(&self) {
        self.rx_count.set(self.rx_count.get().wrapping_add(1));
    }

    /// Count a frame too long to be a node's.
    pub fn note_oversize(&self) {
        self.rx_oversize.set(self.rx_oversize.get().wrapping_add(1));
    }

    /// Send the P4 a line composed here: a `Log` at `level`, or an `Error` without one.
    /// Text past a line's length is cut.
    #[inline(never)]
    pub fn line(&self, level: Option<LogLevel>, args: fmt::Arguments<'_>) {
        let mut message = LogStr::new();
        let _ = message.write_fmt(args);
        let msg = match level {
            Some(level) => BridgeToHost::Log { level, message },
            None => BridgeToHost::Error { message },
        };
        self.push(&Up::Message(msg));
    }

    /// Pass on what an update step had to say: a failure as an `Error`, anything else as an
    /// `Info` line.
    pub fn note(&self, note: &Note) {
        let level = if matches!(note, Note::Failed(..)) { None } else { Some(LogLevel::Info) };
        self.line(level, format_args!("{note}"));
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
    pub status: Option<OtaStatusMsg>,
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

/// The radio handles the P4's commands reach.
pub struct Radio<'a> {
    pub manager: &'a EspNowManager,
    pub jobs: &'a Jobs<RADIO_JOBS>,
    pub payloads: &'a Payloads<SEND_JOBS>,
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
    /// The P4's link revision, once its hello has arrived. Until then this end sends only its
    /// hello.
    p4_link_rev: Option<u16>,
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
    /// The most send payloads waiting at once.
    send_jobs_high_water: u32,
    /// Sends refused because the payload pool was full.
    send_jobs_dropped: u32,
    /// The most radio jobs waiting at once.
    radio_jobs_high_water: u32,
    /// Radio jobs refused because their queue was full.
    radio_jobs_dropped: u32,
    /// A record from the rings that did not fit the last packet. Static, since it holds
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
            p4_link_rev: None,
            hello_sent: None,
            crc_fail: 0,
            bad_magic: 0,
            seq_gap: 0,
            uart_errors: 0,
            undecoded: 0,
            undecoded_said: 0,
            send_jobs_high_water: 0,
            send_jobs_dropped: 0,
            radio_jobs_high_water: 0,
            radio_jobs_dropped: 0,
            held,
            updates,
        }
    }

    /// Microseconds since boot, wrapping after roughly 71 minutes. The host only subtracts
    /// one from another.
    fn us_since_boot(&self, at: Instant) -> u32 {
        (at - self.boot).as_micros() as u32
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
            heap_free: u32::try_from(esp_alloc::HEAP.free()).unwrap_or(u32::MAX),
            image: Some(self.updates.image),
        }
    }

    /// Act on what the reader made of a packet from the P4.
    fn take(
        &mut self,
        packet: Result<Packet<'_>, VerifyError>,
        shared: &Shared,
        radio: &Radio<'_>,
    ) {
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
                if let Some(hello) = Hello::decode(packet.payload) {
                    self.p4_link_rev = Some(hello.link_rev);
                    // What queued behind the hello exchange can go now.
                    shared.wake.signal(());
                    self.confirm(shared);
                }
            }
            Kind::Messages => {
                mark(LoopPhase::Command);
                for record in records(packet.payload) {
                    self.act(record, shared, radio);
                }
            }
            Kind::Update => {
                mark(LoopPhase::Command);
                if let Some(op) = Op::decode(packet.payload) {
                    self.update(op, shared);
                }
            }
        }
        mark(LoopPhase::SpiTransfer);
        // Once per change, so a P4 sending garbage costs one line per packet at most.
        if self.undecoded != self.undecoded_said {
            self.undecoded_said = self.undecoded;
            shared.say(Said::Error("a P4 command did not decode"));
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
                shared.line(Some(LogLevel::Info), format_args!("image confirmed by the P4"));
            }
            Err(err) => shared.line(None, format_args!("could not confirm the image: {err}")),
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
    fn install(&mut self, session: u8, shared: &Shared) -> Option<OtaStatusMsg> {
        let receiver = &mut *self.updates.receiver;
        if !receiver.is_verified() || receiver.status().session != session {
            return Some(receiver.status());
        }
        match self.updates.ota.install() {
            // The notes cannot reach the P4 before the reboot; the new image's hello says
            // the rest.
            Ok(_) => reboot(),
            Err(err) => {
                shared.line(None, format_args!("could not switch to the new image: {err}"));
                receiver.reset();
                Some(OtaStatusMsg { session, state: OtaState::FlashError, next_offset: 0 })
            }
        }
    }

    /// Carry out one record from the P4.
    ///
    /// Out of line: a decoded [`Down`] is 1.5 KB.
    #[inline(never)]
    fn act(&mut self, record: &[u8], shared: &Shared, radio: &Radio<'_>) {
        let Ok(down) = decode_down(record) else {
            self.undecoded = self.undecoded.wrapping_add(1);
            return;
        };
        match &down {
            Down::GetRadioStatus => shared.push(&Up::RadioStatus(self.status(shared, radio))),
            Down::Command(command) => self.queue_command(command, shared, radio),
        }
    }

    /// Hand a radio command to `radio::run_jobs`, which keeps the order commands arrive in.
    /// A full queue is answered now, and counted. The P4 answers every other command itself.
    ///
    /// Out of line for the reason [`Link::act`] is: a send's 1470-byte payload is copied
    /// into the pool.
    #[inline(never)]
    fn queue_command(&mut self, command: &HostToBridge, shared: &Shared, radio: &Radio<'_>) {
        let command = match *command {
            HostToBridge::AddPeer { mac } => Command::AddPeer(mac),
            HostToBridge::RemovePeer { mac } => Command::RemovePeer(mac),
            HostToBridge::SetTxPower { power } => Command::SetTxPower(power),
            HostToBridge::SendEspNow { id, dst, ensure_peer, ref payload } => {
                let queued =
                    radio::queue_send(radio.jobs, radio.payloads, id, dst, ensure_peer, payload);
                return self.note_queued(queued, shared, radio);
            }
            _ => return,
        };
        let queued = radio::queue_command(radio.jobs, command).map(|done| (Full::Jobs, done));
        self.note_queued(queued, shared, radio);
    }

    /// Report a refusal and count it against the queue that was full, and note both
    /// queues' depths.
    fn note_queued(&mut self, refused: Option<(Full, Done)>, shared: &Shared, radio: &Radio<'_>) {
        if let Some((full, done)) = refused {
            match full {
                Full::Jobs => self.radio_jobs_dropped = self.radio_jobs_dropped.wrapping_add(1),
                Full::Payloads => self.send_jobs_dropped = self.send_jobs_dropped.wrapping_add(1),
            }
            self.report(&done, shared);
        }
        let jobs = u32::try_from(radio.jobs.len()).unwrap_or(u32::MAX);
        self.radio_jobs_high_water = self.radio_jobs_high_water.max(jobs);
        let payloads = u32::try_from(radio.payloads.len()).unwrap_or(u32::MAX);
        self.send_jobs_high_water = self.send_jobs_high_water.max(payloads);
    }

    /// The counters the P4 folds into the host's `Status`.
    fn status(&self, shared: &Shared, radio: &Radio<'_>) -> RadioStatus {
        let peer_count = radio
            .manager
            .peer_count()
            .map(|count| count.total_count.clamp(0, u8::MAX as i32) as u8)
            .unwrap_or(0);
        let up = shared.up.borrow();
        let priority = shared.priority.borrow();
        let mut queues = Vec::new();
        let _ = queues.push(QueueStat {
            queue: QueueId::RadioUp,
            capacity: UP_QUEUE_BYTES as u32,
            high_water: up.high_water() as u32,
            dropped: up.dropped(),
        });
        let _ = queues.push(QueueStat {
            queue: QueueId::RadioSendJobs,
            capacity: SEND_JOBS as u32,
            high_water: self.send_jobs_high_water,
            dropped: self.send_jobs_dropped,
        });
        let _ = queues.push(QueueStat {
            queue: QueueId::RadioUpPriority,
            capacity: UP_PRIORITY_BYTES as u32,
            high_water: priority.high_water() as u32,
            dropped: priority.dropped(),
        });
        let _ = queues.push(QueueStat {
            queue: QueueId::RadioJobs,
            capacity: RADIO_JOBS as u32,
            high_water: self.radio_jobs_high_water,
            dropped: self.radio_jobs_dropped,
        });
        RadioStatus {
            peer_count,
            rx_count: shared.rx_count.get(),
            rx_oversize: shared.rx_oversize.get(),
            heap_free: u32::try_from(esp_alloc::HEAP.free()).unwrap_or(u32::MAX),
            uptime_ms: self.boot.elapsed().as_millis() as u32,
            queues,
            crc_fail: self.crc_fail,
            bad_magic: self.bad_magic,
            seq_gap: self.seq_gap,
            uart_errors: self.uart_errors,
        }
    }

    /// Queue what became of one send. `tx_us` is on this chip's clock, as `rx_us` is.
    fn report_send(&self, done: &SendDone, shared: &Shared) {
        let tx_us = self.us_since_boot(done.done_at);
        shared.push(&Up::Message(BridgeToHost::SendResult {
            id: done.id,
            status: done.status,
            tx_us,
        }));
    }

    /// Queue what became of one radio job.
    fn report(&self, done: &Done, shared: &Shared) {
        match done {
            Done::Sent(sent) => self.report_send(sent, shared),
            &Done::Said(said) => shared.say(said),
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
        if self.p4_link_rev.is_none() {
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
        let mut priority = shared.priority.borrow_mut();
        // The priority ring wins only between records.
        self.held.pack(&mut packer, |out| {
            if priority.is_empty() { up.read(out).0 } else { priority.read(out).0 }
        });
        (!packer.is_empty()).then(|| (Kind::Messages, packer.len()))
    }

    /// Whether the writer must look again on a timer: its next hello is due with or without a
    /// signal.
    fn awaits_hello(&self) -> bool {
        self.p4_link_rev.is_none()
    }
}

/// Run the link forever: the reader, the writer, and the pump for the radio's results.
pub async fn link_loop(
    bus: Bus<'_>,
    link: &RefCell<Link>,
    shared: &Shared,
    radio: Radio<'_>,
    dones: &Dones<RADIO_JOBS>,
) -> ! {
    let Bus { rx, tx, rx_bufs, tx_buf, reader, body } = bus;
    join3(
        read_packets(rx, rx_bufs, reader, link, shared, &radio),
        write_packets(tx, tx_buf, body, link, shared),
        report_results(link, shared, dones),
    )
    .await
    .0
}

/// Read forever: arm the next read, hand the last one's bytes to the link, wait for it to end.
async fn read_packets(
    mut rx: UhciRx<'_, Async>,
    [mut armed, mut last]: [DmaRxBuf; 2],
    reader: &'static mut PacketReader,
    link: &RefCell<Link>,
    shared: &Shared,
    radio: &Radio<'_>,
) -> ! {
    let mut last_len = 0;
    loop {
        let mut read = rx.read(armed).map_err(|(err, ..)| err).expect("arm a UART read");
        mark(LoopPhase::SpiTransfer);
        for &byte in &last.as_slice()[..last_len] {
            if let Some(packet) = reader.push(byte) {
                link.borrow_mut().take(packet, shared, radio);
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
            if link.borrow().awaits_hello() {
                select(shared.wake.wait(), Timer::after(HELLO_EVERY)).await;
            } else {
                shared.wake.wait().await;
            }
            continue;
        };
        let wire = encode_packet(&body[..len], buf.as_mut_slice());
        buf.set_length(wire);
        mark(LoopPhase::SpiTransfer);
        let mut write = tx.write(buf).map_err(|(err, ..)| err).expect("start a UART write");
        Timer::after(FIFO_FILL).await;
        write.wait_for_done().await;
        let (result, tx_back, done) = write.wait();
        tx = tx_back;
        buf = done;
        if result.is_err() {
            let mut link = link.borrow_mut();
            link.uart_errors = link.uart_errors.wrapping_add(1);
        }
    }
}

/// Queue what became of each job as `radio::run_jobs` finishes it.
async fn report_results(link: &RefCell<Link>, shared: &Shared, dones: &Dones<RADIO_JOBS>) -> ! {
    loop {
        let done = dones.receive().await;
        link.borrow().report(&done, shared);
    }
}
