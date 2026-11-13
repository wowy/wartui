//! The host link over Bluetooth: a GATT peripheral that carries the P4's host stream to one
//! bonded central and the central's frames back.
//!
//! TrouBLE is the host, over `esp-radio`'s controller, on the link's executor: its futures are
//! joined beside the link's in the one task, so each `.await` here hands the C6 link its turn.
//! The service, its characteristics and the advertised name are `wartui_proto::ble`'s.
//!
//! **Up notifies the stream; Down takes the central's writes.** Up carries what the P4 sends as
//! `Down::HostBytes`, cut at the ATT MTU less 3 wherever that falls. Down forwards each write
//! to the P4 as `Up::HostBytes`, in order. A write waits for room in the queue to the P4 rather
//! than evicting what it holds, and one that waits past [`FORWARD_WAIT`] is dropped and counted
//! in `dropped_writes`, so the queue stays the only place a write is lost. A write request is
//! answered only once its write is forwarded, behind every write queued ahead of it, which is
//! what paces a central that ends each frame with one (`wartui_bridge::ble`). A request whose
//! write was dropped is refused with Insufficient Resources rather than acknowledged, so the
//! central learns its frame is gone: wartui's transport ends the connection on a failed request,
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
//! subscription clears the ring. It is coalesced to one `Up::Credit` per [`CREDIT_EVERY_BYTES`],
//! per [`CREDIT_EVERY`], or when the ring empties, whichever comes first, and nothing is sent
//! before the P4 has named a first offset. With no subscribed central the stream is discarded and
//! credited at once.
//!
//! - `Down::HostStart` begins an attachment: the ring is cleared and the position is `at`, so the
//!   central reads only bytes from that offset on. One resync `0x00` goes in first. A
//!   notification [`notify`] took from the ring before the clear is still sent, and its partial
//!   frame would run into this attachment's `Ready`; the `0x00` ends it, so the `Ready` decodes
//!   whole. It is not a stream byte either, and is never credited.
//! - A gap before a `Down::HostBytes` is counted in `resynced_bytes`, and the position is past it.
//!   One `0x00` goes into the ring ahead of the bytes, so the central's COBS reader drops the torn
//!   frame rather than merging it with the next. That byte is not a stream byte, so the position
//!   never counts it. The ring holds [`HOST_WINDOW_BYTES`] and [`RESYNC_MAX`] of them; a gap with
//!   no room left gets none, and the central resynchronizes at the next terminator, one frame
//!   later.
//!
//! **A subscription is the CCCD on Up.** Every change of it, the encryption, the bond or the MTU
//! sends a `BleState`; the P4 reads a subscribed one as Bluetooth taking the host queue. The ring
//! is cleared on every change of subscription, which moves the credit past what it held, so a new
//! central never reads the last one's stream.
//!
//! **The C6 asks for the 2M PHY and data length 251 once connected, and an interval only when
//! the build names one** ([`CONN_INTERVAL_US`]). TrouBLE reports only whether a request went out:
//! an accepted one shows in `BleCounters`' `phy` and `interval_us`, and a refusal as no change.
//!
//! **Two upstream bugs shape this build** (`docs/c6-ble-host-findings.md`, "Workarounds a
//! production build needs"). The packet pool is capped at 251 bytes, so the ATT MTU is
//! [`ATT_MTU_MAX`] and no notification fragments (`Cargo.toml`). `trouble-host`'s
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
//! **A central the C6 holds a bond for gets no Security Request.** Outside the window the request
//! would say no bonding, and BlueZ 5.88 answers that by pairing again rather than encrypting with
//! the key it holds, and is then disconnected as unbonded (`docs/c6-ble-pairing-findings.md`,
//! "Two faults the bench found"). It encrypts on its own, or on the Insufficient Authentication
//! answer.
//!
//! **TrouBLE accepts every pairing, so the window is the C6's to enforce.** A connection is
//! bondable only when it is made inside the P4's pairing window. These centrals are disconnected
//! and counted in `unbonded_kicks`, so none holds the only connection without a bond:
//!
//! - one encrypted without a stored bond on a connection made outside the window;
//! - one that lost its bond, outside the window;
//! - one not bonded and encrypted [`PAIR_DEADLINE`] after connecting inside the window, or
//!   [`UNBONDABLE_DEADLINE`] after connecting outside it.
//!
//! **The P4 keeps the bonds; the C6 holds them in TrouBLE's RAM** (`wartui_proto::bonds`). It
//! waits up to [`BONDS_WAIT`] for the P4's first `Down::Bonds` before it advertises, so the
//! resolving list holds them from the first advertisement, and applies each later one whenever
//! no central is connected ([`sync_bonds`]). Removing the connected central's bond waits for it
//! to go. A new bond goes up as `Up::BondAdded`, after an `Up::BondRemoved` when it replaces one a
//! central lost. The window's end is tracked here from each `Down::Bonds`' `window_ms`.
//!
//! **The first advertisement ends the image's trial** with the P4's hello (`src/link.rs`):
//! [`advertise`] raises `advertising_started` once the controller takes it.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use embassy_futures::join::join3;
use embassy_futures::select::{Either, select, select3};
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::BT;
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use trouble_host::att::AttClient;
use trouble_host::prelude::*;
use wartui_proto::ble::{ByteRing, DOWN_UUID, NAME_PREFIX, SERVICE_UUID, UP_UUID};
use wartui_proto::bonds::{BondAddr, BondLevel, BondRecord, Bonds};
use wartui_proto::heapless::Deque;
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::{Accept, BleCounters, BleState, HOST_WINDOW_BYTES, Inflow, Up};

