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

use core::cell::{Cell, RefCell};
use core::fmt::Write as _;

use bt_hci::cmd::info::ReadBdAddr;
use embassy_futures::join::join;
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

use crate::config::BLE_WINDOW_BYTES;
use crate::hci::{Counting, Ledger};
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
        ExternalController::new(Counting::new(connector, &shared.ble.ledger));
    let mut resources: HostResources<DefaultPacketPool, CONNECTIONS, L2CAP_CHANNELS> =
        HostResources::new();
    let stack = trouble_host::new(controller, &mut resources).build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    join(run_host(&mut runner, shared), advertise(&stack, &mut peripheral, mac, shared)).await.0
}

/// Run TrouBLE's host forever, starting it again if it stops.
async fn run_host(
    runner: &mut Runner<'_, Controller<'_>, DefaultPacketPool>,
    shared: &Shared,
) -> ! {
    loop {
        if let Err(err) = runner.run().await {
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
        let conn = match peripheral.advertise(&params, advertisement).await {
            Ok(advertiser) => advertiser.accept().await.map_err(BleHostError::BleHost),
            Err(err) => Err(err),
        };
        let conn = match conn.and_then(|conn| Ok(conn.with_attribute_server(server)?)) {
            Ok(conn) => conn,
            Err(err) => {
                shared.ble.count(|c| c.hci_errors = c.hci_errors.wrapping_add(1));
                shared.error(format_args!("could not advertise or connect: {err:?}"));
                Timer::after(Duration::from_secs(1)).await;
                continue;
            }
        };
        serve(stack, &conn, server, shared).await;
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
    if let Err(err) = raw.set_phy(stack, PhyKind::Le2M).await {
        shared.log(format_args!("Bluetooth 2M PHY not requested: {err:?}"));
    }
    if let Err(err) = raw.update_data_length(stack, 251, 2120).await {
        shared.log(format_args!("Bluetooth data length not requested: {err:?}"));
    }
    select(events(stack, conn, server, shared), notify(conn, server, shared)).await;
}

/// Handle the connection's events until it ends.
async fn events(stack: &BleStack<'_>, conn: &Conn<'_, '_>, server: &Server<'_>, shared: &Shared) {
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { .. } => return,
            GattConnectionEvent::PhyUpdated { tx_phy, .. } => {
                set_state(shared, |state| state.phy = phy_number(tx_phy));
            }
            GattConnectionEvent::ConnectionParamsUpdated { conn_interval, .. } => {
                set_state(shared, |state| state.interval_us = conn_interval.as_micros() as u32);
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
                        forward(shared, &write).await;
                        write.accept_unprocessed()
                    }
                    event => event.accept(),
                };
                match reply {
                    Ok(reply) => reply.send().await,
                    Err(_) => shared.ble.count(|c| c.link_errors = c.link_errors.wrapping_add(1)),
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
            ble.ready.wait().await;
            continue;
        }
        // An MTU exchange raises no event, so the notifier is where a change shows.
        let mtu = conn.raw().att_mtu();
        set_state(shared, |state| state.mtu = mtu);
        let room = usize::from(mtu.saturating_sub(3)).min(VALUE_MAX);
        let n = ble.tx.borrow_mut().take(&mut chunk[..room]);
        let sent = loop {
            match server.link.up.notify_raw(conn, &chunk[..n], false).await {
                // The pool is out of packets until the controller completes some.
                Err(Error::OutOfMemory) => Timer::after(RETRY).await,
                other => break other,
            }
        };
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

/// `BleState`'s number for a PHY.
fn phy_number(phy: PhyKind) -> u8 {
    match phy {
        PhyKind::Le1M => 1,
        PhyKind::Le2M => 2,
        PhyKind::LeCoded | PhyKind::LeCodedS2 => 3,
    }
}
