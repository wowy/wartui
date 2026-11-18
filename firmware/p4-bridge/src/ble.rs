//! The host link over Bluetooth, through the C6: who owns the host queue, and the credit window
//! that paces the host stream down the C6 link.
//!
//! **One host owns the host queue**, and `wartui_proto::host_owner::Owner` decides which, so its
//! rules are tested with `cargo test`. Its inputs are a central's or a phone's subscription, as
//! the C6's `BleState` reports it, and UART0's frames and silence (`src/host.rs`); [`own`] carries
//! out what it returns. A central counts as subscribed only while it reads the host stream
//! (`Stream::Host`) on an encrypted link and holds a stored bond. A phone counts as subscribed
//! while it reads the WardriveESP stream (`Stream::Phone`) and the setting is on
//! (`src/wardrive_esp.rs`). The P4 keeps which of the two the last state was, so a change of the
//! setting reads as the phone letting go. Either lets go on an unsubscribe, a disconnect or a C6
//! restart, and never on silence, since a subscription is the sign that someone reads. While
//! either holds the queue the UART0 writer sends nothing from it, and the stream goes down the C6
//! link. After each input, a change in whether the C6 may admit a phone goes down in
//! `Down::Bonds` (`src/bonds.rs`).
//!
//! **The stream's offsets never rewind**, across C6 lives too. `sent` is the wrapping offset of the
//! next stream byte. Each attachment starts with a `Down::HostStart` at `sent`, and each
//! `Down::HostBytes` carries the offset of its first byte, so the C6
//! (`wartui_proto::radio_link::Inflow`) adopts the P4's offsets, moves past bytes lost on the link
//! and drops an earlier attachment's.
//!
//! **The stream is credited by position.** `accounted` is the C6's latest `Up::Credit`: the offset,
//! in the P4's offsets, up to which the C6 is done. The C6 repeats it every second while a central
//! is subscribed, so a credit lost on the link never leaves the window shut, and a repeat sets the
//! position the P4 already holds. The link's writer packs `Down::HostBytes` only
//! inside `host_window(sent, accounted)`, so a slow central backs the stream up in the host queue,
//! where the bulk ring evicts, and never holds the C6 link behind RTS/CTS. A new C6 life sets
//! `accounted` to `sent`: the bytes in flight died with the old C6, and the new one credits in the
//! same offsets from the first it sees. More than a window in flight means the two disagree, and
//! `host_window` opens the window, since only a send resynchronizes them.
//!
//! **A central that loses the queue to USB is told why**: one `BridgeToHost::Error` goes down its
//! stream ahead of anything else, from its own buffer, after the queue has moved. It waits for
//! credit at most [`NOTICE_WAIT`], so a dead central holds nothing. A phone is told nothing: the
//! notice is a COBS frame, and each attachment drops one still owed.
//!
//! Frames a central writes arrive as `Up::HostBytes`, through their own accumulator, to the decode
//! path UART0's reads take (`host::take_frame`), and only while Bluetooth owns the queue.

use core::cell::{Cell, RefCell};

use embassy_time::{Duration, Instant};
use wartui_proto::host_owner::{Effect, Effects, Holder, Input};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostOwner, LogStr, MAX_HOST_FRAME, USB_TOOK_BRIDGE,
    encode_frame,
};
use wartui_proto::radio_link::{
    BleState, Down, HOST_BYTES_MAX, Packer, Stream, encode_down, host_bytes_fit, host_window,
};

use crate::Shared;
use crate::host::Source;

/// Host frames a central writes, reassembled from `Up::HostBytes`.
pub type BleFrames = FrameAccumulator<MAX_HOST_FRAME>;

/// What a central that loses the queue to USB is told. The host transport reads it as the reason
/// its link ended, so both ends take the text from `wartui-proto`.
const NOTICE: &str = USB_TOOK_BRIDGE;

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

/// What the C6's latest `BleState` was to the owner.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Reader {
    /// Neither a host nor a phone.
    Neither,
    /// A central that may hold the host queue ([`authorized`]).
    Host,
    /// A phone on the WardriveESP stream, while the setting is on.
    Phone,
}

