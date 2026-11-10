//! The host link: wartui's link protocol over UART0, which the board's CH343 carries to the
//! host's USB.
//!
//! UART0 runs at [`HOST_BAUD`], 8N1, on GPIO37 (TX) and GPIO38 (RX), through UHCI's DMA and
//! with no flow control: the CH343's modem lines drive the P4's EN and GPIO35, so neither can
//! carry it. The P4 has one UHCI, and it goes to the link without flow control
//! (`src/link.rs` has the C6's side). `docs/p4-host-uart-findings.md` has what the link was
//! measured to carry.
//!
//! **A read ends at [`RX_LEN`] bytes or when the line goes idle**, and the next read is armed
//! before this one's bytes are looked at, as `firmware/p4-radio` reads its end of the C6 link.
//! Two buffers keep DMA landing while the P4 works; only the moment between two reads leans on
//! the 128-byte FIFO.
//!
//! **A write packs queued frames into one DMA buffer of up to [`HOST_CHUNK`] bytes, and is
//! polled to its end.** `wait_for_done` and `wait` both flush the UART, and the flush spins
//! without yielding until the FIFO is empty, which stops this whole task. `is_done` goes high
//! once the DMA has handed over the last byte, and `cancel` stops only the DMA channel, so the
//! FIFO's last bytes go out behind it and the next write lands after them.
//!
//! **Host→P4 bytes are lost while the P4 erases or writes its own flash.** Interrupts stay off
//! for about 40 ms, the armed buffer lasts about 10 ms, and then the FIFO overflows. The link
//! needs no flow control for it. An update of the P4's own image is the flash work a host causes,
//! and `wartui_proto::radio_link::update::Sender` waits for each sector's status and rewinds on
//! `next_offset` and on silence. A `SetFan` or `SetPool` for a value the sector does not hold
//! writes one word, and one in 1024 erases a sector first, which can lose the host's next frames
//! (`src/settings.rs`).
//! `Status.host_uart_errors` counts the reads that saw one or more line errors or overflows, not
//! the errors themselves.
//!
//! **The gate opens on any host frame that decodes and closes after [`HOST_SILENCE`] without
//! one.** The P4 cannot see the CH343's DTR, so a frame is the only sign that someone reads.
//! Opening clears both host queues, so a host never reads a backlog from before it attached,
//! and announces this life. While the gate is closed nothing queues (`Shared::push_host`), and
//! closing drops any update session and the cached node image (`update::host_gone`,
//! `cache::host_gone`). The engine polls every 5 s, so three
//! polls go missing before the gate closes on a running host. The fleet planner runs whatever
//! the gate does: its reports are dropped while it is closed, and the announcement that opens it
//! carries the current plan.
//!
//! **The bootloader prints on UART0 at every boot**, at 115200 for about 250 ms, so a host
//! reads garbage after every reset. The host's framing resynchronizes on the next `0x00`.
//!
//! **A host frame confirms the P4's image on trial once the C6 has said hello**, and
//! `HostToBridge::Update` frames carry its next one; `src/update.rs` has both.

use core::cell::Cell;

use embassy_futures::join::join4;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use esp_hal::Async;
use esp_hal::dma::{DmaRxBuf, DmaTxBuf};
use esp_hal::uart::uhci::{UhciRx, UhciTx};
use wartui_firmware::phase::mark;
use wartui_firmware::reboot;
use wartui_proto::host_queue::HOST_CHUNK;
use wartui_proto::link::{
    FrameAccumulator, HOST_BAUD, HostToBridge, LoopPhase, MAX_HOST_FRAME, decode_frame,
};
use wartui_proto::radio_link::Down;

use crate::Shared;

/// Bytes in each receive DMA buffer. The hardware's limit is 4095; this is a multiple of the
/// P4's 64-byte cache line, and lasts about 10 ms at [`HOST_BAUD`].
pub const RX_LEN: usize = 4032;

/// How long the gate stays open without a host frame: three of the engine's 5 s status polls.
pub const HOST_SILENCE: Duration = Duration::from_secs(15);

/// How long a `GetStatus` waits for the C6's counters before answering without fresh ones.
const STATUS_WAIT: Duration = Duration::from_millis(50);

/// How often a write looks for its end once its wire time has passed.
const WRITE_POLL: Duration = Duration::from_micros(100);

/// UART0 through UHCI, and its buffers. All static: a chunk is too large for a future's frame.
pub struct Bus {
    pub rx: UhciRx<'static, Async>,
    pub tx: UhciTx<'static, Async>,
    pub rx_bufs: [DmaRxBuf; 2],
    pub tx_buf: DmaTxBuf,
}

/// The gate: whether a host is present, and when it last sent a frame.
pub struct Presence {
    present: Cell<bool>,
    heard: Cell<Instant>,
}

impl Presence {
    pub const fn new() -> Self {
        Self { present: Cell::new(false), heard: Cell::new(Instant::from_ticks(0)) }
    }

    /// Whether a host has sent a frame within [`HOST_SILENCE`].
    pub fn is_open(&self) -> bool {
        self.present.get()
    }
}

/// Serve the host forever: the reader, the writer, the gate and the `GetStatus` answer.
pub async fn run(bus: Bus, shared: &Shared) -> ! {
    let Bus { rx, tx, rx_bufs, tx_buf } = bus;
    join4(
        read_host(rx, rx_bufs, shared),
        write_host(tx, tx_buf, shared),
        close_on_silence(shared),
        answer_status(shared),
    )
    .await
    .0
}

