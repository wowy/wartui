//! The P4's end of the SPI link to the C6: what each transfer carries down, and what the
//! P4 does with what comes up.
//!
//! The P4 is master and clocks one transfer per READY edge; `wartui_proto::spi` has the
//! framing and the handshake. The C6 holds READY low for only a few microseconds between
//! transfers: it drops READY when a transfer completes, re-arms at once and raises it
//! again. So the wait for the next edge is armed before the current transfer starts, as
//! the spike's was. `join` polls the edge wait first, and the transfer's completion, the
//! received buffer and the next transfer's preparation all happen while that wait is
//! listening. An edge that comes meanwhile is latched rather than missed. A wait that
//! times out with READY high anyway is an armed C6 whose edge was missed, counted in
//! `ready_recovered`, and the transfer starts. A wait before the C6 first arms is not
//! counted: the C6 is still booting, not missing an edge.
//!
//! The P4 sends its hello while the C6 is sending its own, and stops once the C6 sends
//! anything else. The C6 sends its hello until it hears the P4's, so this covers a C6
//! that restarts under a running P4 as well as the first exchange. The pair hears the seq of
//! every C6 transfer, since a hello whose seq falls below the last is a restart.
//!
//! Records flow only while the pair bridges (`Pair::bridges`): a C6 that encodes them
//! differently is sent nothing but `Idle`, and its records are not read. A C6 updating on
//! this P4's revision has its records read too (`Pair::reads_records`), so its lines about
//! the update reach the host. Only its lines and status are relayed then; the frames it
//! received meanwhile are dropped.
//!
//! While the pair is `Updating`, each transfer carries the next op from a
//! [`Sender`] of the bundled image ([`crate::radio`]), and the C6's statuses come back as
//! [`Kind::Update`] transfers. The sender's progress goes to the host as the pair's fault,
//! every tenth of the image and at 100%, then once more when the C6 is told to install. A
//! sender that fails leaves the pair `Stale` or `Mismatch`; one that installs leaves the pair
//! waiting for the C6's next hello.

use core::fmt::Write;

use embassy_futures::join::join;
use embassy_time::{Duration, Instant, with_timeout};
use esp_hal::Async;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::gpio::Input;
use esp_hal::spi::master::SpiDma;
use wartui_firmware::phase::mark;
use wartui_proto::link::{BridgeToHost, Chip, LogStr, LoopPhase, ShortStr};
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;
use wartui_proto::spi::update::{Failure, Op, Sender, Stage};
use wartui_proto::spi::{
    HEADER_LEN, Held, Hello, Kind, LINK_REVISION, PAYLOAD_MAX, Packer, SeqCheck, SeqNote,
    TRANSFER_LEN, Up, VerifyError, decode_up, records, seal, verify,
};
use wartui_proto::split::{PairEvent, RadioState};

use crate::Shared;
use crate::radio::{RADIO_DIGEST, RADIO_IMAGE, RADIO_SHA256};

/// How long one wait for READY lasts before checking whether its edge was missed.
const READY_TIMEOUT: Duration = Duration::from_millis(100);

/// The SPI master and its READY pin and buffers.
pub struct Bus {
    pub spi: SpiDma<'static, Async>,
    /// Pulled down, so a C6 that is absent or in reset reads as not ready.
    pub ready: Input<'static>,
    pub rx: DmaRxBuf,
    pub tx: DmaTxBuf,
}

/// The link's state.
pub struct Link {
    boot: Instant,
    mac: Mac,
    /// Why this life started, and where the last one stopped, for the hello.
    cause: ResetCause,
    phase: LoopPhase,
    /// This end's seq, +1 per transfer prepared.
    seq: u32,
    c6_seq: SeqCheck,
    /// The C6 is sending its hello, so it has not heard this end's.
    owe_hello: bool,
    /// C6 records that did not decode, and how many of them have been reported.
    undecoded: u32,
    undecoded_said: u32,
    /// A record from the down queue that did not fit the last transfer. Static, since it
    /// holds up to [`RECORD_MAX`](wartui_proto::spi::RECORD_MAX) bytes.
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
            owe_hello: true,
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

    /// Verify a transfer from the C6 and act on it.
    fn take(&mut self, buf: &[u8; TRANSFER_LEN], shared: &Shared) {
        let transfer = match verify(buf) {
            Ok(transfer) => transfer,
            Err(VerifyError::BadMagic) => return shared.count_spi(|spi| &mut spi.bad_magic),
            Err(VerifyError::BadCrc) => return shared.count_spi(|spi| &mut spi.crc_fail),
            // Another core, a bad `len` or an unknown kind, each under a good CRC: no C6
            // built from this tree sends one, and `SpiStats` has no counter for them.
            Err(VerifyError::OtherCore(_) | VerifyError::BadLen | VerifyError::UnknownKind(_)) => {
                return;
            }
        };
        match self.c6_seq.note(transfer.seq) {
            // The C6 sent this transfer again; relaying it twice would deliver twice.
            SeqNote::Repeat => return,
            SeqNote::Gap => shared.count_spi(|spi| &mut spi.seq_gap),
            SeqNote::First | SeqNote::InOrder => {}
        }
        match transfer.kind {
            Kind::Hello => {
                self.owe_hello = true;
                if let Some(hello) = Hello::decode(transfer.payload) {
                    let now = crate::now_ms();
                    let event = shared.pair.borrow_mut().on_hello(hello, transfer.seq, now);
                    if event == PairEvent::NewLife {
                        shared.new_life();
                    }
                }
            }
            Kind::Messages => {
                self.past_hello(transfer.seq, shared);
                let pair = shared.pair.borrow();
                let (read, bridging) = (pair.reads_records(), pair.bridges());
                drop(pair);
                if read {
                    for record in records(transfer.payload) {
                        self.relay(record, bridging, shared);
                    }
                }
            }
            Kind::Idle => self.past_hello(transfer.seq, shared),
            Kind::Update => {
                self.past_hello(transfer.seq, shared);
                if let Some(Op::Status(status)) = Op::decode(transfer.payload)
                    && updating(shared)
                {
                    self.sender.on_status(&status);
                }
            }
        }
        // Once per change, so a C6 sending garbage costs one line per transfer at most.
        if self.undecoded != self.undecoded_said {
            self.undecoded_said = self.undecoded;
            let message = LogStr::try_from("a radio record did not decode").unwrap_or_default();
            shared.push_host(&BridgeToHost::Error { message });
        }
    }

