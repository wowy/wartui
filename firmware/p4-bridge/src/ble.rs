//! The host link over Bluetooth, through the C6: who owns the host queue, and the credit window
//! that paces the host stream down the C6 link.
//!
//! **One host owns the host queue**, and `wartui_proto::host_owner::Owner` decides which, so its
//! rules are tested with `cargo test`. Its inputs are a central's subscription, as the C6's
//! `BleState` reports it, and UART0's frames and silence (`src/host.rs`); [`own`] carries out
//! what it returns. A central counts as subscribed only while its link is encrypted and it holds a
//! stored bond. Bluetooth lets go on an unsubscribe, a disconnect or a C6 restart, and never on
//! silence, since a subscription is the sign that someone reads. While it owns the queue the UART0
//! writer sends nothing from it.
//!
//! **The stream's offsets never rewind.** `sent` is the wrapping offset of the next stream byte.
//! Each attachment starts with a `Down::HostStart` at `sent`, and each `Down::HostBytes` carries
//! the offset of its first byte, so the C6 (`wartui_proto::radio_link::Inflow`) credits bytes lost
//! on the link and drops an earlier attachment's. Both counters restart at 0 with a new C6 life,
//! since a fresh C6 counts its credit from 0 and adopts the first offset it sees.
//!
//! **The stream is credited.** The link's writer packs `Down::HostBytes` only while
//! `sent.wrapping_sub(accounted)` stays under `HOST_WINDOW_BYTES`, where `accounted` is the C6's
//! latest `Up::Credit`. A slow central backs the stream up in the host queue, where the bulk ring
//! evicts, and never holds the C6 link behind RTS/CTS.
//!
//! **A central that loses the queue to USB is told why**: one `BridgeToHost::Error` goes down its
//! stream ahead of anything else, from its own buffer, after the queue has moved. It waits for
//! credit at most [`NOTICE_WAIT`], so a dead central holds nothing.
//!
//! Frames a central writes arrive as `Up::HostBytes`, through their own accumulator, to the decode
//! path UART0's reads take (`host::take_frame`), and only while Bluetooth owns the queue.

use core::cell::{Cell, RefCell};

use embassy_time::{Duration, Instant};
use wartui_proto::host_owner::{Effect, Effects, Input};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostOwner, LogStr, MAX_HOST_FRAME, encode_frame,
};
use wartui_proto::radio_link::{
    BleState, Down, HOST_BYTES_MAX, HOST_WINDOW_BYTES, Packer, encode_down, host_bytes_fit,
};

use crate::Shared;
use crate::host::Source;

/// Host frames a central writes, reassembled from `Up::HostBytes`.
pub type BleFrames = FrameAccumulator<MAX_HOST_FRAME>;

/// What a central that loses the queue to USB is told. The host transport reads it as the reason
/// its link ended.
const NOTICE: &str = "the USB host took the bridge";

/// Bytes the notice's frame takes, with the `0x00` ahead of it.
const NOTICE_MAX: usize = 64;

/// How long the notice waits for the window to take it before it is dropped.
const NOTICE_WAIT: Duration = Duration::from_secs(1);

/// The notice's frame, and how much of it has gone.
struct Notice {
    buf: [u8; NOTICE_MAX],
    len: usize,
    sent: usize,
    until: Instant,
}

impl Notice {
    const fn new() -> Self {
        Self { buf: [0; NOTICE_MAX], len: 0, sent: 0, until: Instant::from_ticks(0) }
    }

    /// The bytes still to send; empty with no notice owed.
    fn rest(&self) -> &[u8] {
        &self.buf[self.sent..self.len]
    }

    fn drop_it(&mut self) {
        self.len = 0;
        self.sent = 0;
    }
}

/// The Bluetooth host's state, as the link and the host futures see it.
pub struct Ble {
    /// The C6's latest `BleState`.
    state: Cell<BleState>,
    /// The wrapping stream offset of the next byte sent.
    sent: Cell<u32>,
    /// The C6's latest `Up::Credit`: stream bytes it is done with.
    accounted: Cell<u32>,
    notice: RefCell<Notice>,
    /// Static, since it holds a whole host frame.
    frames: RefCell<&'static mut BleFrames>,
}

impl Ble {
    pub fn new(frames: &'static mut BleFrames) -> Self {
        Self {
            state: Cell::new(BleState::default()),
            sent: Cell::new(0),
            accounted: Cell::new(0),
            notice: RefCell::new(Notice::new()),
            frames: RefCell::new(frames),
        }
    }

    /// The C6's latest `BleState`.
    pub fn state(&self) -> BleState {
        self.state.get()
    }

    /// Stream bytes the window still allows: none while more than the window is in flight.
    fn window(&self) -> usize {
        let in_flight = self.sent.get().wrapping_sub(self.accounted.get());
        HOST_WINDOW_BYTES.saturating_sub(in_flight) as usize
    }
}

/// Whether `state` is a central that may hold the host queue.
const fn authorized(state: BleState) -> bool {
    state.subscribed && state.encrypted && state.bonded
}