/// Read forever: arm the next read, act on the last one's frames, wait for it to end.
async fn read_host(
    mut rx: UhciRx<'static, Async>,
    [mut armed, mut last]: [DmaRxBuf; 2],
    shared: &Shared,
) -> ! {
    let mut frames = FrameAccumulator::<MAX_HOST_FRAME>::new();
    let mut last_len = 0;
    loop {
        let mut read = rx.read(armed).map_err(|(err, ..)| err).expect("arm a UART read");
        mark(LoopPhase::DrainLink);
        for &byte in &last.as_slice()[..last_len] {
            if let Some(frame) = frames.push(byte) {
                take_frame(frame, shared);
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
            shared.host_uart_errors.set(shared.host_uart_errors.get().wrapping_add(1));
        }
    }
}

/// Decode one host frame, open the gate, and carry out the command: on the P4, by queueing
/// a record for a node, by passing a power-cycle to the carrier, or by telling the fleet planner,
/// which sends what it owes before the next frame is read (`src/fleet.rs`).
///
/// Out of line: a decoded command is over 1 KB.
#[inline(never)]
fn take_frame(frame: &mut [u8], shared: &Shared) {
    // Matched in place rather than moved out of the `Option`, which would copy it.
    let decoded = decode(frame);
    let Some(cmd) = &decoded else { return };
    let opened = open(shared);
    let confirmed = crate::update::note_host_frame(shared);
    // An `Identify` announces below, and the announcement carries the confirm's line. Any other
    // frame announces a host's arrival here, and says the line to a host already announced to.
    if !matches!(cmd, HostToBridge::Identify) {
        if opened {
            shared.announce();
        } else if confirmed {
            shared.flush_update_lines();
        }
    }
    mark(LoopPhase::Command);
    // Listed rather than caught, so a new command is a compile error until it is placed.
    match cmd {
        HostToBridge::Identify => shared.announce(),
        HostToBridge::GetStatus => shared.status_asked.signal(()),
        HostToBridge::Reset => reboot(),
        HostToBridge::GpsWrite { bytes } => crate::gps::write(shared, bytes),
        HostToBridge::Update(op) => crate::update::take(op, shared),
        HostToBridge::ToNode { slot, id, kind, body } => {
            crate::bus::to_node(shared, *slot, *id, *kind, body);
        }
        HostToBridge::PowerCycle { slot } => crate::carrier::request(shared, *slot),
        HostToBridge::CacheImage(op) => crate::cache::take(op, shared),
        HostToBridge::ToNodeChunk { slot, id, image, session, offset, len } => {
            crate::cache::to_node_chunk(shared, *slot, *id, *image, *session, *offset, *len);
        }
        HostToBridge::SetFan { duty } => crate::fan::set(shared, *duty),
        HostToBridge::SetPool { pool } => crate::settings::set_pool(shared, *pool),
        HostToBridge::StartScan { bluetooth } => crate::fleet::start(shared, *bluetooth),
        HostToBridge::StopScan => crate::fleet::stop(shared),
    }
}

/// Note a host frame, and open the gate if it was closed. Returns whether it opened.
fn open(shared: &Shared) -> bool {
    let presence = &shared.presence;
    presence.heard.set(Instant::now());
    if presence.present.get() {
        return false;
    }
    // A new host: whatever is queued was for one that has gone.
    shared.host.borrow_mut().clear();
    presence.present.set(true);
    shared.host_arrived.signal(());
    true
}

#[inline(never)]
fn decode(frame: &mut [u8]) -> Option<HostToBridge> {
    decode_frame::<HostToBridge>(frame).ok()
}

/// Write queued frames to the host forever, a chunk at a time.
async fn write_host(mut tx: UhciTx<'static, Async>, mut buf: DmaTxBuf, shared: &Shared) -> ! {
    loop {
        buf.set_length(HOST_CHUNK);
        let chunk: &mut [u8; HOST_CHUNK] =
            buf.as_mut_slice().try_into().expect("the buffer holds a chunk");
        let n = shared.host.borrow_mut().next_chunk(chunk);
        if n == 0 {
            shared.pushed.wait().await;
            continue;
        }
        buf.set_length(n);
        mark(LoopPhase::Pump);
        let write = tx.write(buf).map_err(|(err, ..)| err).expect("start a UART write");
        // Ten bits a byte: 8N1. No flow control, so the write ends close to its wire time.
        Timer::after(Duration::from_micros(n as u64 * 10 * 1_000_000 / HOST_BAUD as u64)).await;
        while !write.is_done() {
            Timer::after(WRITE_POLL).await;
        }
        (tx, buf) = write.cancel();
    }
}

/// Close the gate once [`HOST_SILENCE`] passes without a host frame.
async fn close_on_silence(shared: &Shared) -> ! {
    let presence = &shared.presence;
    loop {
        if !presence.present.get() {
            shared.host_arrived.wait().await;
            continue;
        }
        let deadline = presence.heard.get() + HOST_SILENCE;
        if Instant::now() < deadline {
            // A frame that arrives meanwhile moves the deadline, which the next pass reads.
            Timer::at(deadline).await;
            continue;
        }
        presence.present.set(false);
        crate::update::host_gone(shared);
        crate::cache::host_gone(shared);
    }
}

/// Answer each `GetStatus` with the C6's counters as fresh as [`STATUS_WAIT`] allows.
async fn answer_status(shared: &Shared) -> ! {
    loop {
        shared.status_asked.wait().await;
        shared.pair.borrow_mut().note_asked(crate::now_ms());
        shared.deadline_set.signal(());
        shared.radio_status.reset();
        let up = shared.pair.borrow().bridges();
        if up && shared.push_down(&Down::GetRadioStatus) {
            let _ = with_timeout(STATUS_WAIT, shared.radio_status.wait()).await;
        }
        shared.push_status();
    }
}