use crate::config::CONN_INTERVAL_US;
use crate::link::Shared;

/// Resync `0x00`s the ring holds beside the window's stream bytes.
const RESYNC_MAX: usize = 16;

/// The ring's bytes: the most the P4 keeps in flight, and room for the resync `0x00`s.
const OUTBOX_BYTES: usize = HOST_WINDOW_BYTES as usize + RESYNC_MAX;

/// The largest ATT MTU: the packet pool's 251 bytes less the L2CAP header. The pool is capped
/// there (`Cargo.toml`, `default-packet-pool-mtu-251`) as the workaround for esp-radio
/// 1.0.0-beta.1 marking every ACL fragment as a start, so a notification is never fragmented.
const ATT_MTU_MAX: usize = 247;

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

/// How long a changed bond list gives TrouBLE's runner to load the resolving list, while
/// advertising is idle, before the next advertisement.
const RESOLVING_SETTLE: Duration = Duration::from_millis(50);

/// How long a central that connected inside the pairing window has to be bonded and encrypted
/// before it is disconnected. Pairing waits on a person tapping the phone's prompt, which took
/// more than 10 s on the Pixel 8 (inferred); 30 s is the pairing protocol's own timeout.
const PAIR_DEADLINE: Duration = Duration::from_secs(30);

/// The same, for a central that connected outside the window: it can only resume a bond, which
/// took under 0.7 s on every host, so it gets no time to wait on a person.
const UNBONDABLE_DEADLINE: Duration = Duration::from_secs(10);

/// `wartui XX:YY`.
const NAME_MAX: usize = NAME_PREFIX.len() + 5;

/// How long one advertising interval is.
const ADVERTISE_EVERY: Duration = Duration::from_millis(100);

type Controller<'a> = ExternalController<BleConnector<'a>, COMMAND_SLOTS>;
type BleStack<'a> = Stack<'a, Controller<'a>, DefaultPacketPool>;
type Conn<'a, 'b> = GattConnection<'a, 'b, DefaultPacketPool>;

#[gatt_server(connections_max = CONNECTIONS, mutex_type = NoopRawMutex)]
struct Server {
    link: LinkService,
}