/// Feed `input` to the owner, and carry out what it returns, in order. Returns the effects, so a
/// UART0 frame's caller knows whether it opened the gate or was answered already.
pub fn own(shared: &Shared, input: Input) -> Effects {
    let mut owner = shared.owner.get();
    let effects = owner.on(input);
    // Set first, so what the effects queue goes to the new owner.
    shared.owner.set(owner);
    for effect in &effects {
        match effect {
            Effect::AttachBluetooth => take(shared),
            Effect::AttachUsb => crate::host::open(shared),
            Effect::ReleaseBluetooth => release(shared),
            Effect::ReleaseUsb => crate::host::close(shared),
            Effect::NoticeCentral => notice(shared),
            Effect::ReadyOnUart => crate::host::owe_ready(shared),
        }
    }
    effects
}

/// The C6's Bluetooth link changed.
pub fn on_state(shared: &Shared, state: BleState) {
    let was = authorized(shared.ble.state.replace(state));
    match (was, authorized(state)) {
        (false, true) => {
            let _ = own(shared, Input::Subscribed);
        }
        (true, false) => {
            let _ = own(shared, Input::Unsubscribed);
        }
        _ => {}
    }
}

/// The C6 is done with the stream up to `accounted`.
pub fn on_credit(shared: &Shared, accounted: u32) {
    shared.ble.accounted.set(accounted);
    shared.wake_link.signal(());
}

/// A new C6 life: its central is gone, and it counts the stream from 0.
pub fn on_new_life(shared: &Shared) {
    let ble = &shared.ble;
    if authorized(ble.state.replace(BleState::default())) {
        let _ = own(shared, Input::Unsubscribed);
    }
    ble.notice.borrow_mut().drop_it();
    ble.sent.set(0);
    ble.accounted.set(0);
}

/// Make Bluetooth the host: mark where its stream starts, clear the queue, and announce.
fn take(shared: &Shared) {
    let ble = &shared.ble;
    // Owed to an earlier attachment, not this one.
    ble.notice.borrow_mut().drop_it();
    ble.frames.borrow_mut().reset();
    // Ahead of this attachment's bytes: the link's writer packs the down queue's records before
    // any host bytes, and none while a record is held (`Link::fill`). A full queue refuses it and
    // counts the refusal; the C6 then reads the bytes as following on from the last.
    let _ = shared.push_down(&Down::HostStart { at: ble.sent.get() });
    shared.host_attached();
    shared.announce();
    shared.wake_link.signal(());
}

/// Stop serving the central: clear what was queued for it, and drop its sessions.
fn release(shared: &Shared) {
    shared.host.borrow_mut().clear();
    crate::update::host_gone(shared);
    crate::cache::host_gone(shared);
}

/// Owe the central the notice that USB took the queue. Out of line, for the encode buffer.
#[inline(never)]
fn notice(shared: &Shared) {
    let message = LogStr::try_from(NOTICE).unwrap_or_default();
    let mut notice = shared.ble.notice.borrow_mut();
    // The `0x00` ends a frame the queue cut part-way down the stream.
    notice.buf[0] = 0x00;
    let Ok(len) = encode_frame(&BridgeToHost::Error { message }, &mut notice.buf[1..]) else {
        return;
    };
    notice.len = 1 + len;
    notice.sent = 0;
    notice.until = Instant::now() + NOTICE_WAIT;
    drop(notice);
    shared.wake_link.signal(());
}

/// Take the bytes of the `HostToBridge` stream a central wrote. Dropped unless Bluetooth owns
/// the queue.
pub fn take_bytes(shared: &Shared, bytes: &[u8]) {
    if shared.owner.get().owner() != HostOwner::Bluetooth {
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
/// the window allow: the notice first, while one is owed, then the queue's, while Bluetooth owns
/// it. `scratch` holds them on the way.
pub fn pack(shared: &Shared, packer: &mut Packer<'_>, scratch: &mut [u8; HOST_BYTES_MAX]) {
    let ble = &shared.ble;
    let room = |packer: &Packer<'_>| host_bytes_fit(packer.room()).min(ble.window());
    {
        let mut notice = ble.notice.borrow_mut();
        if !notice.rest().is_empty() && Instant::now() >= notice.until {
            notice.drop_it();
        }
        let rest = notice.rest();
        if !rest.is_empty() {
            let n = rest.len().min(room(packer));
            scratch[..n].copy_from_slice(&rest[..n]);
            if n > 0 && push_bytes(ble, packer, &scratch[..n]) {
                notice.sent += n;
            }
            // Nothing goes ahead of it.
            if !notice.rest().is_empty() {
                return;
            }
            notice.drop_it();
        }
    }
    if shared.owner.get().owner() != HostOwner::Bluetooth {
        return;
    }
    let max = room(packer).min(HOST_BYTES_MAX);
    if max == 0 {
        return;
    }
    let n = shared.host.borrow_mut().fill(&mut scratch[..max]);
    if n > 0 {
        push_bytes(ble, packer, &scratch[..n]);
    }
}

/// Pack `bytes` as the `Down::HostBytes` at `sent`, and advance it. `host_bytes_fit` sized them
/// to the room, so they always pack.
fn push_bytes(ble: &Ble, packer: &mut Packer<'_>, bytes: &[u8]) -> bool {
    let at = ble.sent.get();
    let packed = packer.push_with(|out| encode_down(&Down::HostBytes { at, bytes }, out).ok());
    if packed {
        ble.sent.set(at.wrapping_add(bytes.len() as u32));
    }
    packed
}
