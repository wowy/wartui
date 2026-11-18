//! The host link over Bluetooth: a GATT peripheral that carries the P4's host stream to one
//! bonded central and the central's frames back, or, while the P4 admits phones, a stream to one
//! phone on the WardriveESP service and its commands back.
//!
//! TrouBLE is the host, over `esp-radio`'s controller, on the link's executor: its futures are
//! joined beside the link's in the one task, so each `.await` here hands the C6 link its turn.
//! The link's service, its characteristics and the advertised name are `wartui_proto::ble`'s;
//! the WardriveESP service's are `wartui_proto::wardrive_esp::frame`'s.
//!
//! **Up notifies the stream; Down takes the central's writes.** Up carries what the P4 sends as
//! `Down::HostBytes`, cut at the ATT MTU less 3 wherever that falls. Down forwards each write
//! to the P4 as `Up::HostBytes`, in order. A write waits for room in the queue to the P4 rather
//! than evicting what it holds, and one that waits past [`FORWARD_WAIT`] is dropped and counted
//! in `dropped_writes`, so the queue stays the only place a write is lost. Every other record
//! evicts the queue's oldest to make room, and the oldest can be a write already acknowledged, so
//! a write is queued only while [`UP_RESERVE_BYTES`] stay free beside it. A write request is
//! answered only once its write is forwarded, behind every write queued ahead of it, which is
//! what paces a central that ends each write group with one (`wartui_bridge::ble`). A request
//! whose write was dropped is refused with Insufficient Resources rather than acknowledged, so the
//! central learns its group is gone: wartui's transport ends the connection on a failed request,
//! and its next connection leads with a `0x00`, which ends the torn frame on the P4. A write
//! without response is answered by nothing either way.
//!
//! **The stream is credited by position** (`wartui_proto::radio_link::Inflow` has the design).
//! `Up::Credit` carries `accounted`, the stream offset in the P4's offsets up to which the C6 is
//! done: the next offset it expects, less the stream bytes the [`Outbox`] holds. Everything before
//! it was handed to TrouBLE, discarded, or found missing. Credit follows the hand-off, not
//! delivery: the window keeps the C6 link from stalling behind RTS/CTS, and bytes TrouBLE holds
//! are off that link. The position is recomputed whenever it may have moved: after each
//! `Down::HostStart` and `Down::HostBytes`, after each notification, and when a change of
//! stream clears the ring. It is coalesced to one `Up::Credit` per [`CREDIT_EVERY_BYTES`],
//! per [`CREDIT_EVERY`], or when the ring empties, whichever comes first, and nothing is sent
//! before the P4 has named a first offset. With no central reading the stream is discarded and
//! credited at once. While a central reads the current position is also repeated every
//! [`STATE_EVERY`] ([`repeat_credit`]): the queue to the P4 evicts, and losing the last credit
//! before the window fills would leave the P4's window shut with nothing left to move it. The
//! position is absolute, so the P4 takes a repeat as changing nothing.
//!
//! The resync byte ends what a gap tore: `0x00` on the host stream, a COBS terminator, and `\n`
//! on a phone's, which ends its line.
//!
//! - `Down::HostStart` begins an attachment: the ring is cleared and the position is `at`, so the
//!   central reads only bytes from that offset on. One resync byte goes in first. A notification
//!   [`notify`] took from the ring before the clear is still sent, and its partial frame would run
//!   into this attachment's `Ready`; the resync byte ends it, so the `Ready` decodes whole. It is
//!   not a stream byte either, and is never credited.
//! - A gap before a `Down::HostBytes` is counted in `resynced_bytes`, and the position is past it.
//!   One resync byte goes into the ring ahead of the bytes, so the central's reader drops the torn
//!   frame or line rather than merging it with the next. That byte is not a stream byte, so the
//!   position never counts it. The ring holds [`HOST_WINDOW_BYTES`] and [`RESYNC_MAX`] of them; a
//!   gap with no room left gets none, and the central resynchronizes at the next terminator, one
//!   frame later.
//!
//! **Which stream a central reads is a CCCD** (`Stream`): Up's, on an encrypted and bonded link,
//! for the host stream, else TX's, while phones are admitted, for the phone's. A central reads
//! one: a subscription to the other is refused with Write Request Rejected. Every change of the
//! stream,
//! the encryption, the bond or the MTU sends a `BleState`; the P4 reads the host stream as
//! Bluetooth taking the host queue. The queue to the P4 evicts and the link does not resend, so
//! one lost `BleState` would leave the P4 never knowing a central subscribed: the current one is
//! also repeated every [`STATE_EVERY`] once the P4 has said hello. The P4 acts only on a change of
//! whether the central may hold the queue, so a repeat changes nothing there. The ring is cleared
//! on every change of stream, which moves the credit past what it held, so a new central never
//! reads the last one's stream.
//!
//! **The C6 asks for the 2M PHY and data length 251 once connected, and an interval only when
//! the build names one** ([`CONN_INTERVAL_US`]). TrouBLE reports only whether a request went out:
//! an accepted one shows in `BleCounters`' `phy` and `interval_us`, and a refusal as no change.
//!
//! **Three upstream bugs shape this build** (`docs/c6-ble-host-findings.md`, "Workarounds a
//! production build needs"). The packet pool holds 186 bytes, so the C6 serves an ATT MTU of
//! [`ATT_MTU_MAX`], 182, and a notification carries at most [`VALUE_MAX`], 179 bytes. That keeps
//! every notification in one ACL packet, so none fragments (`Cargo.toml`). It is also the
//! workaround for the chained packet: esp-radio copies only the first buffer of a received packet
//! the controller hands over as a chain, which it does from 193 bytes, and the short packet stops
//! the host. A central on that MTU sends no packet over 190 bytes
//! (`docs/ble-host-link-findings.md`, "The host stop, found"). `trouble-host`'s
//! `connection-params-update` stays off, so a central's parameter request never reaches the
//! host, and the interval request goes out over L2CAP.
//!
//! **Only a bonded, encrypted central reaches the host stream**, by LE Secure Connections Just
//! Works pairing (`docs/c6-ble-pairing-findings.md`). Up's CCCD and Down's writes require
//! encryption in the attribute table, which TrouBLE checks before a write reaches [`events`],
//! and refuses with Insufficient Authentication, the answer that makes BlueZ, iOS and Android
//! pair. Just Works gives an unbonded link the same level, so the C6 checks the bond itself too:
//! a Down write is forwarded, and a subscription counts in `BleState`, only while the link is
//! encrypted and the central holds a stored bond ([`authorized`]).
//!
//! **A phone reaches only the WardriveESP service, and only while the P4 admits phones**
//! (`Down::Bonds`' `wardrive_esp`). The service is open, as the protocol has it
//! (`docs/vendor/wardrive-esp/PROTOCOL.md`), and nothing on it is ever refused with Insufficient
//! Authentication, which makes a phone pair. While phones are admitted:
//!
//! - the advertisement carries the WardriveESP service's UUID, which a phone app looks for, and
//!   the scan response the name; a change of admission restarts the advertisement ([`accept`]);
//! - no central gets a Security Request, which raises a pairing prompt on a phone;
//! - a phone subscribed to TX reads the stream in frames: each owed ACK first, then one DATA frame
//!   a line, or part of one longer than the frame's room;
//! - a write to RX or CTRL is read through `frame::Inbound`: a CMD's ACK is owed, and the command
//!   goes to the P4 as `Up::WardriveEspCommand`, waiting for room as Down's writes do. A write
//!   before the phone reads TX is dropped.
//!
//! Outside admission a write request to RX or CTRL is refused and TX's CCCD cannot be enabled, and
//! a phone still reading is disconnected at the next `Down::Bonds` ([`revoke`], [`events`]).
//!
//! **A central the C6 holds a bond for gets no Security Request.** Outside the window the request
//! would say no bonding, and BlueZ 5.88 answers that by pairing again rather than encrypting with
//! the key it holds, and is then disconnected as unbonded (`docs/c6-ble-pairing-findings.md`,
//! "Two faults the bench found"). It encrypts on its own, or on the Insufficient Authentication
//! answer, as a wartui host does while phones are admitted.
//!
//! **TrouBLE accepts every pairing, so the window is the C6's to enforce.** A connection is
//! bondable only when it is made inside the P4's pairing window. These centrals are disconnected
//! and counted in `unbonded_kicks`, so none holds the only connection without a bond or a phone's
//! stream:
//!
//! - one encrypted without a stored bond on a connection made outside the window;
//! - one that lost its bond, outside the window;
//! - one neither bonded and encrypted nor reading the phone's stream [`PAIR_DEADLINE`] after
//!   connecting inside the window, or [`UNBONDABLE_DEADLINE`] after connecting outside it;
//! - one whose bond the P4 dropped, as `ClearBonds` does, while it was connected.
//!
//! A central that reads no stream for [`IDLE_DEADLINE`] after that is disconnected too, and not
//! counted there, so a phone's background link cannot hold the only connection either
//! ([`deadline`]).
//!
//! **The P4 keeps the bonds; the C6 holds them in TrouBLE's RAM** (`wartui_proto::bonds`). It
//! waits up to [`BONDS_WAIT`] for the P4's first `Down::Bonds` before it advertises, so the
//! resolving list holds them from the first advertisement. TrouBLE loads its resolving list only
//! while it is not advertising, so a `Down::Bonds` that differs from TrouBLE's bonds stops the
//! advertisement, is applied, and advertising starts again ([`accept`], [`sync_bonds`]). One that
//! differs in nothing, as after every hello and in the P4's repeat every 5 s, leaves the
//! advertisement running, logs nothing, and leaves a connected central as it is. While a central
//! is connected the bonds are applied once it goes, and a central whose bond the P4 has dropped is
//! disconnected first ([`revoke`]). A new bond goes up as `Up::BondAdded`, after an
//! `Up::BondRemoved` when it replaces one a central lost. The link never resends, so the
//! `Up::BondAdded` goes again every [`STATE_EVERY`] until the P4's list holds the bond, at most
//! [`BOND_SENDS`] times in all ([`repeat_bond`]). The bound keeps a bond the P4 was told to clear
//! before it held it from coming back forever. A bond the P4 has yet to hold leaves TrouBLE at
//! the next disconnect, as any the P4 lacks does, and returns with the `Down::Bonds` that echoes a
//! later send. The window's end is tracked here from each `Down::Bonds`' `window_ms`, the P4's
//! time left as it sent it.
//!
//! **The first advertisement ends the image's trial** with the P4's hello (`src/link.rs`):
//! [`advertise`] raises `advertising_started` once the controller takes it.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;
use core::pin::pin;