/// The Bluetooth host's state, as the link and the host futures see it.
pub struct Ble {
    /// The C6's latest `BleState`.
    state: Cell<BleState>,
    /// What that state was to the owner, as of when it arrived.
    reader: Cell<Reader>,
    /// The wrapping stream offset of the next byte sent. It never rewinds.
    sent: Cell<u32>,
    /// The C6's latest `Up::Credit`: the stream offset it is done up to.
    accounted: Cell<u32>,
    notice: RefCell<Notice>,
    /// Static, since it holds a whole host frame.
    frames: RefCell<&'static mut BleFrames>,
}

impl Ble {
    pub fn new(frames: &'static mut BleFrames) -> Self {
        Self {
            state: Cell::new(BleState::default()),
            reader: Cell::new(Reader::Neither),
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

    /// Stream bytes the window still allows (`host_window`).
    fn window(&self) -> usize {
        host_window(self.sent.get(), self.accounted.get()) as usize
    }
}

/// Whether `state` is a central that may hold the host queue.
const fn authorized(state: BleState) -> bool {
    matches!(state.stream, Stream::Host) && state.encrypted && state.bonded
}

/// What `state` is to the owner while the setting is `phones`.
const fn reader(state: BleState, phones: bool) -> Reader {
    if authorized(state) {
        Reader::Host
    } else if matches!(state.stream, Stream::Phone) && phones {
        Reader::Phone
    } else {
        Reader::Neither
    }
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
            Effect::AttachPhone => crate::wardrive_esp::attach(shared),
            Effect::ReleasePhone => crate::wardrive_esp::release(shared),
        }
    }
    crate::bonds::follow_admission(shared);
    effects
}

/// The C6's Bluetooth link, sent on a change and repeated every second. Only a change of what it
/// is to the owner ([`Reader`]) reaches the owner, the old one's end first, so a repeat changes
/// nothing here.
pub fn on_state(shared: &Shared, state: BleState) {
    let ble = &shared.ble;
    ble.state.set(state);
    let now = reader(state, shared.phone.enabled());
    let was = ble.reader.replace(now);
    if was != now {
        let_go(shared, was);
        match now {
            Reader::Neither => {}
            Reader::Host => {
                let _ = own(shared, Input::Subscribed);
            }
            Reader::Phone => {
                let _ = own(shared, Input::PhoneSubscribed);
            }
        }
    }
}

/// Tell the owner that `reader` let go.
fn let_go(shared: &Shared, reader: Reader) {
    match reader {
        Reader::Neither => {}
        Reader::Host => {
            let _ = own(shared, Input::Unsubscribed);
        }
        Reader::Phone => {
            let _ = own(shared, Input::PhoneUnsubscribed);
        }
    }
}

/// The C6 is done with the stream up to `accounted`. A repeat of the last changes nothing but a
/// wake of the writer.
pub fn on_credit(shared: &Shared, accounted: u32) {
    shared.ble.accounted.set(accounted);
    shared.wake_link.signal(());
}

/// A new C6 life: its central or phone is gone, and the bytes in flight died with the old C6.
/// `sent` keeps counting, so nothing is in flight from here.
pub fn on_new_life(shared: &Shared) {
    let ble = &shared.ble;
    ble.state.set(BleState::default());
    let_go(shared, ble.reader.replace(Reader::Neither));
    ble.notice.borrow_mut().drop_it();
    ble.accounted.set(ble.sent.get());
}

/// Begin an attachment's stream down the C6 link: drop a notice owed to an earlier one, and queue
/// the `Down::HostStart` at `sent`.
pub fn start_stream(shared: &Shared) {
    let ble = &shared.ble;
    ble.notice.borrow_mut().drop_it();
    // Ahead of this attachment's bytes: the link's writer packs the down queue's records before
    // any host bytes, and none while a record is held (`Link::fill`). A full queue refuses it and
    // counts the refusal; the C6 then reads the bytes as following on from the last.
    let _ = shared.push_down(&Down::HostStart { at: ble.sent.get() });
}

/// Make Bluetooth the host: mark where its stream starts, clear the queue, and announce.
fn take(shared: &Shared) {
    shared.ble.frames.borrow_mut().reset();
    start_stream(shared);
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
/// the window allow: the notice first, while one is owed, then the queue's, while a central or a
/// phone holds it. `scratch` holds them on the way.
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
    if !matches!(shared.owner.get().holder(), Holder::Bluetooth | Holder::Phone) {
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
