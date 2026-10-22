//! The C6's end of the SPI link to the P4: what each transfer carries up, and what the
//! C6 does with what comes down.
//!
//! The P4 is master and clocks every transfer; `wartui_proto::spi` has the framing and the
//! READY handshake. This end keeps two buffers each way. While the P4 clocks one transfer,
//! the C6 acts on the transfer before it and prepares the one after. So a finished transfer
//! is followed at once by the next arm and READY, and the P4's wait for READY never
//! includes the C6's work. The spike measured that work as the link's ceiling.
//!
//! Records go up through [`Shared`]'s two byte rings. `hear_loop` in `main.rs` fills the
//! bulk ring with received frames, and [`link_loop`] fills the priority ring with replies,
//! lines and radio results. A full ring evicts its oldest record: the P4 wants the newest
//! frames when it falls behind, as the host does. The priority ring keeps a backlog of
//! frames from evicting a send result or the C6's status, and drains first, between
//! records, as the P4's own queue toward the host does (`wartui_proto::host_queue`).
//!
//! The C6 sends its hello in every transfer until it hears the P4's. The P4 tells a repeat
//! from a new life by the transfer seq, which starts at 0 on every boot
//! (`wartui_proto::split`, `Pair::on_hello`). The hello carries the booted image's digest,
//! which is how the P4 decides to send its bundled image.
//!
//! **The P4's hello confirms an image on trial.** Hearing the P4 is the test, as hearing the
//! host is a node's; `firmware/common/src/ota.rs` has the trial.
//!
//! **An update arrives as [`Kind::Update`] transfers** (`wartui_proto::spi::update`), which
//! drive [`wartui_proto::ota::Receiver`] over [`wartui_firmware::ota`], as a node's update
//! frames do. Each status goes up in the next transfer, ahead of any records. A sector's
//! flash work holds the loop, so the next READY waits for it; the P4 sends no radio records
//! while it updates. `Install` on a verified session switches slots and reboots, and the P4
//! hears the new image's hello.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};

use embassy_futures::join::join;
use embassy_time::{Duration, with_timeout};
use esp_hal::Blocking;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::slave::dma::SpiDma;
use esp_hal::time::Instant;
use esp_radio::esp_now::EspNowManager;
use wartui_firmware::ota::{Note, Ota};
use wartui_firmware::phase::mark;
use wartui_firmware::radio::{self, Command, Done, Dones, Full, Jobs, Payloads, Said, SendDone};
use wartui_firmware::reboot;
use wartui_proto::air::{OtaBeginMsg, OtaChunkMsg, OtaCommitMsg, OtaState, OtaStatusMsg, Version};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BridgeToHost, Chip, HostToBridge, LogLevel, LogStr, LoopPhase, QueueId, QueueStat, ShortStr,
};
use wartui_proto::mac::Mac;
use wartui_proto::ota::Receiver;
use wartui_proto::ota::image::DIGEST_LEN;
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::update::Op;
use wartui_proto::spi::{
    Down, HEADER_LEN, Held, Hello, Kind, LINK_REVISION, PAYLOAD_MAX, Packer, RECORD_MAX,
    RadioStatus, SeqCheck, SeqNote, TRANSFER_LEN, Up, VerifyError, decode_down, encode_up, records,
    seal, verify,
};

use crate::config::{RADIO_JOBS, SEND_JOBS, UP_PRIORITY_BYTES, UP_QUEUE_BYTES};

/// How long one wait for the end of a transfer lasts before checking whether the edge
/// was missed.
const EDGE_TIMEOUT: Duration = Duration::from_millis(100);

/// The ring of encoded received frames waiting for a transfer.
pub type UpQueue = RefCell<FrameRing<UP_QUEUE_BYTES>>;

/// The ring of every other encoded [`Up`] record ([`Up::is_priority`]) waiting for a
/// transfer.
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
}