use embassy_futures::join::join;
use embassy_futures::select::{Either, select, select4};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::BT;
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use trouble_host::att::AttClient;
use trouble_host::prelude::*;
use wartui_proto::ble::{ByteRing, NAME_PREFIX, SERVICE_UUID};
use wartui_proto::bonds::{BondAddr, BondLevel, BondRecord, Bonds};
use wartui_proto::heapless::Deque;
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::{
    Accept, BleCounters, BleState, HOST_WINDOW_BYTES, Inflow, Stream, Up,
};
use wartui_proto::wardrive_esp::command;
use wartui_proto::wardrive_esp::frame::{self, Inbound};

use crate::config::{CONN_INTERVAL_US, UP_QUEUE_MIN_BYTES};
use crate::link::Shared;
use gatt::Server;

/// Resync `0x00`s the ring holds beside the window's stream bytes.
const RESYNC_MAX: usize = 16;

/// The ring's bytes: the most the P4 keeps in flight, and room for the resync `0x00`s.
const OUTBOX_BYTES: usize = HOST_WINDOW_BYTES as usize + RESYNC_MAX;

/// The ATT MTU the C6 serves: TrouBLE's packet pool less the 4-byte L2CAP header. It is the
/// workaround for two esp-radio 1.0.0-beta.1 bugs (`Cargo.toml`): no notification fragments, and
/// no central's packet reaches the size the controller hands over as a chain.
const ATT_MTU_MAX: usize = wartui_proto::ble::ATT_MTU as usize;

const _: () = assert!(
    <DefaultPacketPool as PacketPool>::MTU == ATT_MTU_MAX + 4,
    "TrouBLE's packet pool is not wartui_proto::ble::ATT_MTU + 4: build from firmware/p4-radio, \
     whose .cargo/config.toml sizes it with TROUBLE_HOST_DEFAULT_PACKET_POOL_MTU"
);

/// The most a notification or a write carries: the MTU less the ATT opcode and handle.
const VALUE_MAX: usize = ATT_MTU_MAX - 3;

/// Credit at least this often, in bytes, while the stream flows.
const CREDIT_EVERY_BYTES: u32 = 2048;

/// Credit at least this often while the stream flows.
const CREDIT_EVERY: Duration = Duration::from_millis(20);

/// How long a central's write waits for room in the queue to the P4.
const FORWARD_WAIT: Duration = Duration::from_secs(1);

/// What a write to the queue to the P4 costs beyond its bytes: the ring's length prefix, the
/// record's variant and the bytes' length.
const FORWARD_OVERHEAD: usize = 2 + 1 + 2;

/// Bytes of the queue to the P4 a central's write leaves free for the C6's other records:
/// credits, state, a status reply and a few log lines fit in it while the P4 is not reading.
const UP_RESERVE_BYTES: usize = 1024;

const _: () = assert!(
    UP_RESERVE_BYTES + VALUE_MAX + FORWARD_OVERHEAD <= UP_QUEUE_MIN_BYTES,
    "the smallest queue to the P4 holds no write beside the reserve"
);

/// How often the C6 repeats its `BleState` to the P4, changed or not.
const STATE_EVERY: Duration = Duration::from_secs(1);

/// The first wait of a notification the packet pool has no room for, doubled on each retry up
/// to [`POOL_RETRY_MAX`].
const POOL_RETRY: Duration = Duration::from_micros(500);

/// The longest wait between those retries: under the shortest connection interval, 7.5 ms, so
/// the controller's queue is refilled before the connection event after the one that freed it.
const POOL_RETRY_MAX: Duration = Duration::from_millis(4);

/// The supervision timeout a connection interval request asks for.
const SUPERVISION_TIMEOUT: Duration = Duration::from_secs(4);

/// The data length the C6 asks for: one 251-byte PDU per packet, sent in 2,120 µs at 1M.
const DATA_LENGTH: (u16, u16) = (251, 2120);

/// HCI commands in flight at once.
const COMMAND_SLOTS: usize = 4;

/// One central at a time.
const CONNECTIONS: usize = 1;

/// The L2CAP channels a connection uses: signaling and ATT.
const L2CAP_CHANNELS: usize = 2;

/// Advertising sets: the one advertisement.
const ADV_SETS: usize = 1;

/// Bonds TrouBLE holds: the P4's four, and room for a fifth pairing to land before the P4 evicts
/// the oldest. A pairing also takes a slot while it runs, bonded or not, and fails when none is
/// free.
const BONDS: usize = wartui_proto::bonds::MAX_BONDS + 1;

/// How long the C6 waits for the P4's first `Down::Bonds` before it advertises with none.
const BONDS_WAIT: Duration = Duration::from_secs(2);

/// How often that wait looks.
const BONDS_POLL: Duration = Duration::from_millis(20);

/// How long TrouBLE's runner is given to stop an advertisement, and then to load a changed bond
/// list into the resolving list, before the next advertisement. It loads the list only while
/// advertising is idle, so an update made while advertising waits for the next idle moment, a
/// connection, by when the controller has already failed to resolve that central's address.
const RESOLVING_SETTLE: Duration = Duration::from_millis(50);

/// How long a central that connected inside the pairing window has to be bonded and encrypted
/// before it is disconnected. Pairing waits on a person tapping the phone's prompt, which took
/// more than 10 s on the Pixel 8 (inferred); 30 s is the pairing protocol's own timeout.
const PAIR_DEADLINE: Duration = Duration::from_secs(30);

/// The same, for a central that connected outside the window: it can only resume a bond, which
/// took under 0.7 s on every host, so it gets no time to wait on a person.
const UNBONDABLE_DEADLINE: Duration = Duration::from_secs(10);

/// How long a bonded central may stay unsubscribed from Up before it is disconnected, from when
/// it connected, paired or last unsubscribed. `wartui` subscribes within a second or two of
/// connecting, and a person opening a test app needs longer.
const IDLE_DEADLINE: Duration = Duration::from_secs(30);

/// How many times a new bond goes up as `Up::BondAdded`, the first included, while the P4's list
/// lacks it: about 10 s of sends, where the P4 echoes one within a second.
const BOND_SENDS: u8 = 10;

/// ACKs owed to a phone at once: a write short of TrouBLE's queue of 8 per connection.
const ACKS_MAX: usize = 8;

