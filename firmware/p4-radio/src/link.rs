//! The C6's end of the SPI link to the P4: what each transfer carries up, and what the
//! C6 does with what comes down.
//!
//! The P4 is master and clocks every transfer; `wartui_proto::spi` has the framing and the
//! READY handshake. This end keeps two buffers each way. While the P4 clocks one transfer,
//! the C6 acts on the transfer before it and prepares the one after. So a finished transfer
//! is followed at once by the next arm and READY, and the P4's wait for READY never
//! includes the C6's work. The spike measured that work as the link's ceiling.
//!
//! Records go up through [`Shared`]'s byte ring, which `hear_loop` in `main.rs` fills from
//! the radio and [`link_loop`] fills with replies and send results. A full ring evicts its
//! oldest record: the P4 wants the newest frames when it falls behind, as the host does.
//!
//! The C6 sends its hello in every transfer until it hears the P4's. The P4 tells a repeat
//! from a new life by the uptime it carries (`wartui_proto::split`, `Pair::on_hello`).

use core::cell::{Cell, RefCell};

use embassy_futures::join::join;
use embassy_time::{Duration, with_timeout};
use esp_hal::Blocking;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::{Input, Output};
use esp_hal::spi::slave::dma::SpiDma;
use esp_hal::time::Instant;
use esp_radio::esp_now::EspNowManager;
use esp_radio::wifi::WifiController;
use wartui_firmware::phase::mark;
use wartui_firmware::radio::{self, Said, SendDone, SendDones, SendJob, SendJobs};
use wartui_proto::frame_ring::FrameRing;
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BridgeToHost, Chip, HostToBridge, LogStr, LoopPhase, QueueId, QueueStat, SendPayload, ShortStr,
};
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::{
    Down, HEADER_LEN, Hello, Kind, LINK_REVISION, PAYLOAD_MAX, Packer, RadioStatus, SeqCheck,
    SeqNote, TRANSFER_LEN, Up, VerifyError, decode_down, encode_up, records, seal, verify,
};

use crate::config::{SEND_JOBS, UP_QUEUE_BYTES};

/// The longest encoded record: a payload less its record's length prefix.
///
/// The ring queues nothing longer, so one record always fits an empty transfer. No [`Up`]
/// comes near it; the largest is a forwarded 1024-byte frame.
pub const RECORD_MAX: usize = PAYLOAD_MAX - 2;

/// How long one wait for the end of a transfer lasts before checking whether the edge
/// was missed.
const EDGE_TIMEOUT: Duration = Duration::from_millis(100);

/// The ring of encoded [`Up`] records waiting for a transfer.
pub type UpQueue = RefCell<FrameRing<UP_QUEUE_BYTES>>;

/// What the radio side and the link share. They are futures joined in one task, so
/// nothing here is borrowed across an `.await`.
pub struct Shared {
    up: &'static UpQueue,
    /// Frames received and queued for the P4.
    rx_count: Cell<u32>,
    /// Received frames too long to be a node's, dropped.
    rx_oversize: Cell<u32>,
}

impl Shared {
    /// Nothing counted yet. `up` is static, since it is the firmware's largest buffer.
    pub fn new(up: &'static UpQueue) -> Self {
        Self { up, rx_count: Cell::new(0), rx_oversize: Cell::new(0) }
    }