/// The "wartui link" service.
#[gatt_service(uuid = SERVICE_UUID)]
struct LinkService {
    /// The bridge's frame stream, notified. Its stored value is never read. Subscribing needs
    /// an encrypted link.
    #[characteristic(uuid = UP_UUID, notify, permissions(encrypted))]
    up: u8,
    /// The host's frames, written. Writes are taken unprocessed, so nothing is stored. Writing
    /// needs an encrypted link.
    #[characteristic(uuid = DOWN_UUID, write, write_without_response, permissions(encrypted))]
    down: u8,
}

/// The host stream waiting to be notified, and where the resync `0x00`s in it sit, so the credit
/// counts only its stream bytes.
pub struct Outbox {
    ring: ByteRing<OUTBOX_BYTES>,
    /// Each resync `0x00` the ring holds, oldest first, as the count of bytes pushed before it.
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

    /// Append one resync `0x00`, or return `false` when there is no room for it.
    fn push_resync(&mut self) -> bool {
        if self.resyncs.is_full() || self.ring.push(&[0]) == 0 {
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
        while self.resyncs.front().is_some_and(|at| at.wrapping_sub(self.taken) < n as u32) {
            self.resyncs.pop_front();
        }
        self.taken = self.taken.wrapping_add(n as u32);
        n
    }

    /// Discard everything held.
    fn clear(&mut self) {
        self.ring.clear();
        self.resyncs.clear();
        self.taken = self.pushed;
    }

    /// The stream bytes held: every byte but the resync `0x00`s.
    fn stream_held(&self) -> usize {
        self.ring.len() - self.resyncs.len()
    }

    fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }
}

/// The Bluetooth side's state, as the link and the stack see it.
pub struct Ble {
    outbox: &'static RefCell<Outbox>,
    /// The next stream offset expected from the P4.
    inflow: Cell<Inflow>,
    /// The link as the C6 sees it, and as it last told the P4.
    state: Cell<BleState>,
    said: Cell<Option<BleState>>,
    /// A central is connected.
    connected: Cell<bool>,
    /// The controller has taken an advertisement since boot.
    advertising: Cell<bool>,
    /// Raised when `advertising` first holds, for the link's trial.
    pub advertising_started: Signal<NoopRawMutex, ()>,
    /// The `Up::Credit` last sent, or `None` before the first, and when.
    credited: Cell<Option<u32>>,
    credited_at: Cell<Instant>,
    /// The counters kept here; [`counters`] adds the heap's.
    counters: Cell<BleCounters>,
    /// Raised when the ring or the subscription changes, for the notifier.
    ready: Signal<NoopRawMutex, ()>,
    /// The bonds the P4 last sent, oldest first, and whether any `Down::Bonds` has arrived.
    bonds: RefCell<Bonds>,
    bonds_heard: Cell<bool>,
    /// Raised by each `Down::Bonds`, for [`follow_bonds`].
    bonds_changed: Signal<NoopRawMutex, ()>,
    /// When the P4's pairing window closes, or `None` while it is closed.
    window_until: Cell<Option<Instant>>,
}

impl Ble {
    pub fn new(outbox: &'static RefCell<Outbox>) -> Self {
        Self {
            outbox,
            inflow: Cell::new(Inflow::new()),
            state: Cell::new(BleState::default()),
            said: Cell::new(None),
            connected: Cell::new(false),
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
        }
    }

    fn count(&self, change: impl FnOnce(&mut BleCounters)) {
        let mut counters = self.counters.get();
        change(&mut counters);
        self.counters.set(counters);
    }