/// `wartui XX:YY`.
const NAME_MAX: usize = NAME_PREFIX.len() + 5;

/// How long one advertising interval is.
const ADVERTISE_EVERY: Duration = Duration::from_millis(100);

type Controller<'a> = ExternalController<BleConnector<'a>, COMMAND_SLOTS>;
type BleStack<'a> = Stack<'a, Controller<'a>, DefaultPacketPool>;
type Conn<'a, 'b> = GattConnection<'a, 'b, DefaultPacketPool>;

mod gatt {
    //! The GATT server and its two services.
    //!
    //! TrouBLE's `#[gatt_server]` constructors build the attribute table by value. With both
    //! services that is a frame of about 5.4 KB: `Server` is 1,128 bytes and each attribute 44.
    //! It is built once, at boot, from `advertise`. The allow covers only this generated code, so
    //! every other frame in the crate answers to the crate's threshold.
    #![allow(clippy::large_stack_frames)]

    use embassy_sync::blocking_mutex::raw::NoopRawMutex;
    use trouble_host::prelude::*;
    use wartui_proto::ble::{DOWN_UUID, SERVICE_UUID, UP_UUID};
    use wartui_proto::wardrive_esp::frame::{self, CTRL_UUID, RX_UUID, TX_UUID};

    use super::CONNECTIONS;

    /// The link's service first, so its handles are the same with the WardriveESP service beside
    /// it.
    #[gatt_server(connections_max = CONNECTIONS, mutex_type = NoopRawMutex)]
    pub(super) struct Server {
        pub(super) link: LinkService,
        pub(super) wardrive_esp: WardriveEspService,
    }

    /// The "wartui link" service.
    #[gatt_service(uuid = SERVICE_UUID)]
    pub(super) struct LinkService {
        /// The bridge's frame stream, notified. Its stored value is never read. Subscribing needs
        /// an encrypted link.
        #[characteristic(uuid = UP_UUID, notify, permissions(encrypted))]
        pub(super) up: u8,
        /// The host's frames, written. Writes are taken unprocessed, so nothing is stored. Writing
        /// needs an encrypted link.
        #[characteristic(uuid = DOWN_UUID, write, write_without_response, permissions(encrypted))]
        pub(super) down: u8,
    }

    /// The WardriveESP service, open: a phone app reads it with no pairing.
    #[gatt_service(uuid = frame::SERVICE_UUID)]
    pub(super) struct WardriveEspService {
        /// A phone's commands, written. Taken unprocessed, so nothing is stored.
        #[characteristic(uuid = RX_UUID, write, write_without_response)]
        pub(super) rx: u8,
        /// The phone's output, notified in frames. Its stored value is never read.
        #[characteristic(uuid = TX_UUID, notify)]
        pub(super) tx: u8,
        /// A phone's commands, read as RX's are.
        #[characteristic(uuid = CTRL_UUID, write, write_without_response)]
        pub(super) ctrl: u8,
    }
}

/// The stream waiting to be notified, and where the resync bytes in it sit, so the credit counts
/// only its stream bytes.
pub struct Outbox {
    ring: ByteRing<OUTBOX_BYTES>,
    /// Each resync byte the ring holds, oldest first, as the count of bytes pushed before it.
    resyncs: Deque<u32, RESYNC_MAX>,
    /// Bytes pushed and taken since boot, each wrapping.
    pushed: u32,
    taken: u32,
}

impl Outbox {
    /// Empty, for a static: a constant rather than a `const fn`, which would build 16 KB in a
    /// stack frame if ever called at run time.
    pub const EMPTY: Self =
        Self { ring: ByteRing::new(), resyncs: Deque::new(), pushed: 0, taken: 0 };

    /// Append stream bytes, as many as fit, and return how many did.
    fn push(&mut self, bytes: &[u8]) -> usize {
        let n = self.ring.push(bytes);
        self.pushed = self.pushed.wrapping_add(n as u32);
        n
    }

    /// Append one resync `byte`, or return `false` when there is no room for it.
    fn push_resync(&mut self, byte: u8) -> bool {
        if self.resyncs.is_full() || self.ring.push(&[byte]) == 0 {
            return false;
        }
        // Room was checked above.
        let _ = self.resyncs.push_back(self.pushed);
        self.pushed = self.pushed.wrapping_add(1);
        true
    }

    /// Move the oldest bytes into `out`, and return how many.
    fn take(&mut self, out: &mut [u8]) -> usize {
        let n = self.ring.take(out);
        self.forget(n);
        n
    }

    /// Move the oldest bytes into `out`, up to and including the first `delim`, and return how
    /// many.
    fn take_through(&mut self, out: &mut [u8], delim: u8) -> usize {
        let n = self.ring.take_through(out, delim);
        self.forget(n);
        n
    }

    /// Account for `n` bytes taken: drop the resyncs among them.
    fn forget(&mut self, n: usize) {
        while self.resyncs.front().is_some_and(|at| at.wrapping_sub(self.taken) < n as u32) {
            self.resyncs.pop_front();
        }
        self.taken = self.taken.wrapping_add(n as u32);
    }

    /// Discard everything held.
    fn clear(&mut self) {
        self.ring.clear();
        self.resyncs.clear();
        self.taken = self.pushed;
    }

    /// The stream bytes held: every byte but the resync bytes.
    fn stream_held(&self) -> usize {
        self.ring.len() - self.resyncs.len()
    }

    fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// A new bond the P4's list has yet to hold, when its `Up::BondAdded` last went, and how many
/// times it has.
#[derive(Clone, Copy)]
struct PendingBond {
    record: BondRecord,
    sent_at: Instant,
    sent: u8,
}

/// The Bluetooth side's state, as the link and the stack see it.
pub struct Ble {
    outbox: &'static RefCell<Outbox>,
    /// The next stream offset expected from the P4.
    inflow: Cell<Inflow>,
    /// The link as the C6 sees it, as it last told the P4, and when.
    state: Cell<BleState>,
    said: Cell<Option<BleState>>,
    said_at: Cell<Instant>,
    /// The controller has taken an advertisement since boot.
    advertising: Cell<bool>,
    /// Raised when `advertising` first holds, for the link's trial.
    pub advertising_started: Signal<NoopRawMutex, ()>,
    /// The `Up::Credit` last sent or repeated, or `None` before the first, and when.
    credited: Cell<Option<u32>>,
    credited_at: Cell<Instant>,
    /// The counters kept here; [`counters`] adds the heap's.
    counters: Cell<BleCounters>,
    /// Raised when the ring or the subscription changes, for the notifier.
    ready: Signal<NoopRawMutex, ()>,
    /// The bonds the P4 last sent, oldest first, and whether any `Down::Bonds` has arrived.
    bonds: RefCell<Bonds>,
    bonds_heard: Cell<bool>,
    /// Raised by each `Down::Bonds`. One waiter at a time: [`accept`] while advertising,
    /// [`revoke`] while a central is connected.
    bonds_changed: Signal<NoopRawMutex, ()>,
    /// When the P4's pairing window closes, or `None` while it is closed.
    window_until: Cell<Option<Instant>>,
    /// The bond last paired, until the P4's list holds it ([`repeat_bond`]).
    pending_bond: Cell<Option<PendingBond>>,
    /// A phone may read the WardriveESP service: the P4 said so in its last `Down::Bonds`.
    admitted: Cell<bool>,
    /// The id the phone's frames carry. It lives as long as the C6, not the connection.
    inbound: Cell<Inbound>,
    /// The ids of the ACKs owed to the phone, oldest first. Cleared when the stream changes.
    acks: RefCell<Deque<u32, ACKS_MAX>>,
}

impl Ble {
    pub fn new(outbox: &'static RefCell<Outbox>) -> Self {
        Self {
            outbox,
            inflow: Cell::new(Inflow::new()),
            state: Cell::new(BleState::default()),
            said: Cell::new(None),
            said_at: Cell::new(Instant::from_ticks(0)),
            advertising: Cell::new(false),
            advertising_started: Signal::new(),
            credited: Cell::new(None),
            credited_at: Cell::new(Instant::from_ticks(0)),
            counters: Cell::new(BleCounters::default()),
            ready: Signal::new(),
            bonds: RefCell::new(Bonds::new()),
            bonds_heard: Cell::new(false),
            bonds_changed: Signal::new(),
            window_until: Cell::new(None),
            pending_bond: Cell::new(None),
            admitted: Cell::new(false),
            inbound: Cell::new(Inbound::new()),
            acks: RefCell::new(Deque::new()),
        }
    }

