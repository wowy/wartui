//! The host link over Bluetooth: a GATT peripheral that carries the P4's host stream to one
//! central and the central's frames back. Spike only.
//!
//! TrouBLE is the host, over `esp-radio`'s controller through [`crate::hci::Counting`], on the
//! link's executor: its futures are joined beside the link's in the one task, so each `.await`
//! here hands the C6 link its turn and nothing holds the CPU between them.
//!
//! **Up notifies the stream; Down takes the central's writes.** Up carries what the P4 sends as
//! `Down::HostBytes`, cut at the ATT MTU less 3 wherever that falls. Down forwards each write
//! to the P4 as `Up::HostBytes`, in order. A write waits for room in the queue to the P4 rather
//! than evicting what it holds, and one that waits past [`FORWARD_WAIT`] is dropped and
//! counted, so the queue stays the only place a write is lost.
//!
//! **Credit follows the hand-off to TrouBLE**, not delivery: the window exists to keep the C6
//! link from stalling behind RTS/CTS, and bytes TrouBLE holds are off that link. Credit is
//! coalesced to one `Up::BleCredit` per [`CREDIT_EVERY_BYTES`], per [`CREDIT_EVERY`], or when
//! the buffer empties, whichever comes first. With no subscriber the stream is discarded and
//! credited at once; the P4 sends none then, unless its subscriber has only just gone.
//!
//! **A subscription is the CCCD on Up.** Every change of it, the MTU, the PHY or the interval
//! sends a `BleState`; the P4 reads a subscribed one as Bluetooth taking the host queue. The
//! buffer is cleared, and credited, on every change of subscription, so a new central never
//! reads the last one's stream.
//!
//! **The interval is the central's unless the build asks.** With
//! `WARTUI_RADIO_CONN_INTERVAL_MS` set ([`CONN_INTERVAL_US`]), the C6 requests that range once
//! connected, by the link-layer procedure where its controller supports it and L2CAP otherwise.
//! TrouBLE reports only whether the request went out: an accepted one shows as the interval the
//! next `BleState` carries, and a refusal as no change.
//!
//! **The PHY is the build's.** The C6 requests `WARTUI_RADIO_PHY` ([`PHY`]) once connected, 2M
//! unless the build names another, and logs the request.
//!
//! **Each await reports where it is.** The notifier, the event loop, the advertiser and the
//! runner set a phase in `BleStats` around each await, and the transport counts its own
//! ([`crate::hci::Watch`]), so the counters of a stalled stack name the future that waits.
//! [`watch_stall`] logs those phases when the notifier or the transport has waited too long.

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use bt_hci::cmd::info::ReadBdAddr;
use embassy_futures::join::join3;
use embassy_futures::select::select;
use embassy_futures::yield_now;
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_hal::peripherals::BT;
use esp_radio::ble::Config;
use esp_radio::ble::controller::BleConnector;
use trouble_host::prelude::*;
use wartui_proto::mac::{self, Mac};
use wartui_proto::radio_link::{BleState, BleStats, Up};
use wartui_proto::spike::{ByteRing, DOWN_UUID, NAME_PREFIX, SERVICE_UUID, UP_UUID};

use crate::config::{BLE_WINDOW_BYTES, CONN_INTERVAL_US, PHY};
use crate::hci::{Counting, Ledger, Watch};
use crate::link::Shared;

/// The host stream waiting to be notified. Static: it is the window's size.
pub type TxRing = RefCell<ByteRing<BLE_WINDOW_BYTES>>;

/// The largest ATT MTU: what `.cargo/config.toml` sizes the packet pool for.
const ATT_MTU_MAX: usize = 517;

/// The most a notification or a write carries: the MTU less the ATT opcode and handle.
const VALUE_MAX: usize = ATT_MTU_MAX - 3;

/// Credit at least this often, in bytes, while the stream flows.
const CREDIT_EVERY_BYTES: u32 = 2048;

/// Credit at least this often while the stream flows.
const CREDIT_EVERY: Duration = Duration::from_millis(20);

/// How long a central's write waits for room in the queue to the P4.
const FORWARD_WAIT: Duration = Duration::from_secs(1);

/// How often a waiting write, or a notification the packet pool has no room for, looks again.
const RETRY: Duration = Duration::from_millis(1);

