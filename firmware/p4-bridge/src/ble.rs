//! The host link over Bluetooth, through the C6: who owns the host queue, and the credit window
//! that paces the host stream down the C6 link.
//!
//! **One host owns the host queue.** A C6 `BleState` that turns subscribed makes Bluetooth the
//! host: UART0's gate closes, the rings are cleared and the life announced, as a UART0 host's
//! first frame does. The Bluetooth gate closes on an unsubscribe, a disconnect or a C6 restart, and
//! never on silence, since a subscription is the sign that someone reads. While it is open the
//! UART0 writer sends nothing from the queue (`src/host.rs`).
//!
//! **A UART0 frame takes the queue back**, subscriber or not, so `wartui flash-bridge` always
//! gets in; with no subscriber UART0 runs exactly as it does without Bluetooth. Once UART0's gate
//! closes on silence, a central still subscribed takes the queue again.
//!
//! **The stream is credited** (`wartui_proto::radio_link`, `Up::BleCredit`). The link's writer
//! packs `Down::HostBytes` only while the bytes sent less the bytes credited stay under
//! [`BLE_WINDOW_BYTES`], so a slow central backs the stream up in the host queue, where the bulk
//! ring evicts, and never holds the C6 link behind RTS/CTS. A credit ahead of what was sent, from
//! bytes the C6 took after the window was reset, reads as nothing in flight.
//!
//! Frames a central writes arrive as `Up::HostBytes`, through their own accumulator, to the decode
//! path UART0's reads take (`host::take_frame`).

use core::cell::{Cell, RefCell};

use embassy_time::Instant;
use wartui_proto::link::{FrameAccumulator, MAX_HOST_FRAME};
use wartui_proto::radio_link::{
    BleState, BleStats, Down, HOST_BYTES_MAX, Packer, encode_down, host_bytes_fit,
};

use crate::Shared;
use crate::config::BLE_WINDOW_BYTES;
use crate::host::Source;

/// Host frames a central writes, reassembled from `Up::HostBytes`.
pub type BleFrames = FrameAccumulator<MAX_HOST_FRAME>;

/// The Bluetooth host's state, as the link and the host futures see it.
pub struct Ble {
    /// The C6's latest `BleState`.
    state: Cell<BleState>,
    /// The C6's latest `BleStats`.
    stats: Cell<BleStats>,
    /// Bluetooth owns the host queue.
    owns: Cell<bool>,
    /// Milliseconds of ownership before the current stretch, and when that stretch began.
    owned_ms: Cell<u64>,
    owned_since: Cell<Instant>,
    /// Stream bytes sent, wrapping, against [`Ble::credited`].
    sent: Cell<u32>,
    /// The C6's latest `drained`.
    credited: Cell<u32>,
    /// Stream bytes sent and credited since boot, for the spike's counters.
    sent_total: Cell<u32>,
    credited_total: Cell<u32>,
    /// Static, since it holds a whole host frame.
    frames: RefCell<&'static mut BleFrames>,
}

impl Ble {
    pub fn new(frames: &'static mut BleFrames) -> Self {
        Self {
            state: Cell::new(BleState::default()),
            stats: Cell::new(BleStats::default()),
            owns: Cell::new(false),
            owned_ms: Cell::new(0),
            owned_since: Cell::new(Instant::from_ticks(0)),
            sent: Cell::new(0),
            credited: Cell::new(0),
            sent_total: Cell::new(0),
            credited_total: Cell::new(0),
            frames: RefCell::new(frames),
        }
    }

    /// Whether Bluetooth owns the host queue.
    pub fn owns(&self) -> bool {
        self.owns.get()
    }

    /// Milliseconds Bluetooth has owned the queue since boot, the current stretch included.
    pub fn owned_ms(&self) -> u64 {
        let current =
            if self.owns.get() { self.owned_since.get().elapsed().as_millis() } else { 0 };
        self.owned_ms.get() + current
    }

    /// Set whether Bluetooth owns the queue, timing each stretch, and return whether it did.
    fn set_owns(&self, owns: bool) -> bool {
        let was = self.owns.replace(owns);
        match (was, owns) {
            (false, true) => self.owned_since.set(Instant::now()),
            (true, false) => {
                let stretch = self.owned_since.get().elapsed().as_millis();
                self.owned_ms.set(self.owned_ms.get() + stretch);
            }
            _ => {}
        }
        was
    }

    /// The C6's latest `BleState`.
    pub fn state(&self) -> BleState {
        self.state.get()
    }