    fn count(&self, change: impl FnOnce(&mut BleCounters)) {
        let mut counters = self.counters.get();
        change(&mut counters);
        self.counters.set(counters);
    }

    /// Which stream the central reads.
    pub fn stream(&self) -> Stream {
        self.state.get().stream
    }

    /// Whether a phone may read the WardriveESP service.
    fn admitted(&self) -> bool {
        self.admitted.get()
    }

    /// Whether the P4's pairing window is open.
    fn window_open(&self) -> bool {
        self.window_until.get().is_some_and(|until| Instant::now() < until)
    }

    /// Whether the controller has taken an advertisement since boot.
    pub fn advertising(&self) -> bool {
        self.advertising.get()
    }
}

/// The byte that ends a torn piece of `stream`: `0x00` ends a COBS frame, and `\n` a phone's line.
fn resync_byte(stream: Stream) -> u8 {
    if stream == Stream::Phone { b'\n' } else { 0 }
}

/// A central took the host queue, and its first byte is at `at`: clear the ring, lead it with a
/// resync byte, and credit up to `at`.
pub fn host_start(shared: &Shared, at: u32) {
    let ble = &shared.ble;
    let mut inflow = ble.inflow.get();
    // What never arrived before `at` is behind the position, which is all the credit needs.
    let _ = inflow.start(at);
    ble.inflow.set(inflow);
    let mut outbox = ble.outbox.borrow_mut();
    outbox.clear();
    // A cleared ring has room for it.
    outbox.push_resync(resync_byte(ble.stream()));
    drop(outbox);
    credit(shared, true);
    ble.ready.signal(());
}

/// Take the host stream's next bytes, at stream offset `at`: held for the central, or discarded
/// and credited when none reads.
pub fn host_bytes(shared: &Shared, at: u32, bytes: &[u8]) {
    let ble = &shared.ble;
    let mut inflow = ble.inflow.get();
    let accept = inflow.accept(at, bytes.len());
    ble.inflow.set(inflow);
    let Accept::Take { skip, gap } = accept else { return };
    let bytes = &bytes[skip..];
    if gap > 0 {
        ble.count(|c| c.resynced_bytes = c.resynced_bytes.wrapping_add(gap));
    }
    if ble.stream() == Stream::None {
        // Nothing is held, so the credit is the expected offset.
        credit(shared, true);
        return;
    }
    let mut outbox = ble.outbox.borrow_mut();
    // Without room, the central resynchronizes at the next terminator instead.
    if gap > 0 {
        outbox.push_resync(resync_byte(ble.stream()));
    }
    let lost = bytes.len() - outbox.push(bytes);
    drop(outbox);
    // Bytes with no room are behind the position, done with: credit them at once.
    credit(shared, lost > 0);
    ble.ready.signal(());
}

/// Take the P4's bonds, its pairing window's time left, and whether it admits phones.
/// [`advertise`] gives the bonds to TrouBLE, at once while it advertises ([`accept`]) and else
/// once the central goes, and [`revoke`] disconnects a central whose bond they dropped, and a
/// phone once phones are not admitted. A list that holds the pending bond ends its repeats.
///
/// Each `Down::Bonds` signals `bonds_changed`, a repeat that changes nothing included. Its waiters
/// compare before they act: [`accept`] keeps the advertisement running while TrouBLE's bonds are
/// the P4's, and [`revoke`] finds the central's bond where it was. The window's end is re-derived
/// from each, and moves by no more than the message's transit.
pub fn take_bonds(shared: &Shared, bonds: Bonds, window_ms: u32, wardrive_esp: bool) {
    let ble = &shared.ble;
    ble.admitted.set(wardrive_esp);
    let until = (window_ms > 0).then(|| Instant::now() + Duration::from_millis(window_ms.into()));
    ble.window_until.set(until);
    if ble.pending_bond.get().is_some_and(|pending| holds(&bonds, &pending.record)) {
        ble.pending_bond.set(None);
    }
    ble.bonds.replace(bonds);
    ble.bonds_heard.set(true);
    ble.bonds_changed.signal(());
}

/// Whether `bonds` holds `record`'s bond: its address, with its key.
fn holds(bonds: &Bonds, record: &BondRecord) -> bool {
    bonds.iter().any(|kept| kept.addr == record.addr && kept.ltk == record.ltk)
}

/// Send the pending bond's `Up::BondAdded` again once [`STATE_EVERY`] has passed since the last,
/// up to [`BOND_SENDS`] in all, so a lost one does not leave the P4 without the bond.
pub fn repeat_bond(shared: &Shared) {
    let ble = &shared.ble;
    let Some(mut pending) = ble.pending_bond.get() else { return };
    if pending.sent_at.elapsed() < STATE_EVERY {
        return;
    }
    if pending.sent >= BOND_SENDS {
        ble.pending_bond.set(None);
        let peer = pending.record.addr.mac();
        shared.log(format_args!(
            "Bluetooth bond {} not echoed by the P4 after {BOND_SENDS} sends",
            mac::short(&peer)
        ));
        return;
    }
    shared.push(&Up::BondAdded(pending.record));
    pending.sent += 1;
    pending.sent_at = Instant::now();
    ble.pending_bond.set(Some(pending));
}

/// The counters `RadioStatus.ble` reports.
pub fn counters(shared: &Shared) -> BleCounters {
    let mut counters = shared.ble.counters.get();
    counters.heap_high_water = esp_alloc::HEAP.stats().max_usage as u32;
    counters
}

/// Send the credit when it has moved and is due or `now`. Nothing goes before the P4 has named a
/// first offset.
fn credit(shared: &Shared, now: bool) {
    let ble = &shared.ble;
    let Some(accounted) = accounted(ble) else { return };
    // A position the P4 has never heard is owed in full.
    let owed = ble.credited.get().map_or(u32::MAX, |credited| accounted.wrapping_sub(credited));
    let due = now || owed >= CREDIT_EVERY_BYTES || ble.credited_at.get().elapsed() >= CREDIT_EVERY;
    if owed > 0 && due {
        send_credit(shared, accounted);
    }
}

/// Send the credit again once [`STATE_EVERY`] has passed since the last, while a central reads
/// and the P4 has named a first offset, so a lost credit cannot leave the P4's window
/// shut. The P4's `on_credit` sets the position it already holds and wakes its writer, so a
/// repeat changes nothing there.
pub fn repeat_credit(shared: &Shared) {
    let ble = &shared.ble;
    if ble.stream() == Stream::None || ble.credited_at.get().elapsed() < STATE_EVERY {
        return;
    }
    if let Some(accounted) = accounted(ble) {
        send_credit(shared, accounted);
    }
}

/// The credit: the expected offset less the stream bytes held, or `None` before the P4 has named
/// a first offset.
fn accounted(ble: &Ble) -> Option<u32> {
    let expected = ble.inflow.get().expected()?;
    Some(expected.wrapping_sub(ble.outbox.borrow().stream_held() as u32))
}

fn send_credit(shared: &Shared, accounted: u32) {
    let ble = &shared.ble;
    ble.credited.set(Some(accounted));
    ble.credited_at.set(Instant::now());
    shared.push(&Up::Credit { accounted });
}

/// Set the link's state, and tell the P4 if it changed. A change of stream clears the ring and
/// the ACKs owed.
fn set_state(shared: &Shared, change: impl FnOnce(&mut BleState)) {
    let ble = &shared.ble;
    let mut state = ble.state.get();
    let was = state.stream;
    change(&mut state);
    ble.state.set(state);
    if state.stream != was {
        ble.outbox.borrow_mut().clear();
        ble.acks.borrow_mut().clear();
        credit(shared, true);
        ble.ready.signal(());
    }
    if ble.said.get() != Some(state) {
        say_state(shared, state);
    }
}

/// Tell the P4 the link's state again once [`STATE_EVERY`] has passed since it was last told.
pub fn repeat_state(shared: &Shared) {
    if shared.ble.said_at.get().elapsed() >= STATE_EVERY {
        say_state(shared, shared.ble.state.get());
    }
}

fn say_state(shared: &Shared, state: BleState) {
    let ble = &shared.ble;
    ble.said.set(Some(state));
    ble.said_at.set(Instant::now());
    shared.push(&Up::BleState(state));
}

/// Run the Bluetooth peripheral forever. A controller that will not start leaves the link
/// running alone, and the image is never confirmed.
pub async fn run(bt: BT<'static>, mac: &Mac, shared: &Shared) -> ! {
    let config = Config::default().with_max_connections(CONNECTIONS as u16);
    let connector = match BleConnector::new(bt, config) {
        Ok(connector) => connector,
        Err(err) => {
            shared.error(format_args!("the Bluetooth controller would not start: {err}"));
            core::future::pending().await
        }
    };
    let controller: Controller<'_> = ExternalController::new(connector);
    let mut resources: HostResources<
        DefaultPacketPool,
        CONNECTIONS,
        L2CAP_CHANNELS,
        ADV_SETS,
        BONDS,
    > = HostResources::new();
    // No display and no input: Just Works.
    let stack = trouble_host::new(controller, &mut resources)
        .set_io_capabilities(IoCapabilities::NoInputNoOutput)
        .build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    join(run_host(&mut runner), advertise(&stack, &mut peripheral, mac, shared)).await.0
}

/// Run TrouBLE's host, and restart the C6 if it ever stops.
///
/// A stopped host cannot be started again in place. Starting it resets the controller, which
/// drops the connection without a disconnection event, so TrouBLE keeps a central that is gone
/// and the C6 never advertises again: on D0:1C a host that stopped under a flood of writes left
/// the bridge unreachable over Bluetooth until its P4 was reset. A restart clears all of it, the
/// P4 resynchronizes on the C6's new life, and the panic handler keeps the reason for the next
/// boot's report (`src/last_panic.rs`).
async fn run_host(runner: &mut Runner<'_, Controller<'_>, DefaultPacketPool>) -> ! {
    let end = runner.run().await;
    panic!("the Bluetooth host stopped: {end:?}");
}

/// Advertise, serve the central that connects until it goes, and advertise again.
async fn advertise(
    stack: &BleStack<'_>,
    peripheral: &mut Peripheral<'_, Controller<'_>, DefaultPacketPool>,
    mac: &Mac,
    shared: &Shared,
) -> ! {
    let mut name = wartui_proto::heapless::String::<NAME_MAX>::new();
    let _ = write!(name, "{NAME_PREFIX}{}", mac::short(mac));
    let gap =
        GapConfig::Peripheral(PeripheralConfig { name: &name, appearance: &appearance::UNKNOWN });
    // Matched by reference, so the server is not copied out of the result.
    let built = Server::new_with_config(gap);
    let server = match &built {
        Ok(server) => server,
        Err(err) => {
            shared.error(format_args!("the GATT server would not build: {err}"));
            core::future::pending().await
        }
    };

    let (Some(wartui), Some(phones)) =
        (Payload::new(name.as_bytes(), false), Payload::new(name.as_bytes(), true))
    else {
        shared.error(format_args!("the advertisement would not encode"));
        core::future::pending().await
    };
    let params = AdvertisementParameters {
        interval_min: ADVERTISE_EVERY,
        interval_max: ADVERTISE_EVERY,
        ..Default::default()
    };

    // Polled: `bonds_changed` is `accept`'s to wait on, and this runs once, at boot.
    let waited = Instant::now();
    while !shared.ble.bonds_heard.get() && waited.elapsed() < BONDS_WAIT {
        Timer::after(BONDS_POLL).await;
    }
    if !shared.ble.bonds_heard.get() {
        shared.log(format_args!(
            "Bluetooth heard no bonds from the P4 in {} ms: advertising with none",
            BONDS_WAIT.as_millis()
        ));
    }
    let held = shared.ble.bonds.borrow().len();
    let window = if shared.ble.window_open() { "open" } else { "closed" };
    let service = service_name(shared.ble.admitted());
    shared.log(format_args!(
        "Bluetooth advertises `{name}` and the {service} service with {held} bonds, pairing \
         window {window}"
    ));
    if sync_bonds(stack, shared) {
        Timer::after(RESOLVING_SETTLE).await;
    }

    loop {
        let admitted = shared.ble.admitted();
        let payload = if admitted { &phones } else { &wartui };
        let conn = match peripheral.advertise(&params, payload.advertisement()).await {
            Ok(advertiser) => {
                if !shared.ble.advertising.replace(true) {
                    shared.ble.advertising_started.signal(());
                }
                let Some(accepted) = accept(advertiser, stack, shared, admitted).await else {
                    // The advertisement is dropped, and TrouBLE's runner stops it.
                    if shared.ble.admitted() != admitted {
                        let service = service_name(shared.ble.admitted());
                        shared.log(format_args!(
                            "Bluetooth stops advertising to advertise the {service} service"
                        ));
                    } else {
                        let held = shared.ble.bonds.borrow().len();
                        shared.log(format_args!(
                            "Bluetooth stops advertising to load {held} bonds from the P4"
                        ));
                    }
                    Timer::after(RESOLVING_SETTLE).await;
                    sync_bonds(stack, shared);
                    Timer::after(RESOLVING_SETTLE).await;
                    continue;
                };
                accepted.map_err(BleHostError::BleHost)
            }
            Err(err) => Err(err),
        };
        let conn = match conn.and_then(|conn| Ok(conn.with_attribute_server(server)?)) {
            Ok(conn) => conn,
            Err(err) => {
                shared.error(format_args!("could not advertise or connect: {err:?}"));
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };
        serve(stack, &conn, server, shared).await;
        shared.ble.count(|c| {
            c.phy = 0;
            c.interval_us = 0;
        });
        set_state(shared, |state| *state = BleState::default());
        // The bonds held back while the central was connected.
        if sync_bonds(stack, shared) {
            Timer::after(RESOLVING_SETTLE).await;
        }
    }
}

/// One advertisement and its scan response.
struct Payload {
    adv: [u8; 31],
    adv_len: usize,
    scan: [u8; 31],
    scan_len: usize,
}

impl Payload {
    /// The pair for the wartui link, or, with `phones`, for the WardriveESP service. A phone app
    /// finds a board by its service UUID in the advertisement, so that pair moves the name to the
    /// scan response; the wartui pair carries the name an operator picks the bridge by.
    fn new(name: &[u8], phones: bool) -> Option<Self> {
        let flags = AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED);
        let name = AdStructure::CompleteLocalName(name);
        let link = [SERVICE_UUID.to_le_bytes()];
        let wardrive_esp = [frame::SERVICE_UUID.to_le_bytes()];
        let (adv, scan) = if phones {
            ([flags, AdStructure::CompleteServiceUuids128(&wardrive_esp)], name)
        } else {
            ([flags, name], AdStructure::CompleteServiceUuids128(&link))
        };
        let mut payload = Self { adv: [0; 31], adv_len: 0, scan: [0; 31], scan_len: 0 };
        payload.adv_len = AdStructure::encode_slice(&adv, &mut payload.adv).ok()?;
        payload.scan_len = AdStructure::encode_slice(&[scan], &mut payload.scan).ok()?;
        Some(payload)
    }

    fn advertisement(&self) -> Advertisement<'_> {
        Advertisement::ConnectableScannableUndirected {
            adv_data: &self.adv[..self.adv_len],
            scan_data: &self.scan[..self.scan_len],
        }
    }
}