/// What a write to the queue to the P4 costs beyond its bytes: the ring's length prefix, the
/// record's variant and the bytes' length.
const FORWARD_OVERHEAD: usize = 2 + 1 + 2;

/// The supervision timeout a connection interval request asks for.
const SUPERVISION_TIMEOUT: Duration = Duration::from_secs(4);

/// HCI commands in flight at once.
const COMMAND_SLOTS: usize = 4;

/// One central at a time.
const CONNECTIONS: usize = 1;

/// The L2CAP channels a connection uses: signaling and ATT.
const L2CAP_CHANNELS: usize = 2;

/// `wartui XX:YY`.
const NAME_MAX: usize = NAME_PREFIX.len() + 5;

/// How long one advertising interval is.
const ADVERTISE_EVERY: Duration = Duration::from_millis(100);

/// How long the notifier waits on TrouBLE, or a transport read stays pending with ACL
/// outstanding, before [`watch_stall`] logs.
const STALL_AFTER: Duration = Duration::from_secs(3);

/// The least time between two of [`watch_stall`]'s lines.
const STALL_LOG_EVERY: Duration = Duration::from_secs(5);

/// How often [`watch_stall`] looks.
const STALL_POLL: Duration = Duration::from_millis(500);

/// `BleStats::notify_phase`, as `radio_link::NOTIFY_PHASES` names them.
const NOTIFY_READY: u8 = 0;
const NOTIFY_RAW: u8 = 1;
const NOTIFY_OOM: u8 = 2;
const NOTIFY_CREDIT: u8 = 3;

/// `BleStats::events_phase`, as `radio_link::EVENTS_PHASES` names them.
const EVENTS_NEXT: u8 = 0;
const EVENTS_FORWARD: u8 = 1;
const EVENTS_REPLY: u8 = 2;
const EVENTS_OTHER: u8 = 3;

/// `BleStats::adv_phase`, as `radio_link::ADV_PHASES` names them.
const ADV_ADVERTISE: u8 = 0;
const ADV_SERVE: u8 = 1;
const ADV_BACKOFF: u8 = 2;

type Controller<'a> = ExternalController<Counting<'a, BleConnector<'a>>, COMMAND_SLOTS>;
type BleStack<'a> = Stack<'a, Controller<'a>, DefaultPacketPool>;
type Conn<'a, 'b> = GattConnection<'a, 'b, DefaultPacketPool>;

#[gatt_server(connections_max = CONNECTIONS, mutex_type = NoopRawMutex)]
struct Server {
    link: LinkService,
}

/// The "wartui link" service.
#[gatt_service(uuid = SERVICE_UUID)]
struct LinkService {
    /// The bridge's frame stream, notified. Its stored value is never read.
    #[characteristic(uuid = UP_UUID, notify)]
    up: u8,
    /// The host's frames, written. Writes are taken unprocessed, so nothing is stored.
    #[characteristic(uuid = DOWN_UUID, write, write_without_response)]
    down: u8,
}

/// The Bluetooth side's state, as the link and the stack see it.
pub struct Ble {
    tx: &'static TxRing,
    /// The link layer's delivery, fed by the transport.
    pub ledger: Ledger,
    /// The transport's progress.
    pub hci: Watch,
    /// When the notifier last started waiting on TrouBLE: `notify_raw` or the OOM sleep.
    notify_since: Cell<Instant>,
    /// The link as the C6 sees it, and as it last told the P4.
    state: Cell<BleState>,
    said: Cell<Option<BleState>>,
    /// `Down::HostBytes` bytes taken since boot, wrapping: what `Up::BleCredit` reports.
    drained: Cell<u32>,
    /// The `drained` last sent, and when.
    credited: Cell<u32>,
    credited_at: Cell<Instant>,
    /// The counters kept here; [`stats`] adds the ledger's and the heap's.
    counters: Cell<BleStats>,
    /// Raised when the buffer or the subscription changes, for the notifier.
    ready: Signal<NoopRawMutex, ()>,
}

impl Ble {
    pub fn new(tx: &'static TxRing) -> Self {
        Self {
            tx,
            ledger: RefCell::new(wartui_proto::spike::AclLedger::new()),
            hci: Watch::new(),
            notify_since: Cell::new(Instant::from_ticks(0)),
            state: Cell::new(BleState::default()),
            said: Cell::new(None),
            drained: Cell::new(0),
            credited: Cell::new(0),
            credited_at: Cell::new(Instant::from_ticks(0)),
            counters: Cell::new(BleStats::default()),
            ready: Signal::new(),
        }
    }