    /// Whether an authorized central has subscribed to Up.
    fn subscribed(&self) -> bool {
        self.state.get().subscribed
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

/// A central took the host queue, and its first byte is at `at`: clear the ring, lead it with a
/// resync `0x00`, and credit up to `at`.
pub fn host_start(shared: &Shared, at: u32) {
    let ble = &shared.ble;
    let mut inflow = ble.inflow.get();
    // What never arrived before `at` is behind the position, which is all the credit needs.
    let _ = inflow.start(at);
    ble.inflow.set(inflow);
    let mut outbox = ble.outbox.borrow_mut();
    outbox.clear();
    // A cleared ring has room for it.
    outbox.push_resync();
    drop(outbox);
    credit(shared, true);
    ble.ready.signal(());
}

/// Take the host stream's next bytes, at stream offset `at`: held for the central, or discarded
/// and credited when none is subscribed.
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
    if !ble.subscribed() {
        // Nothing is held, so the credit is the expected offset.
        credit(shared, true);
        return;
    }
    let mut outbox = ble.outbox.borrow_mut();
    // Without room, the central resynchronizes at the next terminator instead.
    if gap > 0 {
        outbox.push_resync();
    }
    let lost = bytes.len() - outbox.push(bytes);
    drop(outbox);
    // Bytes with no room are behind the position, done with: credit them at once.
    credit(shared, lost > 0);
    ble.ready.signal(());
}

/// Take the P4's bonds and its pairing window's time left. [`follow_bonds`] gives TrouBLE the
/// bonds once no central is connected.
pub fn take_bonds(shared: &Shared, bonds: Bonds, window_ms: u32) {
    let ble = &shared.ble;
    let until = (window_ms > 0).then(|| Instant::now() + Duration::from_millis(window_ms.into()));
    ble.window_until.set(until);
    ble.bonds.replace(bonds);
    ble.bonds_heard.set(true);
    ble.bonds_changed.signal(());
}

/// The counters `RadioStatus.ble` reports.
pub fn counters(shared: &Shared) -> BleCounters {
    let mut counters = shared.ble.counters.get();
    counters.heap_high_water = esp_alloc::HEAP.stats().max_usage as u32;
    counters
}

/// Send the credit, the expected offset less the stream bytes held, when it has moved and is due
/// or `now`. Nothing goes before the P4 has named a first offset.
fn credit(shared: &Shared, now: bool) {
    let ble = &shared.ble;
    let Some(expected) = ble.inflow.get().expected() else { return };
    let accounted = expected.wrapping_sub(ble.outbox.borrow().stream_held() as u32);
    // A position the P4 has never heard is owed in full.
    let owed = ble.credited.get().map_or(u32::MAX, |credited| accounted.wrapping_sub(credited));
    let due = now || owed >= CREDIT_EVERY_BYTES || ble.credited_at.get().elapsed() >= CREDIT_EVERY;
    if owed > 0 && due {
        ble.credited.set(Some(accounted));
        ble.credited_at.set(Instant::now());
        shared.push(&Up::Credit { accounted });
    }
}

/// Set the link's state, and tell the P4 if it changed. A change of subscription clears the
/// ring.
fn set_state(shared: &Shared, change: impl FnOnce(&mut BleState)) {
    let ble = &shared.ble;
    let mut state = ble.state.get();
    let was = state.subscribed;
    change(&mut state);
    ble.state.set(state);
    if state.subscribed != was {
        ble.outbox.borrow_mut().clear();
        credit(shared, true);
        ble.ready.signal(());
    }
    if ble.said.get() != Some(state) {
        ble.said.set(Some(state));
        shared.push(&Up::BleState(state));
    }
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

    join3(
        run_host(&mut runner),
        advertise(&stack, &mut peripheral, mac, shared),
        follow_bonds(&stack, shared),
    )
    .await
    .0
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

    let mut adv_data = [0; 31];
    let mut scan_data = [0; 31];
    let service = SERVICE_UUID.to_le_bytes();
    let encoded = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(name.as_bytes()),
        ],
        &mut adv_data,
    )
    .and_then(|adv| {
        let scan = AdStructure::encode_slice(
            &[AdStructure::CompleteServiceUuids128(&[service])],
            &mut scan_data,
        )?;
        Ok((adv, scan))
    });
    let Ok((adv_len, scan_len)) = encoded else {
        shared.error(format_args!("the advertisement would not encode"));
        core::future::pending().await
    };
    let params = AdvertisementParameters {
        interval_min: ADVERTISE_EVERY,
        interval_max: ADVERTISE_EVERY,
        ..Default::default()
    };

    // Polled: `bonds_changed` is `follow_bonds`' to wait on, and this runs once, at boot.
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
    shared.log(format_args!(
        "Bluetooth advertises `{name}` with {held} bonds, pairing window {window}"
    ));
    if sync_bonds(stack, shared) {
        Timer::after(RESOLVING_SETTLE).await;
    }

    loop {
        let advertisement = Advertisement::ConnectableScannableUndirected {
            adv_data: &adv_data[..adv_len],
            scan_data: &scan_data[..scan_len],
        };
        let conn = match peripheral.advertise(&params, advertisement).await {
            Ok(advertiser) => {
                if !shared.ble.advertising.replace(true) {
                    shared.ble.advertising_started.signal(());
                }
                advertiser.accept().await.map_err(BleHostError::BleHost)
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
        shared.ble.connected.set(true);
        serve(stack, &conn, server, shared).await;
        shared.ble.connected.set(false);
        shared.ble.count(|c| {
            c.phy = 0;
            c.interval_us = 0;
        });
        set_state(shared, |state| *state = BleState::default());
        // The bonds `follow_bonds` held back while the central was connected.
        if sync_bonds(stack, shared) {
            Timer::after(RESOLVING_SETTLE).await;
        }
    }
}

/// Give TrouBLE each bond the P4 sent whenever no central is connected. While one is, the
/// advertiser applies them once it goes.
async fn follow_bonds(stack: &BleStack<'_>, shared: &Shared) -> ! {
    loop {
        shared.ble.bonds_changed.wait().await;
        if !shared.ble.connected.get() {
            sync_bonds(stack, shared);
        }
    }
}

/// Make TrouBLE's bonds the P4's: drop each it holds that the P4 does not, and add each the P4
/// holds that it lacks or holds with another key. Returns whether anything changed.
fn sync_bonds(stack: &BleStack<'_>, shared: &Shared) -> bool {
    let want = shared.ble.bonds.borrow().clone();
    let mut gone: wartui_proto::heapless::Vec<Identity, BONDS> = wartui_proto::heapless::Vec::new();
    stack.with_bond_information(|held| {
        for bond in held.iter().filter(|bond| bond.is_bonded) {
            let addr = bond_addr(&bond.identity);
            if !want.iter().any(|kept| kept.addr == addr) {
                let _ = gone.push(bond.identity);
            }
        }
    });
    let mut changed = !gone.is_empty();
    for identity in gone {
        let _ = stack.remove_bond_information(identity);
    }
    for record in &want {
        let info = bond_information(record);
        let held = stack.with_bond_information(|held| {
            held.iter().any(|bond| {
                bond.is_bonded && bond.identity.addr == info.identity.addr && bond.ltk == info.ltk
            })
        });
        if held {
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
    /// The central's address, in display order, for the log.
    peer: Mac,
}

/// Whether the link is encrypted and the central holds a stored bond: what the host stream needs.
fn authorized(conn: &Conn<'_, '_>) -> bool {
    let raw = conn.raw();
    raw.security_level().is_ok_and(|level| level.encrypted()) && raw.is_bonded_peer()
}

/// Disconnect a central that holds no bond, and say why.
fn kick(conn: &Conn<'_, '_>, session: &Session, shared: &Shared, why: &str) {
    conn.raw().disconnect();
    shared.ble.count(|c| c.unbonded_kicks = c.unbonded_kicks.wrapping_add(1));
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
        *state = BleState { subscribed: false, encrypted: false, bonded: false, mtu: raw.att_mtu() }
    });
    let session = Session {
        connected_at: Instant::now(),
        bondable: shared.ble.window_open(),
        lost: Cell::new(None),
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
    if !raw.is_bonded_peer()
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
    select3(
        events(conn, server, &session, shared),
        notify(conn, server, shared),
        deadline(conn, &session, shared),
    )
    .await;
}

/// Disconnect a central not bonded and encrypted [`PAIR_DEADLINE`] or [`UNBONDABLE_DEADLINE`]
/// after it connected, so an idle or unbonded one cannot hold the only connection.
async fn deadline(conn: &Conn<'_, '_>, session: &Session, shared: &Shared) -> ! {
    let limit = if session.bondable { PAIR_DEADLINE } else { UNBONDABLE_DEADLINE };
    Timer::at(session.connected_at + limit).await;
    if !authorized(conn) {
        kick(conn, session, shared, "not bonded and encrypted in time after connecting");
    }
    core::future::pending().await
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
                let up_cccd = server.link.up.cccd_handle;
                // Down's writes skip the attribute table, which would store each one.
                let reply = match event {
                    GattEvent::Write(write) if write.handle() == server.link.down.handle => {
                        // TrouBLE answers a rejected write without response too, so only a
                        // request is ever rejected for a dropped write.
                        let request = matches!(write.payload().incoming(), AttClient::Request(_));
                        if !authorized(conn) {
                            write.reject(refused)
                        } else if forward(shared, &write).await || !request {
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
        let subscribed = encrypted && bonded && server.link.up.should_notify(conn);
        let mtu = raw.att_mtu();
        set_state(shared, |state| {
            state.subscribed = subscribed;
            state.mtu = mtu;
            state.encrypted = encrypted;
            state.bonded = bonded;
        });
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
    if let Some(old) = session.lost.take() {
        shared.push(&Up::BondRemoved(old));
    }
    shared.push(&Up::BondAdded(bond_record(bond)));
    shared.log(format_args!("Bluetooth paired and bonded {peer} in {ms} ms, {level:?}"));
}

/// Send what a central wrote to the P4, waiting for room rather than evicting, and return
/// whether it went: `false` for a write dropped after [`FORWARD_WAIT`]. The write is held
/// meanwhile, so the next waits behind it.
///
/// Only the link's writer makes room, by taking records off the queue for a packet, and it
/// raises `up_freed` each time, so the wait is that signal rather than a poll.
async fn forward(shared: &Shared, write: &WriteEvent<'_, '_, DefaultPacketPool>) -> bool {
    let len = write.with_data(|_, data| data.len());
    let deadline = Instant::now() + FORWARD_WAIT;
    while shared.up_room() < len + FORWARD_OVERHEAD {
        if let Either::Second(()) = select(shared.up_freed.wait(), Timer::at(deadline)).await {
            shared.ble.count(|c| c.dropped_writes = c.dropped_writes.wrapping_add(len as u32));
            return false;
        }
    }
    // A write is at most `VALUE_MAX`, well inside one record.
    write.with_data(|_, data| shared.push(&Up::HostBytes(data)));
    true
}

/// Notify the ring to the central while it is subscribed, a notification at a time.
async fn notify(conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) -> ! {
    let ble = &shared.ble;
    let mut chunk = [0; VALUE_MAX];
    loop {
        if !ble.subscribed() || ble.outbox.borrow().is_empty() {
            ble.ready.wait().await;
            continue;
        }
        // An MTU exchange raises no event, so the notifier is where a change shows.
        let mtu = conn.raw().att_mtu();
        set_state(shared, |state| state.mtu = mtu);
        let room = usize::from(mtu.saturating_sub(3)).min(VALUE_MAX);
        let n = ble.outbox.borrow_mut().take(&mut chunk[..room]);
        // TrouBLE frees its packets inside its runner and offers nothing to wait on, so a full
        // pool is waited out with a back-off.
        let mut wait = POOL_RETRY;
        while let Err(Error::OutOfMemory) =
            server.link.up.notify_raw(conn, &chunk[..n], false).await
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

/// `BleCounters`' number for a PHY.
fn phy_number(phy: PhyKind) -> u8 {
    match phy {
        PhyKind::Le1M => 1,
        PhyKind::Le2M => 2,
        PhyKind::LeCoded | PhyKind::LeCodedS2 => 3,
    }
}