/// The service advertised while phones are `admitted` or not, for the log.
fn service_name(admitted: bool) -> &'static str {
    if admitted { "WardriveESP" } else { "wartui" }
}

/// Wait for a central to connect to `advertiser`'s advertisement, through every `Down::Bonds`
/// that leaves the P4's bonds as TrouBLE holds them and phones admitted as `admitted` says. `None`
/// once one does not: the advertisement is dropped on return, so TrouBLE stops it and can load its
/// resolving list ([`RESOLVING_SETTLE`]), and the next is the pair for phones as they now are.
async fn accept<'d>(
    advertiser: Advertiser<'d, Controller<'_>, DefaultPacketPool>,
    stack: &BleStack<'_>,
    shared: &Shared,
    admitted: bool,
) -> Option<Result<Connection<'d, DefaultPacketPool>, Error>> {
    // Pinned here, so the same advertisement runs through a `Down::Bonds` that changes nothing.
    let mut accepting = pin!(advertiser.accept());
    loop {
        match select(accepting.as_mut(), shared.ble.bonds_changed.wait()).await {
            Either::First(conn) => return Some(conn),
            Either::Second(()) if shared.ble.admitted() != admitted => return None,
            Either::Second(()) if bonds_differ(stack, shared) => return None,
            Either::Second(()) => {}
        }
    }
}