    /// The C6's latest `BleStats`.
    pub fn stats(&self) -> BleStats {
        self.stats.get()
    }

    /// Stream bytes sent to the C6 and credited by it since boot, wrapping.
    pub fn totals(&self) -> (u32, u32) {
        (self.sent_total.get(), self.credited_total.get())
    }

    /// Stream bytes the window still allows.
    fn window(&self) -> usize {
        let in_flight = self.sent.get().wrapping_sub(self.credited.get());
        let in_flight = if in_flight > u32::MAX / 2 { 0 } else { in_flight as usize };
        BLE_WINDOW_BYTES.saturating_sub(in_flight)
    }
}

/// The C6's Bluetooth link changed.
pub fn on_state(shared: &Shared, state: BleState) {
    let was = shared.ble.state.replace(state).subscribed;
    match (was, state.subscribed) {
        (false, true) => take(shared),
        (true, false) => release(shared),
        _ => {}
    }
}

/// The C6 took more of the stream.
pub fn on_credit(shared: &Shared, drained: u32) {
    let ble = &shared.ble;
    let credit = drained.wrapping_sub(ble.credited.replace(drained));
    ble.credited_total.set(ble.credited_total.get().wrapping_add(credit));
    shared.wake_link.signal(());
}

/// The C6's Bluetooth counters arrived.
pub fn on_stats(shared: &Shared, stats: BleStats) {
    shared.ble.stats.set(stats);
}

/// A new C6 life: its subscriber and its count of drained bytes are gone.
pub fn on_new_life(shared: &Shared) {
    let ble = &shared.ble;
    ble.state.set(BleState::default());
    release(shared);
    ble.sent.set(0);
    ble.credited.set(0);
}

/// UART0's gate opened: the queue is its own now. The central stays subscribed, for
/// [`uart_gone`].
pub fn uart_took(shared: &Shared) {
    shared.ble.set_owns(false);
    crate::spike::on_bluetooth(shared, false);
}

/// UART0's gate closed on silence: a central still subscribed takes the queue again.
pub fn uart_gone(shared: &Shared) {
    let ble = &shared.ble;
    if ble.state.get().subscribed && !ble.owns.get() {
        take(shared);
    }
}

/// Make Bluetooth the host: close UART0's gate, clear the queue for the new host, and announce.
fn take(shared: &Shared) {
    crate::host::close(shared);
    let ble = &shared.ble;
    ble.set_owns(true);
    // Nothing is in flight to the new host.
    ble.sent.set(ble.credited.get());
    ble.frames.borrow_mut().reset();
    shared.host_attached();
    shared.announce();
    crate::spike::on_bluetooth(shared, true);
    shared.wake_link.signal(());
}

/// Close the Bluetooth gate, if it is open, and drop what was queued for the central.
fn release(shared: &Shared) {
    if !shared.ble.set_owns(false) {
        return;
    }
    crate::spike::on_bluetooth(shared, false);
    shared.host.borrow_mut().clear();
    crate::update::host_gone(shared);
    crate::cache::host_gone(shared);
}

/// Take the bytes of the `HostToBridge` stream a central wrote. Dropped unless Bluetooth owns
/// the queue.
pub fn take_bytes(shared: &Shared, bytes: &[u8]) {
    if !shared.ble.owns.get() {
        return;
    }
    let mut frames = shared.ble.frames.borrow_mut();
    for &byte in bytes {
        if let Some(frame) = frames.push(byte) {
            crate::host::take_frame(frame, Source::Bluetooth, shared);
        }
    }
}

/// Pack the host stream's next bytes into the packet `packer` is filling, as far as its room and
/// the window allow. `scratch` holds them on the way.
pub fn pack(shared: &Shared, packer: &mut Packer<'_>, scratch: &mut [u8; HOST_BYTES_MAX]) {
    let ble = &shared.ble;
    if !ble.owns.get() {
        return;
    }
    let max = host_bytes_fit(packer.room()).min(ble.window()).min(HOST_BYTES_MAX);
    if max == 0 {
        return;
    }
    let n = shared.host.borrow_mut().fill(&mut scratch[..max]);
    if n == 0 {
        return;
    }
    // `host_bytes_fit` sized the bytes to the room, so they always pack.
    let packed = packer.push_with(|out| encode_down(&Down::HostBytes(&scratch[..n]), out).ok());
    if packed {
        ble.sent.set(ble.sent.get().wrapping_add(n as u32));
        ble.sent_total.set(ble.sent_total.get().wrapping_add(n as u32));
    }
}