    fn count(&self, change: impl FnOnce(&mut BleStats)) {
        let mut counters = self.counters.get();
        change(&mut counters);
        self.counters.set(counters);
    }

    /// Whether a central has subscribed to Up.
    fn subscribed(&self) -> bool {
        self.state.get().subscribed
    }

    /// Set the notifier's phase, timing each stretch of waiting on TrouBLE.
    fn notify_phase(&self, phase: u8) {
        let waits = |phase| matches!(phase, NOTIFY_RAW | NOTIFY_OOM);
        let was = self.counters.get().notify_phase;
        if waits(phase) && !waits(was) {
            self.notify_since.set(Instant::now());
        }
        self.count(|c| c.notify_phase = phase);
    }
}

/// Take the host stream's next bytes from the P4: held for the central, or discarded and
/// credited when there is none.
pub fn take_stream(shared: &Shared, bytes: &[u8]) {
    let ble = &shared.ble;
    let len = bytes.len() as u32;
    if !ble.subscribed() {
        ble.count(|c| c.dropped_unsubscribed = c.dropped_unsubscribed.wrapping_add(len));
        credit(shared, bytes.len(), true);
        return;
    }
    let taken = ble.tx.borrow_mut().push(bytes);
    if taken < bytes.len() {
        let lost = bytes.len() - taken;
        ble.count(|c| c.dropped_overflow = c.dropped_overflow.wrapping_add(lost as u32));
        credit(shared, lost, true);
    }
    ble.ready.signal(());
}

/// The counters `Up::BleStats` reports.
pub fn stats(shared: &Shared) -> BleStats {
    let ble = &shared.ble;
    let mut stats = ble.counters.get();
    let ledger = ble.ledger.borrow();
    stats.acl_bytes = ledger.acl_bytes();
    stats.delivered_bytes = ledger.delivered_bytes();
    stats.untracked_acl = ledger.untracked();
    stats.acl_outstanding = ledger.in_flight() as u32;
    let hci = &ble.hci;
    stats.hci_reads = hci.reads.get();
    stats.hci_writes = hci.writes.get();
    stats.reads_in_flight = hci.reads_in_flight.get();
    stats.writes_in_flight = hci.writes_in_flight.get();
    stats.nocp_events = hci.nocp_events.get();
    stats.disconnection_events = hci.disconnection_events.get();
    stats.heap_used = esp_alloc::HEAP.used() as u32;
    stats.heap_high_water = esp_alloc::HEAP.stats().max_usage as u32;
    stats
}

/// Count `n` more bytes drained, and send the credit when it is due or `now`.
pub fn credit(shared: &Shared, n: usize, now: bool) {
    let ble = &shared.ble;
    let drained = ble.drained.get().wrapping_add(n as u32);
    ble.drained.set(drained);
    let owed = drained.wrapping_sub(ble.credited.get());
    let due = now || owed >= CREDIT_EVERY_BYTES || ble.credited_at.get().elapsed() >= CREDIT_EVERY;
    if owed > 0 && due {
        ble.credited.set(drained);
        ble.credited_at.set(Instant::now());
        shared.push(&Up::BleCredit { drained });
    }
}

/// Set the link's state, and tell the P4 if it changed. A change of subscription clears the
/// buffer.
fn set_state(shared: &Shared, change: impl FnOnce(&mut BleState)) {
    let ble = &shared.ble;
    let mut state = ble.state.get();
    let was = state.subscribed;
    change(&mut state);
    ble.state.set(state);
    if state.subscribed != was {
        let held = ble.tx.borrow().len();
        ble.tx.borrow_mut().clear();
        ble.count(|c| c.dropped_unsubscribed = c.dropped_unsubscribed.wrapping_add(held as u32));
        credit(shared, held, true);
        ble.ready.signal(());
    }
    if ble.said.get() != Some(state) {
        ble.said.set(Some(state));
        shared.push(&Up::BleState(state));
    }
}

