//! The P4's end of the UART link to the C6: what each packet carries down, and what the
//! P4 does with what comes up.
//!
//! `wartui_proto::radio_link` has the framing. The link is two futures, and nothing ticks: the
//! reader wakes when a DMA read ends, and the writer when [`Shared`]'s `wake_link` says there may
//! be something to send. The writer loops until it has nothing left before it waits, so a record
//! left held or queued never waits for a new signal. They share [`Link`] through a `RefCell`,
//! with nothing borrowed across an `.await`.
//!
//! **A read ends at `RX_BUF_LEN` bytes or when the line goes idle.** The next read is armed
//! before this one's bytes are looked at, which is why there are two buffers: DMA keeps
//! landing while the P4 works, and only the moment between two reads leans on the 128-byte
//! FIFO, with RTS behind it. A packet may span two reads, and [`PacketReader`] absorbs that.
//!
//! **A write held by CTS waits without blocking**, so USB, the radio clock and the GPS reader
//! run on while the C6 is in reset or busy in its flash. While a write is held the sender's
//! clock stops too, so a C6 that hangs mid-update holding RTS stays `Updating`, and a host's poll
//! going unanswered is what raises the fault (`Pair::fault`, the radio has stopped answering).
//!
//! The P4 answers each hello from the C6 with one of its own, which covers a C6 that restarts
//! under a running P4 as well as the first exchange. The pair hears the seq of every C6
//! packet, since a hello whose seq is not past the last is a restart.
//!
//! Records flow only while the pair bridges (`Pair::bridges`): a C6 that encodes them
//! differently is sent none, and its records are not read. A C6 updating on this P4's revision
//! has its records read too (`Pair::reads_records`), so its lines about the update reach the
//! host. Only its lines and status are relayed then; the frames it received meanwhile are
//! dropped.
//!
//! While the pair is `Updating`, the writer sends the next op from a [`Sender`] of the bundled
//! image ([`crate::radio`]), polling it on each status from the C6, which come back as
//! [`Kind::Update`] packets, and every [`SENDER_POLL_MS`]. The sender's progress goes to the
//! host as the pair's fault, every tenth of the image and at 100%, then once more when the C6
//! is told to install. A sender that fails leaves the pair `Stale` or `Mismatch`; one that
//! installs leaves the pair waiting for the C6's next hello.

use core::cell::RefCell;
use core::fmt::Write;

use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::Async;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::uart::uhci::{UhciRx, UhciTx};
use wartui_firmware::phase::mark;
use wartui_proto::link::{BridgeToHost, Chip, LogStr, LoopPhase, ShortStr};
use wartui_proto::mac::Mac;
use wartui_proto::radio_link::update::{Failure, Op, Sender, Stage};
use wartui_proto::radio_link::{
    BODY_MAX, HEADER_LEN, Held, Hello, Kind, LINK_REVISION, PAYLOAD_MAX, Packer, Packet,
    PacketReader, SENDER_POLL_MS, SeqCheck, SeqNote, Up, VerifyError, decode_up, encode_packet,
    records, seal,
};
use wartui_proto::reset::ResetCause;
use wartui_proto::split::{PairEvent, RadioState};

use crate::Shared;
use crate::radio::{RADIO_DIGEST, RADIO_IMAGE, RADIO_SHA256};

/// How long a write waits before `wait_for_done`, so the DMA has put bytes in the TX FIFO.
///
/// `wait_for_done` flushes the UART first. It awaits while the FIFO holds anything, but spins
/// until the transmitter is idle once the FIFO reads empty. Reached before the DMA's first
/// byte, that spin lasts the packet. Shorter than the fewest bytes a packet takes at any baud
/// rate this link allows.
const FIFO_FILL: Duration = Duration::from_micros(20);

/// How often an updating writer polls its [`Sender`] with no status to prompt it.
const SENDER_POLL: Duration = Duration::from_millis(SENDER_POLL_MS);

/// UART1 through UHCI, and its buffers. All static: a packet is too large for a future's frame.
pub struct Bus {
    pub rx: UhciRx<'static, Async>,
    pub tx: UhciTx<'static, Async>,
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
    /// This end's seq, +1 per packet sent.
    seq: u32,
    c6_seq: SeqCheck,
    /// A hello from the C6 waits for its answer.
    hello_owed: bool,
    /// C6 records that did not decode, and how many of them have been reported.
    undecoded: u32,
    undecoded_said: u32,
    /// A record from the down queue that did not fit the last packet. Static, since it
    /// holds up to [`RECORD_MAX`](wartui_proto::radio_link::RECORD_MAX) bytes.
    held: &'static mut Held,
    /// Sends the bundled image while the pair is updating. One per P4 life, as the pair
    /// attempts one update per life.
    sender: Sender<'static>,
    /// The pair has heard of the install.
    install_noted: bool,
}