impl Shared {
    /// Nothing counted yet. The rings are static, since `up` is the firmware's largest
    /// buffer.
    pub fn new(up: &'static UpQueue, priority: &'static UpPriority) -> Self {
        Self { up, priority, rx_count: Cell::new(0), rx_oversize: Cell::new(0) }
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
    /// The status to send the P4 in the next transfer.
    pub status: Option<OtaStatusMsg>,
}

/// The SPI slave and its pins and buffers.
pub struct Bus<'d> {
    pub spi: SpiDma<'d, Blocking>,
    /// The same pin the slave reads as CS, watched for the edge that ends a transfer.
    pub cs: Input<'d>,
    pub ready: Output<'d>,
    pub rx: [DmaRxBuf; 2],
    pub tx: [DmaTxBuf; 2],
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
    /// This end's seq, +1 per transfer. From 0 at every boot, which is how the P4 tells a
    /// restart from a repeated hello.
    seq: u32,
    p4_seq: SeqCheck,
    /// The P4's link revision, once its hello has arrived. Until then every transfer
    /// carries this end's hello.
    p4_link_rev: Option<u16>,
    /// P4 transfers that failed their CRC.
    crc_fail: u32,
    /// P4 transfers without the magic.
    bad_magic: u32,
    /// Breaks in the P4's seq.
    seq_gap: u32,
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
    /// A record from the rings that did not fit the last transfer. Static, since it holds
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
            crc_fail: 0,
            bad_magic: 0,
            seq_gap: 0,
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

    /// Microseconds since boot, wrapping after roughly 71 minutes, as the single-chip
    /// bridge stamps them. The host only subtracts one from another.
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