/// Whether TrouBLE's bonds differ from the P4's.
fn bonds_differ(stack: &BleStack<'_>, shared: &Shared) -> bool {
    let want = shared.ble.bonds.borrow();
    stack.with_bond_information(|held| {
        held.iter().any(|bond| unwanted(bond, &want))
            || want.iter().any(|record| lacks(held, &bond_information(record)))
    })
}

/// Whether TrouBLE's `bond` is one the P4's `want` does not hold.
fn unwanted(bond: &BondInformation, want: &Bonds) -> bool {
    let addr = bond_addr(&bond.identity);
    bond.is_bonded && !want.iter().any(|kept| kept.addr == addr)
}

/// Whether TrouBLE's `held` bonds lack `info`, or hold it with another key.
fn lacks(held: &[BondInformation], info: &BondInformation) -> bool {
    !held.iter().any(|bond| {
        bond.is_bonded && bond.identity.addr == info.identity.addr && bond.ltk == info.ltk
    })
}

/// Make TrouBLE's bonds the P4's: drop each it holds that the P4 does not, and add each the P4
/// holds that it lacks or holds with another key. Returns whether anything changed. Called only
/// while TrouBLE is not advertising ([`RESOLVING_SETTLE`]).
fn sync_bonds(stack: &BleStack<'_>, shared: &Shared) -> bool {
    let want = shared.ble.bonds.borrow().clone();
    let mut gone: wartui_proto::heapless::Vec<Identity, BONDS> = wartui_proto::heapless::Vec::new();
    stack.with_bond_information(|held| {
        for bond in held.iter().filter(|bond| unwanted(bond, &want)) {
            let _ = gone.push(bond.identity);
        }
    });
    let mut changed = !gone.is_empty();
    for identity in gone {
        let _ = stack.remove_bond_information(identity);
    }
    for record in &want {
        let info = bond_information(record);
        if !stack.with_bond_information(|held| lacks(held, &info)) {
            continue;
        }
        changed = true;
        if let Err(err) = stack.add_bond_information(info) {
            let addr = record.addr.mac();
            shared.error(format_args!(
                "Bluetooth could not hold bond {}: {err:?}",
                mac::short(&addr)
            ));
        }
    }
    changed
}

/// A central's identity address, as the P4 stores it. The controller reports an address it
/// resolved as type 2 or 3, the identity's 0 or 1 with the resolution flagged.
fn bond_addr(identity: &Identity) -> BondAddr {
    let mut addr = [0; 6];
    addr.copy_from_slice(identity.addr.addr.raw());
    BondAddr { kind: identity.addr.kind.into_inner() & 1, addr }
}

/// TrouBLE's bond as the P4 stores it.
fn bond_record(bond: &BondInformation) -> BondRecord {
    BondRecord {
        addr: bond_addr(&bond.identity),
        irk: bond.identity.irk.map(|irk| irk.0.get()),
        ltk: bond.ltk.0,
        level: match bond.security_level {
            SecurityLevel::EncryptedAuthenticated => BondLevel::Authenticated,
            SecurityLevel::Encrypted | SecurityLevel::NoEncryption => BondLevel::Encrypted,
        },
    }
}

/// A bond the P4 stores, as TrouBLE holds it.
fn bond_information(record: &BondRecord) -> BondInformation {
    let addr =
        Address { kind: AddrKind::new(record.addr.kind), addr: BdAddr::new(record.addr.addr) };
    let identity = Identity { addr, irk: record.irk.and_then(IdentityResolvingKey::new) };
    let level = match record.level {
        BondLevel::Encrypted => SecurityLevel::Encrypted,
        BondLevel::Authenticated => SecurityLevel::EncryptedAuthenticated,
    };
    BondInformation::new(identity, LongTermKey::new(record.ltk), level, true)
}

/// What one connection's security handling remembers.
struct Session {
    /// When the central connected, for its deadline and the time to pair.
    connected_at: Instant,
    /// The connection was made bondable: the pairing window was open as it connected.
    bondable: bool,
    /// The bond the central lost and is pairing again to replace, inside the window.
    lost: Cell<Option<BondAddr>>,
    /// The P4's bonds have held the central's since it connected or last paired ([`revoke`]).
    listed: Cell<bool>,
    /// The phone was disconnected because phones are not admitted ([`drop_phone`]).
    phone_dropped: Cell<bool>,
    /// Since when the central has read no stream, for [`IDLE_DEADLINE`]: when it connected,
    /// paired or last let go of one.
    idle_since: Cell<Instant>,
    /// Raised when the stream or `idle_since` changes, for [`deadline`].
    idle_changed: Signal<NoopRawMutex, ()>,
    /// The central's address, in display order, for the log.
    peer: Mac,
}

/// Whether the link is encrypted and the central holds a stored bond: what the host stream needs.
fn authorized(conn: &Conn<'_, '_>) -> bool {
    let raw = conn.raw();
    raw.security_level().is_ok_and(|level| level.encrypted()) && raw.is_bonded_peer()
}

/// Disconnect a central that holds no bond, count it in `unbonded_kicks`, and say why.
fn kick(conn: &Conn<'_, '_>, session: &Session, shared: &Shared, why: &str) {
    shared.ble.count(|c| c.unbonded_kicks = c.unbonded_kicks.wrapping_add(1));
    disconnect(conn, session, shared, why);
}

/// Disconnect a phone because phones are not admitted, once: [`revoke`] and [`events`] both look.
fn drop_phone(conn: &Conn<'_, '_>, session: &Session, shared: &Shared) {
    if !session.phone_dropped.replace(true) {
        disconnect(conn, session, shared, "phones are not admitted");
    }
}

/// Disconnect the central, and say why.
fn disconnect(conn: &Conn<'_, '_>, session: &Session, shared: &Shared, why: &str) {
    conn.raw().disconnect();
    shared.log(format_args!("Bluetooth disconnects {}: {why}", mac::short(&session.peer)));
}

/// Serve one central until it disconnects.
async fn serve(stack: &BleStack<'_>, conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) {
    let raw = conn.raw();
    let interval_us = raw.params().conn_interval.as_micros() as u32;
    shared.ble.count(|c| {
        c.phy = 1;
        c.interval_us = interval_us;
    });
    set_state(shared, |state| {
        *state =
            BleState { stream: Stream::None, encrypted: false, bonded: false, mtu: raw.att_mtu() }
    });
    let connected_at = Instant::now();
    let session = Session {
        connected_at,
        bondable: shared.ble.window_open(),
        lost: Cell::new(None),
        listed: Cell::new(false),
        phone_dropped: Cell::new(false),
        idle_since: Cell::new(connected_at),
        idle_changed: Signal::new(),
        peer: bond_addr(&raw.peer_identity()).mac(),
    };
    let window = if session.bondable { "open" } else { "closed" };
    let bonded = if raw.is_bonded_peer() { "bonded" } else { "not bonded" };
    shared.log(format_args!(
        "Bluetooth connected {}, {bonded}, pairing window {window}",
        mac::short(&session.peer)
    ));
    // Before pairing starts, which the Security Request below prompts.
    if let Err(err) = raw.set_bondable(session.bondable) {
        shared.error(format_args!("Bluetooth could not set bondable: {err:?}"));
    }
    // A phone answers a Security Request with a pairing prompt, so none goes while phones are
    // admitted: a wartui host then pairs on the Insufficient Authentication answer, or through
    // BlueZ.
    if !shared.ble.admitted()
        && !raw.is_bonded_peer()
        && let Err(err) = raw.request_security()
    {
        shared.log(format_args!("Bluetooth security not requested: {err:?}"));
    }
    // Each answered by an event if the central agrees; a refusal leaves the link as it is.
    if let Err(err) = raw.set_phy(stack, PhyKind::Le2M).await {
        shared.log(format_args!("Bluetooth 2M PHY not requested: {err:?}"));
    }
    let (octets, time) = DATA_LENGTH;
    if let Err(err) = raw.update_data_length(stack, octets, time).await {
        shared.log(format_args!("Bluetooth data length not requested: {err:?}"));
    }
    if let Some((min, max)) = CONN_INTERVAL_US {
        let params = RequestedConnParams {
            min_connection_interval: Duration::from_micros(min),
            max_connection_interval: Duration::from_micros(max),
            max_latency: 0,
            supervision_timeout: SUPERVISION_TIMEOUT,
            ..Default::default()
        };
        if let Err(err) = raw.update_connection_params(stack, &params).await {
            shared.log(format_args!("Bluetooth interval {min}-{max} us not requested: {err:?}"));
        }
    }
    select4(
        events(conn, server, &session, shared),
        notify(conn, server, shared),
        deadline(conn, &session, shared),
        revoke(conn, &session, shared),
    )
    .await;
}