/// Run the Bluetooth peripheral forever. A controller that will not start leaves the link
/// running alone.
pub async fn run(bt: BT<'static>, mac: &Mac, shared: &Shared) -> ! {
    let config = Config::default().with_max_connections(CONNECTIONS as u16);
    let connector = match BleConnector::new(bt, config) {
        Ok(connector) => connector,
        Err(err) => {
            shared.error(format_args!("the Bluetooth controller would not start: {err}"));
            core::future::pending().await
        }
    };
    let controller: Controller<'_> =
        ExternalController::new(Counting::new(connector, &shared.ble.ledger, &shared.ble.hci));
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS, L2CAP_CHANNELS> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    join3(
        run_host(&mut runner, shared),
        advertise(&stack, &mut peripheral, mac, shared),
        watch_stall(shared),
    )
    .await
    .0
}

/// Run TrouBLE's host forever, starting it again if it stops.
async fn run_host(
    runner: &mut Runner<'_, Controller<'_>, DefaultPacketPool>,
    shared: &Shared,
) -> ! {
    loop {
        shared.ble.count(|c| c.runner_phase = 1);
        let ran = runner.run().await;
        shared.ble.count(|c| {
            c.runner_phase = 0;
            c.runner_restarts = c.runner_restarts.wrapping_add(1);
        });
        if let Err(err) = ran {
            shared.ble.count(|c| c.hci_errors = c.hci_errors.wrapping_add(1));
            shared.error(format_args!("the Bluetooth host stopped: {err:?}"));
            Timer::after(Duration::from_secs(1)).await;
        }
    }
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

    // The controller's own address, which a central sees: not the station MAC the name carries.
    match stack.command(ReadBdAddr::new()).await {
        Ok(addr) => {
            let mut bd: Mac = [0; 6];
            bd.copy_from_slice(addr.raw());
            bd.reverse();
            shared.log(format_args!("Bluetooth advertises `{name}` from {}", mac::full(&bd)));
        }
        Err(err) => shared.error(format_args!("could not read the Bluetooth address: {err:?}")),
    }

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

    loop {
        let advertisement = Advertisement::ConnectableScannableUndirected {
            adv_data: &adv_data[..adv_len],
            scan_data: &scan_data[..scan_len],
        };
        shared.ble.count(|c| c.adv_phase = ADV_ADVERTISE);
        let conn = match peripheral.advertise(&params, advertisement).await {
            Ok(advertiser) => advertiser.accept().await.map_err(BleHostError::BleHost),
            Err(err) => Err(err),
        };
        let conn = match conn.and_then(|conn| Ok(conn.with_attribute_server(server)?)) {
            Ok(conn) => conn,
            Err(err) => {
                shared.ble.count(|c| {
                    c.hci_errors = c.hci_errors.wrapping_add(1);
                    c.adv_phase = ADV_BACKOFF;
                });
                shared.error(format_args!("could not advertise or connect: {err:?}"));
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };
        shared.ble.count(|c| {
            c.connections = c.connections.wrapping_add(1);
            c.adv_phase = ADV_SERVE;
        });
        serve(stack, &conn, server, shared).await;
        shared.ble.count(|c| c.disconnects = c.disconnects.wrapping_add(1));
        // `serve` drops a notifier mid-await, which would read as a stall.
        shared.ble.notify_phase(NOTIFY_READY);
        set_state(shared, |state| *state = BleState::default());
    }
}

/// Serve one central until it disconnects.
async fn serve(stack: &BleStack<'_>, conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) {
    let raw = conn.raw();
    set_state(shared, |state| {
        *state = BleState {
            subscribed: false,
            mtu: raw.att_mtu(),
            phy: 1,
            interval_us: raw.params().conn_interval.as_micros() as u32,
            conn: 1,
        }
    });
    // Each answered by an event if the central agrees; a refusal leaves the link as it is.
    match PHY {
        None => shared.log(format_args!("Bluetooth PHY not requested: the build keeps 1M")),
        Some(phy) => match raw.set_phy(stack, phy).await {
            Ok(()) => shared.log(format_args!("Bluetooth {} PHY requested", phy_name(phy))),
            Err(err) => {
                shared.log(format_args!("Bluetooth {} PHY not requested: {err:?}", phy_name(phy)))
            }
        },
    }
    if let Err(err) = raw.update_data_length(stack, 251, 2120).await {
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
        match raw.update_connection_params(stack, &params).await {
            Ok(()) => shared.log(format_args!("Bluetooth interval {min}-{max} us requested")),
            Err(err) => shared.log(format_args!("Bluetooth interval not requested: {err:?}")),
        }
    }
    select(events(stack, conn, server, shared), notify(conn, server, shared)).await;
}

/// Handle the connection's events until it ends.
async fn events(stack: &BleStack<'_>, conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) {
    let ble = &shared.ble;
    loop {
        ble.count(|c| c.events_phase = EVENTS_NEXT);
        let event = conn.next().await;
        let kind = event_kind(&event, server);
        ble.count(|c| {
            c.events_seen = c.events_seen.wrapping_add(1);
            c.last_event = kind;
            c.events_phase = EVENTS_OTHER;
        });
        match event {
            GattConnectionEvent::Disconnected { .. } => return,
            GattConnectionEvent::PhyUpdated { tx_phy, .. } => {
                set_state(shared, |state| state.phy = phy_number(tx_phy));
            }
            GattConnectionEvent::ConnectionParamsUpdated { conn_interval, .. } => {
                let us = conn_interval.as_micros();
                if let Some((min, max)) = CONN_INTERVAL_US {
                    let verdict =
                        if (min..=max).contains(&us) { "accepted" } else { "not as requested" };
                    shared.log(format_args!("Bluetooth interval now {us} us: {verdict}"));
                }
                set_state(shared, |state| state.interval_us = us as u32);
            }
            GattConnectionEvent::RequestConnectionParams(request) => {
                if let Err(err) = request.accept(None, stack).await {
                    shared.log(format_args!("Bluetooth parameter request failed: {err:?}"));
                }
            }
            GattConnectionEvent::Gatt { event } => {
                // Down's writes skip the attribute table, which would store each one.
                let reply = match event {
                    GattEvent::Write(write) if write.handle() == server.link.down.handle => {
                        ble.count(|c| c.events_phase = EVENTS_FORWARD);
                        forward(shared, &write).await;
                        ble.count(|c| c.events_phase = EVENTS_OTHER);
                        write.accept_unprocessed()
                    }
                    event => event.accept(),
                };
                match reply {
                    Ok(reply) => {
                        ble.count(|c| c.events_phase = EVENTS_REPLY);
                        reply.send().await;
                        ble.count(|c| c.events_phase = EVENTS_OTHER);
                    }
                    Err(_) => ble.count(|c| c.link_errors = c.link_errors.wrapping_add(1)),
                }
            }
            _ => {}
        }
        let subscribed = server.link.up.should_notify(conn);
        let mtu = conn.raw().att_mtu();
        set_state(shared, |state| {
            state.subscribed = subscribed;
            state.mtu = mtu;
        });
    }
}

/// Send what a central wrote to the P4, waiting for room rather than evicting. The write is
/// held meanwhile, so the next waits behind it.
async fn forward(shared: &Shared, write: &WriteEvent<'_, '_, DefaultPacketPool>) {
    let len = write.with_data(|_, data| data.len());
    let deadline = Instant::now() + FORWARD_WAIT;
    while shared.up_room() < len + FORWARD_OVERHEAD {
        if Instant::now() >= deadline {
            shared.ble.count(|c| c.dropped_writes = c.dropped_writes.wrapping_add(len as u32));
            return;
        }
        Timer::after(RETRY).await;
    }
    // A write is at most `VALUE_MAX`, well inside one record.
    write.with_data(|_, data| shared.push(&Up::HostBytes(data)));
    shared.ble.count(|c| c.forwarded_bytes = c.forwarded_bytes.wrapping_add(len as u32));
}

/// Notify the buffer to the central while it is subscribed, a notification at a time.
async fn notify(conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) -> ! {
    let ble = &shared.ble;
    let mut chunk = [0; VALUE_MAX];
    loop {
        if !ble.subscribed() || ble.tx.borrow().is_empty() {
            ble.notify_phase(NOTIFY_READY);
            ble.ready.wait().await;
            continue;
        }
        ble.notify_phase(NOTIFY_CREDIT);
        // An MTU exchange raises no event, so the notifier is where a change shows.
        let mtu = conn.raw().att_mtu();
        set_state(shared, |state| state.mtu = mtu);
        let room = usize::from(mtu.saturating_sub(3)).min(VALUE_MAX);
        let n = ble.tx.borrow_mut().take(&mut chunk[..room]);
        let sent = loop {
            ble.notify_phase(NOTIFY_RAW);
            ble.count(|c| c.notify_raw_calls = c.notify_raw_calls.wrapping_add(1));
            let sent = server.link.up.notify_raw(conn, &chunk[..n], false).await;
            ble.count(|c| c.notify_raw_returns = c.notify_raw_returns.wrapping_add(1));
            match sent {
                // The pool is out of packets until the controller completes some.
                Err(Error::OutOfMemory) => {
                    ble.notify_phase(NOTIFY_OOM);
                    ble.count(|c| c.notify_oom_retries = c.notify_oom_retries.wrapping_add(1));
                    Timer::after(RETRY).await;
                }
                other => break other,
            }
        };
        ble.notify_phase(NOTIFY_CREDIT);
        match sent {
            Ok(()) => ble.count(|c| {
                c.notifications = c.notifications.wrapping_add(1);
                c.notified_bytes = c.notified_bytes.wrapping_add(n as u32);
            }),
            Err(_) => ble.count(|c| c.link_errors = c.link_errors.wrapping_add(1)),
        }
        let empty = ble.tx.borrow().is_empty();
        credit(shared, n, empty);
        // A send TrouBLE's queue had room for never waited, so the link gets its turn here.
        yield_now().await;
    }
}

/// `BleStats::last_event`'s code for a connection event, as `radio_link::EVENT_KINDS` names it.
fn event_kind(event: &GattConnectionEvent<'_, '_, DefaultPacketPool>, server: &Server<'_>) -> u8 {
    match event {
        GattConnectionEvent::Disconnected { .. } => 1,
        GattConnectionEvent::PhyUpdated { .. } => 2,
        GattConnectionEvent::ConnectionParamsUpdated { .. } => 3,
        GattConnectionEvent::SubratingParamsUpdated { .. } => 4,
        GattConnectionEvent::RequestConnectionParams(_) => 5,
        GattConnectionEvent::DataLengthUpdated { .. } => 6,
        GattConnectionEvent::FrameSpaceUpdated { .. } => 7,
        GattConnectionEvent::ConnectionRateChanged { .. } => 8,
        GattConnectionEvent::Gatt { event: GattEvent::Write(write) }
            if write.handle() == server.link.down.handle =>
        {
            9
        }
        GattConnectionEvent::Gatt { .. } => 10,
    }
}

/// Log the phases once the notifier has waited on TrouBLE, or a transport read has stayed
/// pending with ACL outstanding, for [`STALL_AFTER`]; at most once per [`STALL_LOG_EVERY`].
async fn watch_stall(shared: &Shared) -> ! {
    let ble = &shared.ble;
    let mut logged: Option<Instant> = None;
    loop {
        Timer::after(STALL_POLL).await;
        let c = ble.counters.get();
        let notify_wait = ble.notify_since.get().elapsed();
        let notify_stuck =
            matches!(c.notify_phase, NOTIFY_RAW | NOTIFY_OOM) && notify_wait > STALL_AFTER;
        let read_wait = ble.hci.read_done_at.get().elapsed();
        let acl = ble.ledger.borrow().in_flight();
        let read_stuck = ble.hci.reads_in_flight.get() > 0 && read_wait > STALL_AFTER && acl > 0;
        let recent = logged.is_some_and(|at| at.elapsed() < STALL_LOG_EVERY);
        if !(notify_stuck || read_stuck) || recent {
            continue;
        }
        logged = Some(Instant::now());
        shared.log(format_args!(
            "BLE stall: notify {} {}ms, events {} last {}, adv {}, runner {}, io {}r{}w, ACL {}, \
             rx {}ms ago",
            c.notify_phase,
            notify_wait.as_millis(),
            c.events_phase,
            c.last_event,
            c.adv_phase,
            c.runner_phase,
            ble.hci.reads_in_flight.get(),
            ble.hci.writes_in_flight.get(),
            acl,
            read_wait.as_millis(),
        ));
    }
}

/// A PHY's name in the log.
fn phy_name(phy: PhyKind) -> &'static str {
    match phy {
        PhyKind::Le1M => "1M",
        PhyKind::Le2M => "2M",
        PhyKind::LeCoded => "Coded S8",
        PhyKind::LeCodedS2 => "Coded S2",
    }
}

/// `BleState`'s number for a PHY.
fn phy_number(phy: PhyKind) -> u8 {
    match phy {
        PhyKind::Le1M => 1,
        PhyKind::Le2M => 2,
        PhyKind::LeCoded | PhyKind::LeCodedS2 => 3,
    }
}