impl Link {
    pub fn new(
        boot: Instant,
        mac: Mac,
        cause: ResetCause,
        phase: LoopPhase,
        held: &'static mut Held,
    ) -> Self {
        Self {
            boot,
            mac,
            cause,
            phase,
            seq: 0,
            c6_seq: SeqCheck::new(),
            hello_owed: false,
            undecoded: 0,
            undecoded_said: 0,
            held,
            sender: Sender::new(RADIO_IMAGE, RADIO_SHA256, &RADIO_DIGEST),
            install_noted: false,
        }
    }

    fn hello(&self) -> Hello {
        Hello {
            link_rev: LINK_REVISION,
            chip: Chip::Esp32P4,
            mac: self.mac,
            fw_version: ShortStr::try_from(env!("CARGO_PKG_VERSION")).unwrap_or_default(),
            reset_cause: self.cause,
            last_phase: self.phase,
            uptime_ms: self.boot.elapsed().as_millis() as u32,
            // No heap: nothing on the P4 allocates.
            heap_free: 0,
            // The C6 never updates the P4; the host does, over USB (`src/update.rs`).
            image: None,
        }
    }

    /// Act on what the reader made of a packet from the C6.
    fn take(&mut self, packet: Result<Packet<'_>, VerifyError>, shared: &Shared) {
        let packet = match packet {
            Ok(packet) => packet,
            Err(VerifyError::BadMagic) => {
                return shared.count_link(|radio_link| &mut radio_link.bad_magic);
            }
            Err(VerifyError::BadCrc) => {
                return shared.count_link(|radio_link| &mut radio_link.crc_fail);
            }
            // Another core, a bad `len` or an unknown kind, each under a good CRC: no C6
            // built from this tree sends one, and `RadioLinkStats` has no counter for them.
            Err(VerifyError::OtherCore(_) | VerifyError::BadLen | VerifyError::UnknownKind(_)) => {
                return;
            }
        };
        shared.count_link(|radio_link| &mut radio_link.packets);
        shared.pair.borrow_mut().note_heard();
        match self.c6_seq.note(packet.seq) {
            SeqNote::Gap => shared.count_link(|radio_link| &mut radio_link.seq_gap),
            SeqNote::First | SeqNote::InOrder => {}
        }
        match packet.kind {
            Kind::Hello => {
                self.hello_owed = true;
                shared.wake_link.signal(());
                if let Some(hello) = Hello::decode(packet.payload) {
                    let now = crate::now_ms();
                    let event = shared.pair.borrow_mut().on_hello(hello, packet.seq, now);
                    if event == PairEvent::NewLife {
                        shared.new_life();
                    }
                }
            }
            Kind::Messages => {
                shared.pair.borrow_mut().note_seq(packet.seq);
                let pair = shared.pair.borrow();
                let (read, bridging) = (pair.reads_records(), pair.bridges());
                drop(pair);
                if read {
                    for record in records(packet.payload) {
                        self.relay(record, bridging, shared);
                    }
                }
            }
            Kind::Update => {
                shared.pair.borrow_mut().note_seq(packet.seq);
                if let Some(Op::Status(status)) = Op::decode(packet.payload)
                    && updating(shared)
                {
                    self.sender.on_status(&status);
                    shared.wake_link.signal(());
                }
            }
        }
        // Once per change, so a C6 sending garbage costs one line per packet at most.
        if self.undecoded != self.undecoded_said {
            self.undecoded_said = self.undecoded;
            let message = LogStr::try_from("a radio record did not decode").unwrap_or_default();
            shared.push_host(&BridgeToHost::Error { message });
        }
    }

    /// Act on one record from the C6. Unless `bridging`, only its lines and status.
    ///
    /// Out of line: a decoded [`Up`] is over 1 KB.
    #[inline(never)]
    fn relay(&mut self, record: &[u8], bridging: bool, shared: &Shared) {
        match decode_up(record) {
            Ok(Up::Message(msg))
                if bridging
                    || matches!(msg, BridgeToHost::Log { .. } | BridgeToHost::Error { .. }) =>
            {
                shared.push_host(&msg);
            }
            Ok(Up::Message(_)) => {}
            Ok(Up::RadioStatus(status)) => {
                shared.pair.borrow_mut().note_radio_status(status);
                shared.radio_status.signal(());
            }
            Err(_) => self.undecoded = self.undecoded.wrapping_add(1),
        }
    }

    /// Build the next packet to the C6 in `body` and return its length, or `None` when there
    /// is nothing to send.
    fn prepare(&mut self, body: &mut [u8; BODY_MAX], shared: &Shared) -> Option<usize> {
        let payload = &mut body[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX];
        let (kind, len) = self.fill(payload, shared)?;
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        Some(seal(body, kind, seq, len))
    }