    /// Verify a transfer from the P4 and act on it.
    fn take(&mut self, buf: &[u8; TRANSFER_LEN], shared: &Shared, radio: &Radio<'_>) {
        let transfer = match verify(buf) {
            Ok(transfer) => transfer,
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
        match self.p4_seq.note(transfer.seq) {
            // The P4 sent this transfer again; acting on it twice would send twice.
            SeqNote::Repeat => return,
            SeqNote::Gap => self.seq_gap = self.seq_gap.wrapping_add(1),
            SeqNote::First | SeqNote::InOrder => {}
        }
        match transfer.kind {
            Kind::Hello => {
                if let Some(hello) = Hello::decode(transfer.payload) {
                    self.p4_link_rev = Some(hello.link_rev);
                    self.confirm(shared);
                }
            }
            Kind::Messages => {
                mark(LoopPhase::Command);
                for record in records(transfer.payload) {
                    self.act(record, shared, radio);
                }
            }
            Kind::Update => {
                mark(LoopPhase::Command);
                if let Some(op) = Op::decode(transfer.payload) {
                    self.update(op, shared);
                }
            }
            Kind::Idle => {}
        }
        // Once per change, so a P4 sending garbage costs one line per transfer at most.
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

    /// Hand an update op to the receiver, keeping its status for the next transfer.
    ///
    /// A `begin` is refused with [`OtaState::FlashError`] while this image is on trial, and
    /// on a table without a `trial` sector or a second app slot, as a node refuses one.
    fn update(&mut self, op: Op<'_>, shared: &Shared) {
        let updates = &mut self.updates;
        let receiver = &mut *updates.receiver;
        let status = match op {
            Op::Begin { session, len, sha256 } => {
                // The link's `Begin` names no version, and the receiver checks none.
                let version = Version::new(0, 0, 0);
                let begin = OtaBeginMsg { session, len, sha256, version, board: receiver.board() };
                match (updates.ota.slot(), updates.trial) {
                    (Some(slot), false) => Some(receiver.begin(&begin, &slot)),
                    _ => {
                        Some(OtaStatusMsg { session, state: OtaState::FlashError, next_offset: 0 })
                    }
                }
            }
            Op::Chunk { session, offset, data } => match updates.ota.slot() {
                Some(mut slot) => receiver.chunk(&OtaChunkMsg { session, offset, data }, &mut slot),
                None => Some(OtaStatusMsg { session, state: OtaState::Idle, next_offset: 0 }),
            },
            Op::Commit { session } => match updates.ota.slot() {
                Some(mut slot) => Some(receiver.commit(&OtaCommitMsg { session }, &mut slot)),
                None => Some(OtaStatusMsg { session, state: OtaState::Idle, next_offset: 0 }),
            },
            Op::Install { session } => self.install(session, shared),
            // Only the C6 sends a status.
            Op::Status(_) => None,
        };
        if status.is_some() {
            self.updates.status = status;
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

    /// Queue every job `radio::run_jobs` has finished.
    fn take_results(&self, dones: &Dones<RADIO_JOBS>, shared: &Shared) {
        while let Ok(done) = dones.try_receive() {
            self.report(&done, shared);
        }
    }

    /// Fill a transfer to the P4: the hello until the P4's has arrived, then as many
    /// queued records as fit, or nothing.
    fn prepare(&mut self, buf: &mut [u8; TRANSFER_LEN], shared: &Shared) {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        if self.p4_link_rev.is_none() {
            let len = self.hello().encode(&mut buf[HEADER_LEN..]);
            seal(buf, Kind::Hello, seq, len);
            return;
        }
        if let Some(status) = self.updates.status.take() {
            let len = Op::Status(status).encode(&mut buf[HEADER_LEN..]);
            seal(buf, Kind::Update, seq, len);
            return;
        }
        let mut packer = Packer::new(&mut buf[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX]);
        let mut up = shared.up.borrow_mut();
        let mut priority = shared.priority.borrow_mut();
        // The priority ring wins only between records.
        self.held.pack(&mut packer, |out| {
            if priority.is_empty() { up.read(out).0 } else { priority.read(out).0 }
        });
        drop((up, priority));
        let len = packer.len();
        seal(buf, if len == 0 { Kind::Idle } else { Kind::Messages }, seq, len);
    }
}

/// Run the link forever: arm a transfer, raise READY, act on the last transfer and prepare
/// the next while the P4 clocks, then wait for the end of this one.
pub async fn link_loop(
    bus: Bus<'_>,
    link: &mut Link,
    shared: &Shared,
    radio: Radio<'_>,
    dones: &Dones<RADIO_JOBS>,
) -> ! {
    let Bus {
        mut spi,
        mut cs,
        mut ready,
        rx: [mut rx_next, mut rx_last],
        tx: [mut tx_next, mut tx_spare],
    } = bus;
    link.prepare(tx_bytes(&mut tx_next), shared);
    // Whether `rx_last` holds a finished transfer: not until the first lands.
    let mut received = false;
    loop {
        let transfer = spi
            .transfer(TRANSFER_LEN, rx_next, TRANSFER_LEN, tx_next)
            .map_err(|(err, ..)| err)
            .expect("arm SPI slave transfer");
        mark(LoopPhase::SpiTransfer);
        // CS rises when the P4 ends the transfer. `join` polls the edge wait first, which
        // arms the pin's interrupt, and only then raises READY, so the P4 cannot finish a
        // transfer before the edge is being watched.
        let edge = with_timeout(EDGE_TIMEOUT, cs.wait_for_rising_edge());
        let work = async {
            ready.set_high();
            if received {
                link.take(rx_bytes(&rx_last), shared, &radio);
            }
            link.take_results(dones, shared);
            link.prepare(tx_bytes(&mut tx_spare), shared);
            mark(LoopPhase::SpiTransfer);
        };
        let (edge, ()) = join(edge, work).await;
        // A timeout is either a P4 that is not clocking, or an edge missed between two
        // waits. The transfer's own state tells them apart.
        if edge.is_err() {
            while !transfer.is_done() {
                if with_timeout(EDGE_TIMEOUT, cs.wait_for_rising_edge()).await.is_ok() {
                    break;
                }
            }
        }
        // The one poll in this firmware: esp-hal 1.2's SPI slave has no async API, and
        // `wait` is its only way to hand the buffers back. After the edge it spins only
        // for the DMA's end-of-frame and the bus-idle check.
        let (spi_back, (rx_done, tx_done)) = transfer.wait();
        ready.set_low();
        spi = spi_back;
        rx_next = core::mem::replace(&mut rx_last, rx_done);
        tx_next = core::mem::replace(&mut tx_spare, tx_done);
        received = true;
    }
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