    /// The C6 sent something other than a hello, numbered `seq`, so it has heard this
    /// end's.
    fn past_hello(&mut self, seq: u32, shared: &Shared) {
        self.owe_hello = false;
        shared.pair.borrow_mut().note_seq(seq);
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

    /// Fill a transfer to the C6: this end's hello while the C6 is sending its own, the next
    /// update op while updating, else as many queued records as fit, or nothing.
    fn prepare(&mut self, buf: &mut [u8; TRANSFER_LEN], shared: &Shared) {
        let seq = self.seq;
        self.seq = self.seq.wrapping_add(1);
        if self.owe_hello {
            let len = self.hello().encode(&mut buf[HEADER_LEN..]);
            seal(buf, Kind::Hello, seq, len);
            return;
        }
        if updating(shared) {
            let now = crate::now_ms();
            let op = self.sender.next(now, &mut buf[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX]);
            self.follow_sender(now, shared);
            match op {
                Some(len) => seal(buf, Kind::Update, seq, len),
                None => seal(buf, Kind::Idle, seq, 0),
            }
            return;
        }
        if !radio_up(shared) {
            seal(buf, Kind::Idle, seq, 0);
            return;
        }
        let mut packer = Packer::new(&mut buf[HEADER_LEN..HEADER_LEN + PAYLOAD_MAX]);
        let mut down = shared.down.borrow_mut();
        self.held.pack(&mut packer, |out| down.read(out).0);
        drop(down);
        let len = packer.len();
        seal(buf, if len == 0 { Kind::Idle } else { Kind::Messages }, seq, len);
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

/// Run the link forever: one transfer per READY edge, each verified and handled, and the
/// next prepared, while the wait for the following edge is already armed.
pub async fn run_link(bus: Bus, link: &mut Link, shared: &Shared) -> ! {
    let Bus { mut spi, mut ready, mut rx, mut tx } = bus;
    link.prepare(tx_bytes(&mut tx), shared);
    while with_timeout(READY_TIMEOUT, ready.wait_for_rising_edge()).await.is_err() && ready.is_low()
    {
    }
    // The C6 is armed: its READY rose, or was found high after a timeout.
    let mut armed = true;
    loop {
        if !armed {
            armed = with_timeout(READY_TIMEOUT, ready.wait_for_rising_edge()).await.is_ok()
                || timed_out_high(&ready, shared);
            continue;
        }
        mark(LoopPhase::SpiTransfer);
        // `join` polls the edge wait first, so it is listening before the transfer starts.
        let (edge, back) = join(with_timeout(READY_TIMEOUT, ready.wait_for_rising_edge()), async {
            match spi.transfer_buffers(TRANSFER_LEN, rx, TRANSFER_LEN, tx) {
                Ok(mut transfer) => {
                    transfer.wait_for_done().await;
                    let (spi, (rx, mut tx)) = transfer.wait();
                    shared.count_spi(|spi| &mut spi.transfers);
                    link.take(rx_bytes(&rx), shared);
                    link.prepare(tx_bytes(&mut tx), shared);
                    (spi, rx, tx)
                }
                // Nothing was clocked, so `tx` still holds the transfer to send.
                Err((_, spi, rx, tx)) => (spi, rx, tx),
            }
        })
        .await;
        (spi, rx, tx) = back;
        armed = edge.is_ok() || timed_out_high(&ready, shared);
    }
}

/// Counts a READY wait that timed out, and says whether READY is high anyway: an armed
/// C6 whose edge was missed.
fn timed_out_high(ready: &Input<'_>, shared: &Shared) -> bool {
    shared.count_spi(|spi| &mut spi.ready_timeouts);
    let high = ready.is_high();
    if high {
        shared.count_spi(|spi| &mut spi.ready_recovered);
    }
    high
}

fn rx_bytes(buf: &DmaRxBuf) -> &[u8; TRANSFER_LEN] {
    buf.as_slice().first_chunk().expect("a transfer-sized DMA buffer")
}

fn tx_bytes(buf: &mut DmaTxBuf) -> &mut [u8; TRANSFER_LEN] {
    buf.as_mut_slice().first_chunk_mut().expect("a transfer-sized DMA buffer")
}