    /// Queue `msg` for the P4, evicting the oldest records if the ring is full.
    ///
    /// Out of line, with the encode buffer, to keep both out of the callers' frames.
    #[inline(never)]
    pub fn push(&self, msg: &Up) {
        let mut record = [0; RECORD_MAX];
        // Only a record longer than any `Up` fails, and the ring counts its own drops.
        if let Ok(len) = encode_up(msg, &mut record) {
            self.up.borrow_mut().push(&record[..len]);
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
pub struct Radio<'a, 'd> {
    pub controller: &'a mut WifiController<'d>,
    pub manager: &'a EspNowManager,
    pub jobs: &'a SendJobs<SEND_JOBS>,
}

/// The link's state.
pub struct Link {
    boot: Instant,
    mac: Mac,
    /// Why this life started, and where the last one stopped, for the hello.
    cause: ResetCause,
    phase: LoopPhase,
    /// This end's seq, +1 per transfer.
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
    /// The transmit power the radio is known to be at; see `radio::set_tx_power_command`.
    tx_power: Option<i8>,
    /// The most send jobs waiting at once.
    send_jobs_high_water: u32,
    /// Send jobs refused because the queue was full.
    send_jobs_dropped: u32,
    /// A record read from the ring that did not fit the last transfer, sent first in the
    /// next. Static, since it holds up to [`RECORD_MAX`] bytes.
    held: &'static mut [u8; RECORD_MAX],
    held_len: usize,
}

impl Link {
    pub fn new(
        boot: Instant,
        mac: Mac,
        cause: ResetCause,
        phase: LoopPhase,
        tx_power: Option<i8>,
        held: &'static mut [u8; RECORD_MAX],
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
            tx_power,
            send_jobs_high_water: 0,
            send_jobs_dropped: 0,
            held,
            held_len: 0,
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
        }
    }

    /// Verify a transfer from the P4 and act on it.
    fn take(&mut self, buf: &[u8; TRANSFER_LEN], shared: &Shared, radio: &mut Radio<'_, '_>) {
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
                }
            }
            Kind::Messages => {
                mark(LoopPhase::Command);
                for record in records(transfer.payload) {
                    self.act(record, shared, radio);
                }
            }
            Kind::Idle | Kind::Update => {}
        }
        // Once per change, so a P4 sending garbage costs one line per transfer at most.
        if self.undecoded != self.undecoded_said {
            self.undecoded_said = self.undecoded;
            shared.say(Said::Error("a P4 command did not decode"));
        }
    }

    /// Carry out one record from the P4.
    ///
    /// Out of line: a decoded [`Down`] is 1.5 KB.
    #[inline(never)]
    fn act(&mut self, record: &[u8], shared: &Shared, radio: &mut Radio<'_, '_>) {
        let Ok(down) = decode_down(record) else {
            self.undecoded = self.undecoded.wrapping_add(1);
            return;
        };
        match &down {
            Down::GetRadioStatus => shared.push(&Up::RadioStatus(self.status(shared, radio))),
            Down::Command(HostToBridge::AddPeer { mac }) => {
                shared.say(radio::add_peer_command(radio.manager, mac));
            }
            Down::Command(HostToBridge::RemovePeer { mac }) => {
                shared.say(radio::remove_peer_command(radio.manager, mac));
            }
            &Down::Command(HostToBridge::SetTxPower { power }) => {
                let said = radio::set_tx_power_command(radio.controller, power, &mut self.tx_power);
                shared.say(said);
            }
            &Down::Command(HostToBridge::SendEspNow { id, dst, ensure_peer, ref payload }) => {
                self.queue_send(id, dst, ensure_peer, payload, shared, radio);
            }
            // The P4 answers the rest itself.
            Down::Command(_) => {}
        }
    }

    /// Hand one send to `radio::run_sends`. A full queue is answered `Rejected` now.
    ///
    /// Out of line for the reason [`Link::act`] is: the job holds a 1470-byte payload.
    #[inline(never)]
    fn queue_send(
        &mut self,
        id: u16,
        dst: Mac,
        ensure_peer: bool,
        payload: &SendPayload,
        shared: &Shared,
        radio: &Radio<'_, '_>,
    ) {
        let job = SendJob { id, dst, ensure_peer, payload: payload.clone() };
        if let Some(refused) = radio::queue_send(radio.jobs, job) {
            self.send_jobs_dropped = self.send_jobs_dropped.wrapping_add(1);
            self.report_send(&refused, shared);
        }
        let waiting = u32::try_from(radio.jobs.len()).unwrap_or(u32::MAX);
        self.send_jobs_high_water = self.send_jobs_high_water.max(waiting);
    }

    /// The counters the P4 folds into the host's `Status`.
    fn status(&self, shared: &Shared, radio: &Radio<'_, '_>) -> RadioStatus {
        let peer_count = radio
            .manager
            .peer_count()
            .map(|count| count.total_count.clamp(0, u8::MAX as i32) as u8)
            .unwrap_or(0);
        let up = shared.up.borrow();
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

    /// Queue every send `radio::run_sends` has finished.
    fn take_send_results(&self, dones: &SendDones<SEND_JOBS>, shared: &Shared) {
        while let Ok(done) = dones.try_receive() {
            self.report_send(&done, shared);
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
        let mut packer = Packer::new(&mut buf[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX]);
        let mut up = shared.up.borrow_mut();
        loop {
            if self.held_len == 0 {
                if up.is_empty() {
                    break;
                }
                // One read takes a whole record: `Shared::push` queues none longer than
                // `held`.
                let (len, _) = up.read(&mut self.held[..]);
                self.held_len = len;
            }
            if !packer.push_encoded(&self.held[..self.held_len]) {
                break;
            }
            self.held_len = 0;
        }
        drop(up);
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
    mut radio: Radio<'_, '_>,
    dones: &SendDones<SEND_JOBS>,
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
                link.take(rx_bytes(&rx_last), shared, &mut radio);
            }
            link.take_send_results(dones, shared);
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