/// Disconnect the central once the P4's bonds stop holding its bond, as after a `ClearBonds`, so
/// a host that forgot the bridge is not served on the bond TrouBLE still holds. The bond counts
/// only once the P4's list has held it ([`Session`]'s `listed`): a central that has just paired
/// holds its bond in TrouBLE before the P4 echoes it back, and a `Down::Bonds` sent in that gap
/// lacks it. A central whose identity matches no record is never disconnected for its bond.
///
/// A phone reading the WardriveESP stream is disconnected, and not counted, once a `Down::Bonds`
/// says phones are not admitted. [`events`] does the same when an event recomputes the stream
/// first.
async fn revoke(conn: &Conn<'_, '_>, session: &Session, shared: &Shared) -> ! {
    loop {
        let addr = bond_addr(&conn.raw().peer_identity());
        if shared.ble.bonds.borrow().iter().any(|kept| kept.addr == addr) {
            session.listed.set(true);
        } else if session.listed.get() {
            kick(conn, session, shared, "its bond was cleared");
            core::future::pending::<()>().await;
        }
        if !shared.ble.admitted() && shared.ble.stream() == Stream::Phone {
            drop_phone(conn, session, shared);
            core::future::pending::<()>().await;
        }
        shared.ble.bonds_changed.wait().await;
    }
}

/// Disconnect a central that is neither bonded and encrypted nor reading the WardriveESP stream
/// [`PAIR_DEADLINE`] or [`UNBONDABLE_DEADLINE`] after it connected, and one that reads no stream
/// for [`IDLE_DEADLINE`] after that, so neither an unbonded nor an idle one holds the only
/// connection. A central reading a stream is never idle, a USB host holding the bridge while it
/// waits included. The idle clock runs from `idle_since`, which pairing and letting go of a
/// stream restart ([`Session`]).
async fn deadline(conn: &Conn<'_, '_>, session: &Session, shared: &Shared) -> ! {
    let limit = if session.bondable { PAIR_DEADLINE } else { UNBONDABLE_DEADLINE };
    Timer::at(session.connected_at + limit).await;
    if !authorized(conn) && shared.ble.stream() != Stream::Phone {
        kick(conn, session, shared, "not bonded and encrypted in time after connecting");
        core::future::pending::<()>().await;
    }
    loop {
        if shared.ble.stream() != Stream::None {
            session.idle_changed.wait().await;
            continue;
        }
        let until = session.idle_since.get() + IDLE_DEADLINE;
        if Instant::now() >= until {
            disconnect(conn, session, shared, "not subscribed in time");
            core::future::pending::<()>().await;
        }
        select(Timer::at(until), session.idle_changed.wait()).await;
    }
}

/// Handle the connection's events until it ends.
async fn events(conn: &Conn<'_, '_>, server: &Server<'_>, session: &Session, shared: &Shared) {
    let ble = &shared.ble;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { reason } => {
                let reason = reason.into_inner();
                ble.count(|c| c.last_disconnect = reason);
                let peer = mac::short(&session.peer);
                shared.log(format_args!("Bluetooth disconnected {peer}: reason {reason:#04x}"));
                return;
            }
            GattConnectionEvent::PhyUpdated { tx_phy, .. } => {
                ble.count(|c| c.phy = phy_number(tx_phy));
            }
            GattConnectionEvent::ConnectionParamsUpdated { conn_interval, .. } => {
                let us = conn_interval.as_micros() as u32;
                ble.count(|c| c.interval_us = us);
            }
            // Never raised: without trouble-host's `connection-params-update` the controller
            // keeps the remote parameter request event masked and answers a central's
            // LL_CONNECTION_PARAM_REQ itself. Accepting it through bt-hci 0.10.1 panics: its
            // LE Remote Connection Parameter Request Reply declares no return, and the
            // controller returns the connection handle.
            GattConnectionEvent::RequestConnectionParams(_) => {}
            GattConnectionEvent::PairingComplete { security_level, bond } => {
                paired(conn, session, shared, security_level, bond.as_ref());
            }
            GattConnectionEvent::PairingFailed(err) => {
                ble.count(|c| c.failed_pairings = c.failed_pairings.wrapping_add(1));
                let peer = mac::short(&session.peer);
                shared.log(format_args!("Bluetooth pairing with {peer} failed: {err:?}"));
            }
            GattConnectionEvent::BondLost => {
                if session.bondable && ble.window_open() {
                    session.lost.set(Some(bond_addr(&conn.raw().peer_identity())));
                    let peer = mac::short(&session.peer);
                    shared.log(format_args!("Bluetooth {peer} lost its bond and pairs again"));
                } else {
                    kick(conn, session, shared, "lost its bond outside the pairing window");
                }
            }
            // A stored bond resumed: what the host stream waits for.
            GattConnectionEvent::Encrypted { bond: Some(bond), .. } if bond.is_bonded => {}
            // A fresh pairing's: its `PairingComplete` says whether it bonded.
            GattConnectionEvent::Encrypted { .. } if !session.bondable => {
                kick(conn, session, shared, "encrypted without a bond outside the pairing window");
            }
            GattConnectionEvent::Gatt { event } => {
                // The table refused these unencrypted already; an encrypted link without a bond
                // is refused here.
                let refused = AttErrorCode::INSUFFICIENT_AUTHENTICATION;
                // A central reads one stream: a subscription to the other is refused.
                let taken = AttErrorCode::WRITE_REQUEST_REJECTED;
                let up_cccd = server.link.up.cccd_handle;
                let tx_cccd = server.wardrive_esp.tx.cccd_handle;
                let phone = [server.wardrive_esp.rx.handle, server.wardrive_esp.ctrl.handle];
                // Down's writes skip the attribute table, which would store each one.
                let reply = match event {
                    GattEvent::Write(write) if write.handle() == server.link.down.handle => {
                        // TrouBLE answers a rejected write without response too, so only a
                        // request is ever rejected for a dropped write.
                        let request = matches!(write.payload().incoming(), AttClient::Request(_));
                        let len = write.with_data(|_, data| data.len());
                        let send = || write.with_data(|_, data| shared.push(&Up::HostBytes(data)));
                        if !authorized(conn) {
                            write.reject(refused)
                        } else if forward(shared, len, send).await || !request {
                            write.accept_unprocessed()
                        } else {
                            write.reject(AttErrorCode::INSUFFICIENT_RESOURCES)
                        }
                    }
                    GattEvent::Write(write)
                        if Some(write.handle()) == up_cccd && !authorized(conn) =>
                    {
                        write.reject(refused)
                    }
                    GattEvent::Write(write)
                        if Some(write.handle()) == up_cccd
                            && enables_notify(&write)
                            && ble.stream() == Stream::Phone =>
                    {
                        write.reject(taken)
                    }
                    GattEvent::Write(write)
                        if Some(write.handle()) == tx_cccd
                            && enables_notify(&write)
                            && (!ble.admitted() || ble.stream() == Stream::Host) =>
                    {
                        write.reject(taken)
                    }
                    GattEvent::Write(write) if phone.contains(&write.handle()) => {
                        phone_write(shared, write).await
                    }
                    event => event.accept(),
                };
                // A reply that would not build is for a link already going.
                if let Ok(reply) = reply {
                    reply.send().await;
                }
            }
            _ => {}
        }
        // A bonded central's subscription outlives its connection in TrouBLE, so it is read
        // only once the link is authorized again.
        let raw = conn.raw();
        let encrypted = raw.security_level().is_ok_and(|level| level.encrypted());
        let bonded = raw.is_bonded_peer();
        let stream = if encrypted && bonded && server.link.up.should_notify(conn) {
            Stream::Host
        } else if ble.admitted() && server.wardrive_esp.tx.should_notify(conn) {
            Stream::Phone
        } else {
            Stream::None
        };
        let mtu = raw.att_mtu();
        let was = ble.stream();
        set_state(shared, |state| {
            state.stream = stream;
            state.mtu = mtu;
            state.encrypted = encrypted;
            state.bonded = bonded;
        });
        if stream != was {
            match stream {
                Stream::None => session.idle_since.set(Instant::now()),
                Stream::Phone => {
                    let peer = mac::short(&session.peer);
                    shared.log(format_args!("Bluetooth phone linked {peer}"));
                }
                Stream::Host => {}
            }
            session.idle_changed.signal(());
        }
        // Admission ended and an event came before `revoke` looked: the phone still reads TX, so
        // it is not left to the idle rule.
        if was == Stream::Phone
            && stream == Stream::None
            && !ble.admitted()
            && server.wardrive_esp.tx.should_notify(conn)
        {
            drop_phone(conn, session, shared);
        }
    }
}