    /// Fill a payload: a hello owed, the next update op while updating, else as many queued
    /// records as fit.
    fn fill(&mut self, payload: &mut [u8], shared: &Shared) -> Option<(Kind, usize)> {
        if self.hello_owed {
            self.hello_owed = false;
            return Some((Kind::Hello, self.hello().encode(payload)));
        }
        if updating(shared) {
            let now = crate::now_ms();
            let op = self.sender.next(now, payload);
            self.follow_sender(now, shared);
            return op.map(|len| (Kind::Update, len));
        }
        if !radio_up(shared) {
            return None;
        }
        let mut packer = Packer::new(payload);
        let mut down = shared.down.borrow_mut();
        self.held.pack(&mut packer, |out| down.read(out).0);
        (!packer.is_empty()).then(|| (Kind::Messages, packer.len()))
    }

    /// Tell the pair, and the host, what the sender's last step changed.
    #[inline(never)]
    fn follow_sender(&mut self, now_ms: u64, shared: &Shared) {
        // The C6's `Verified` and the `Install` that follows land in one step, so progress is
        // read in either stage, and 100% is said before installing is.
        if matches!(self.sender.stage(), Stage::Sending | Stage::Installed) {
            let percent = self.sender.percent();
            let said = shared.pair.borrow().update_progress();
            if percent / 10 != said / 10 {
                shared.pair.borrow_mut().note_update_progress(percent);
                shared.push_fault();
            }
        }
        match self.sender.stage() {
            Stage::Sending => {}
            Stage::Installed if !self.install_noted => {
                self.install_noted = true;
                shared.pair.borrow_mut().note_installed(now_ms);
                shared.deadline_set.signal(());
                shared.push_fault();
            }
            Stage::Installed => {}
            Stage::Failed(failure) => {
                let mut text = LogStr::new();
                let _ = match failure {
                    Failure::Refused(state) => {
                        write!(text, "the radio refused the bundled image: {state:?}")
                    }
                    Failure::SwitchFailed(state) => {
                        write!(text, "the radio could not switch to the bundled image: {state:?}")
                    }
                    Failure::Silent => write!(text, "the radio stopped answering its update"),
                    Failure::LostSession => write!(
                        text,
                        "the radio kept dropping the update session; check its partition table"
                    ),
                };
                shared.pair.borrow_mut().on_update_failed();
                shared.push_host(&BridgeToHost::Error { message: text });
                shared.push_fault();
                // A stale C6 bridges, so records queued meanwhile can go now.
                shared.wake_link.signal(());
            }
        }
    }
}

/// Whether radio commands and records flow to and from the C6.
fn radio_up(shared: &Shared) -> bool {
    shared.pair.borrow().bridges()
}

/// Whether the P4 is sending the C6 its bundled image.
fn updating(shared: &Shared) -> bool {
    matches!(shared.pair.borrow().state(), RadioState::Updating(_))
}

/// Run the link forever: the reader and the writer, each on its own.
pub async fn run_link(bus: Bus, link: &RefCell<Link>, shared: &Shared) -> ! {
    let Bus { rx, tx, rx_bufs, tx_buf, reader, body } = bus;
    join(
        read_packets(rx, rx_bufs, reader, link, shared),
        write_packets(tx, tx_buf, body, link, shared),
    )
    .await
    .0
}

/// Read forever: arm the next read, hand the last one's bytes to the link, wait for it to end.
async fn read_packets(
    mut rx: UhciRx<'static, Async>,
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
        // Also what brings the received bytes through the P4's cache, so before `as_slice`.
        last_len = last.number_of_received_bytes();
        // The UART keeps line errors until they are read, so any since the last read are here.
        if result.is_err() || rx.uart_rx.check_for_errors().is_err() {
            shared.count_link(|radio_link| &mut radio_link.uart_errors);
        }
    }
}

/// Write forever: every packet the link has to send, then wait for a reason to look again.
async fn write_packets(
    mut tx: UhciTx<'static, Async>,
    mut buf: DmaTxBuf,
    body: &'static mut [u8; BODY_MAX],
    link: &RefCell<Link>,
    shared: &Shared,
) -> ! {
    loop {
        let prepared = link.borrow_mut().prepare(body, shared);
        let Some(len) = prepared else {
            if updating(shared) {
                select(shared.wake_link.wait(), Timer::after(SENDER_POLL)).await;
            } else {
                shared.wake_link.wait().await;
            }
            continue;
        };
        let wire = encode_packet(&body[..len], buf.as_mut_slice());
        buf.set_length(wire);
        mark(LoopPhase::RadioLink);
        let mut write = tx.write(buf).map_err(|(err, ..)| err).expect("start a UART write");
        Timer::after(FIFO_FILL).await;
        write.wait_for_done().await;
        let (result, tx_back, done) = write.wait();
        tx = tx_back;
        buf = done;
        if result.is_err() {
            shared.count_link(|radio_link| &mut radio_link.uart_errors);
        }
    }
}