/// A pairing completed: report a bond to the P4, or disconnect a central that bonded nothing
/// while the window is closed.
fn paired(
    conn: &Conn<'_, '_>,
    session: &Session,
    shared: &Shared,
    level: SecurityLevel,
    bond: Option<&BondInformation>,
) {
    let ms = session.connected_at.elapsed().as_millis();
    let peer = mac::short(&session.peer);
    let Some(bond) = bond else {
        shared.log(format_args!("Bluetooth paired {peer} without a bond in {ms} ms, {level:?}"));
        if !shared.ble.window_open() {
            kick(conn, session, shared, "paired without a bond outside the pairing window");
        }
        return;
    };
    shared.ble.count(|c| c.pairings = c.pairings.wrapping_add(1));
    // A new bond, which the P4's list holds only once it echoes it back.
    session.listed.set(false);
    // The idle clock starts afresh at pairing, which can take a person most of `PAIR_DEADLINE`.
    session.idle_since.set(Instant::now());
    session.idle_changed.signal(());
    if let Some(old) = session.lost.take() {
        shared.push(&Up::BondRemoved(old));
    }
    let record = bond_record(bond);
    shared.push(&Up::BondAdded(record));
    shared.ble.pending_bond.set(Some(PendingBond { record, sent_at: Instant::now(), sent: 1 }));
    shared.log(format_args!("Bluetooth paired and bonded {peer} in {ms} ms, {level:?}"));
}

/// Send the record carrying `len` bytes a central wrote to the P4 with `send`, waiting for room
/// beside [`UP_RESERVE_BYTES`] rather than evicting, and return whether it went: `false` for one
/// dropped after [`FORWARD_WAIT`]. The write is held meanwhile, so the next waits behind it.
///
/// Only the link's writer makes room, by taking records off the queue for a packet, and it
/// raises `up_freed` each time, so the wait is that signal rather than a poll.
async fn forward(shared: &Shared, len: usize, send: impl FnOnce()) -> bool {
    let deadline = Instant::now() + FORWARD_WAIT;
    while shared.up_room() < len + FORWARD_OVERHEAD + UP_RESERVE_BYTES {
        if let Either::Second(()) = select(shared.up_freed.wait(), Timer::at(deadline)).await {
            shared.ble.count(|c| c.dropped_writes = c.dropped_writes.wrapping_add(len as u32));
            return false;
        }
    }
    // A write is at most `VALUE_MAX`, well inside one record.
    send();
    true
}

/// Whether a write to a CCCD turns notifications on: its value is two bytes LE, bit 0 notify.
fn enables_notify(write: &WriteEvent<'_, '_, DefaultPacketPool>) -> bool {
    write.with_data(|_, data| data.first().is_some_and(|flags| flags & 1 != 0))
}

/// Answer a phone's write to RX or CTRL. Never refused with Insufficient Authentication, the
/// answer that makes a phone pair.
///
/// Before phones are admitted a request is refused and a write without response dropped. While
/// the central reads no WardriveESP stream a write is dropped. Else its ACK is owed, and its
/// command goes to the P4 as `Up::WardriveEspCommand`, as Down's writes do.
async fn phone_write<'s>(
    shared: &Shared,
    write: WriteEvent<'s, '_, DefaultPacketPool>,
) -> Result<Reply<'s, DefaultPacketPool>, Error> {
    let ble = &shared.ble;
    let request = matches!(write.payload().incoming(), AttClient::Request(_));
    if !ble.admitted() {
        return if request {
            write.reject(AttErrorCode::WRITE_NOT_PERMITTED)
        } else {
            write.accept_unprocessed()
        };
    }
    if ble.stream() != Stream::Phone {
        return write.accept_unprocessed();
    }
    let mut text = [0; VALUE_MAX];
    let len = write.with_data(|_, data| take_phone_write(ble, data, &mut text));
    if len == 0 {
        return write.accept_unprocessed();
    }
    let command = &text[..len];
    if forward(shared, len, || shared.push(&Up::WardriveEspCommand(command))).await || !request {
        write.accept_unprocessed()
    } else {
        write.reject(AttErrorCode::INSUFFICIENT_RESOURCES)
    }
}

/// Read a phone's write: owe its ACK, and copy its command, trimmed, into `out`. Returns the
/// command's length, 0 for none.
fn take_phone_write(ble: &Ble, data: &[u8], out: &mut [u8; VALUE_MAX]) -> usize {
    let mut inbound = ble.inbound.get();
    let taken = inbound.on_write(data);
    ble.inbound.set(inbound);
    // A CMD sets the reply id to its own. A full queue drops the newest: the phone has written
    // more than TrouBLE queues without an answer.
    if taken.ack.is_some() {
        let _ = ble.acks.borrow_mut().push_back(inbound.reply_id());
        ble.ready.signal(());
    }
    let text = taken.command.map_or(&[][..], command::trim);
    let len = text.len().min(VALUE_MAX);
    out[..len].copy_from_slice(&text[..len]);
    len
}

/// Notify the ring to the central while it reads a stream, a notification at a time: on Up as
/// it comes, or on TX in frames, each owed ACK first.
async fn notify(conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) -> ! {
    let ble = &shared.ble;
    let mut chunk = [0; VALUE_MAX];
    loop {
        let stream = ble.stream();
        let idle = match stream {
            Stream::None => true,
            Stream::Host => ble.outbox.borrow().is_empty(),
            Stream::Phone => ble.outbox.borrow().is_empty() && ble.acks.borrow().is_empty(),
        };
        if idle {
            ble.ready.wait().await;
            continue;
        }
        // An MTU exchange raises no event, so the notifier is where a change shows.
        let mtu = conn.raw().att_mtu();
        set_state(shared, |state| state.mtu = mtu);
        let (characteristic, n) = if stream == Stream::Phone {
            (&server.wardrive_esp.tx, phone_chunk(ble, mtu, &mut chunk))
        } else {
            let room = usize::from(mtu.saturating_sub(3)).min(VALUE_MAX);
            (&server.link.up, ble.outbox.borrow_mut().take(&mut chunk[..room]))
        };
        // TrouBLE frees its packets inside its runner and offers nothing to wait on, so a full
        // pool is waited out with a back-off.
        let mut wait = POOL_RETRY;
        while let Err(Error::OutOfMemory) =
            characteristic.notify_raw(conn, &chunk[..n], false).await
        {
            Timer::after(wait).await;
            wait = (wait * 2).min(POOL_RETRY_MAX);
        }
        // Bytes a failed notification carried are credited too: they are off the link either way.
        let empty = ble.outbox.borrow().is_empty();
        credit(shared, empty);
        // A send TrouBLE's queue had room for never waited, so the link gets its turn here.
        yield_now().await;
    }
}

/// Build the phone's next notification in `chunk` and return its length: an owed ACK, else one
/// DATA frame of the ring's bytes through the next `\n`, so a frame carries one line, or part of
/// one longer than the frame's room.
fn phone_chunk(ble: &Ble, mtu: u16, chunk: &mut [u8; VALUE_MAX]) -> usize {
    if let Some(id) = ble.acks.borrow_mut().pop_front() {
        chunk[..frame::HEADER_LEN].copy_from_slice(&frame::ack(id));
        return frame::HEADER_LEN;
    }
    let room = frame::data_room(mtu).min(VALUE_MAX - frame::HEADER_LEN);
    let (header, body) = chunk.split_at_mut(frame::HEADER_LEN);
    let n = ble.outbox.borrow_mut().take_through(&mut body[..room], b'\n');
    // At most `VALUE_MAX`, well inside a `u16`.
    header.copy_from_slice(&frame::data_header(ble.inbound.get().reply_id(), n as u16));
    frame::HEADER_LEN + n
}

/// `BleCounters`' number for a PHY.
fn phy_number(phy: PhyKind) -> u8 {
    match phy {
        PhyKind::Le1M => 1,
        PhyKind::Le2M => 2,
        PhyKind::LeCoded | PhyKind::LeCodedS2 => 3,
    }
}
