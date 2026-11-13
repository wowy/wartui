//! Talking to a real bridge over Bluetooth, through its C6's GATT service
//! (`wartui_proto::ble`), with `btleplug` on the tokio runtime the crate already runs.
//!
//! **Finding the bridge.** The C6 advertises `wartui XX:YY`, the bridge's last two octets, so a
//! bridge named with `--bridge` is found by scanning for that name, for at most [`SCAN_FOR`]. A
//! remembered Bluetooth address is connected to without a scan, when the OS lists it: BlueZ lists
//! every bonded device, and CoreBluetooth only what it has seen, so macOS falls back to the scan.
//! The name carries two octets, so the first `Ready` is checked against the whole spec, and a
//! bridge that only shares the name is refused.
//!
//! **The P4 serves a central only once it owns the host queue**, which it takes on a subscription
//! to Up from a bonded, encrypted link (`wartui_proto::host_owner`). The stream starts with a
//! `Ready`, and what decodes ahead of it is dropped, as on USB (`crate::gate`).
//!
//! **No `Identify` and no keepalive.** A subscription is the attachment, so there is nothing to
//! ask, and the Bluetooth gate never closes on silence, so there is nothing to hold open. A USB
//! host that holds the queue keeps a subscribed central waiting until 15 s after its last frame,
//! so a subscription with no `Ready` after [`READY_WARN_AFTER`] logs one warning and keeps waiting.
//!
//! **A USB takeover is a link going down, not a fault.** The P4 sends the central one
//! `BridgeToHost::Error` with [`USB_TOOK_BRIDGE`] ahead of releasing it. The transport reports
//! [`LinkEvent::Disconnected`], stays subscribed, and reports `Connected` on the `Ready` that
//! hands the queue back, though it is the same life, since the stream starts afresh there.
//!
//! **Writes.** Each host frame is encoded as on USB (`serial::encode_for_wire`) and written
//! without response in pieces of the ATT MTU less its 3-byte header: 244 bytes at the C6's MTU of
//! 247. A write is a stream fragment, so the first after every connect leads with a `0x00`, which
//! ends whatever partial frame the P4's accumulator held from an earlier attachment.
//!
//! **Not bonded.** The C6 refuses an unbonded central's subscription, and disconnects one that has
//! not paired 10 s after it connected. Either is [`TransportError::NotBonded`], since BlueZ never
//! starts a first pairing on its own (`docs/c6-ble-pairing-findings.md`).
//!
//! **Reconnecting.** Like `serial::supervise`: a failure that repeats is one warning, and a
//! connection that announced a bridge settles the run on that peripheral. A P4 reboot resets its
//! C6 and drops the link, which is an ordinary reconnect.

use std::time::{Duration, Instant};

use btleplug::api::{
    BDAddr, Central as _, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures_util::StreamExt;
use wartui_proto::ble::{DOWN_UUID, NAME_PREFIX, UP_UUID};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LinkError, MAX_FRAME, MAX_HOST_FRAME,
    USB_TOOK_BRIDGE, decode_frame,
};
use wartui_proto::mac::{self, Mac};

use crate::gate::{Admitted, Gate};
use crate::remember::BridgeMemory;
use crate::serial::{BridgeSpec, encode_for_wire};
use crate::{LinkEvent, LinkHandle, LinkPlumbing, TransportError, link_pair};

/// How long a scan looks for the bridge's name before giving up.
pub const SCAN_FOR: Duration = Duration::from_secs(10);

/// How often a scan reads what it has heard.
const SCAN_POLL: Duration = Duration::from_millis(250);

/// How long an unnamed scan listens before taking the bridges it has heard, so a second one
/// advertising nearby is seen rather than passed over.
const UNNAMED_LISTEN: Duration = Duration::from_secs(2);

/// How long a connect or a service discovery may take.
const CONNECT_WITHIN: Duration = Duration::from_secs(10);

/// How long a subscription waits for its `Ready` before warning that a USB host may hold the
/// bridge. Past the P4's 15 s handback, so a USB host that just let go is not reported.
pub const READY_WARN_AFTER: Duration = Duration::from_secs(20);

/// How often a connection asks the OS whether it is still connected, behind the disconnect
/// event that usually says so first.
const ALIVE_EVERY: Duration = Duration::from_secs(2);

/// The longest wait between reconnects after failures.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// When a disconnect before any `Ready` reads as the C6 dropping an unbonded central: 10 s after
/// connecting outside its pairing window, 30 s inside it.
const UNBONDED_DROP: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(8)..=Duration::from_secs(35);

/// The ATT header a write without response carries ahead of its value.
const ATT_HEADER: u16 = 3;

/// The smallest write value, at the default ATT MTU of 23.
const MIN_WRITE: usize = 20;

/// A link to a real bridge over Bluetooth, reconnecting on its own.
#[derive(Debug, Clone)]
pub struct BleTransport {
    spec: Option<BridgeSpec>,
    memory: BridgeMemory,
    reconnect_delay: Duration,
}

impl Default for BleTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl BleTransport {
    /// Find a bridge by scanning, or by the remembered Bluetooth address.
    #[must_use]
    pub fn new() -> Self {
        Self { spec: None, memory: BridgeMemory::none(), reconnect_delay: Duration::from_secs(1) }
    }

    /// Use the bridge the operator named, by its address or the end of it.
    #[must_use]
    pub fn with_spec(spec: BridgeSpec) -> Self {
        Self { spec: Some(spec), ..Self::new() }
    }

    /// Start from the bridge `memory` holds, and hold there the bridge this transport
    /// connects to.
    #[must_use]
    pub fn remember_in(mut self, memory: BridgeMemory) -> Self {
        self.memory = memory;
        self
    }

    /// How long to wait between reconnection attempts.
    #[must_use]
    pub const fn reconnect_delay(mut self, delay: Duration) -> Self {
        self.reconnect_delay = delay;
        self
    }

    /// Start the supervisor. The first [`LinkEvent::Connected`] arrives once a bridge has
    /// announced itself; failures to find or reach one arrive as [`LinkEvent::Disconnected`].
    ///
    /// # Errors
    /// [`TransportError::Bluetooth`] if the bridge was named by a device path, which names a
    /// USB port.
    pub fn start(self) -> Result<LinkHandle, TransportError> {
        if let Some(BridgeSpec::Path(path)) = &self.spec {
            return Err(TransportError::Bluetooth(format!(
                "{path} is a USB port; over Bluetooth, name the bridge by its address: \
                 --bridge XX:YY"
            )));
        }
        let (handle, plumbing) = link_pair();
        tokio::spawn(supervise(self, plumbing));
        Ok(handle)
    }
}

/// The name the C6 advertises for the bridge `spec` names, when the spec gives both octets.
#[must_use]
pub fn advertised_name(spec: &BridgeSpec) -> Option<String> {
    let tail = match spec {
        BridgeSpec::Mac(mac) => &mac[4..],
        BridgeSpec::Tail(tail) if tail.len() >= 2 => &tail[tail.len() - 2..],
        BridgeSpec::Tail(_) | BridgeSpec::Path(_) => return None,
    };
    Some(format!("{NAME_PREFIX}{}", mac::octets(tail)))
}

/// The two octets an advertised `wartui XX:YY` carries, or `None` for any other name.
fn advertised_tail(name: &str) -> Option<[u8; 2]> {
    let rest = name.strip_prefix(NAME_PREFIX)?;
    let (high, low) = rest.split_once(':')?;
    Some([mac::pair(high)?, mac::pair(low)?])
}

/// Whether an advertised `name` is a bridge `spec` could name: any bridge without a spec.
#[must_use]
pub fn names_bridge(spec: Option<&BridgeSpec>, name: &str) -> bool {
    let Some(tail) = advertised_tail(name) else { return false };
    match spec {
        None => true,
        Some(BridgeSpec::Path(_)) => false,
        Some(BridgeSpec::Mac(mac)) => mac[4..] == tail,
        Some(BridgeSpec::Tail(wanted)) => {
            let n = wanted.len().min(2);
            wanted[wanted.len() - n..] == tail[2 - n..]
        }
    }
}

/// Whether `msg` is the P4's notice that a USB host took the queue from this central.
#[must_use]
pub fn is_takeover(msg: &BridgeToHost) -> bool {
    matches!(msg, BridgeToHost::Error { message } if message.as_str() == USB_TOOK_BRIDGE)
}

/// Whether an OS error from a connect or a subscription is the bridge refusing a link that is
/// not encrypted or bonded: an ATT Insufficient Authentication (`0x05`), Authorization (`0x08`)
/// or Encryption (`0x0f`), however the OS words it.
#[must_use]
pub fn refused_for_bonding(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    [
        "authentication",
        "authorization",
        "encryption",
        "not permitted",
        "notpermitted",
        "not authorized",
        "notauthorized",
        "insufficient",
        "0x05",
        "0x08",
        "0x0f",
    ]
    .iter()
    .any(|sign| error.contains(sign))
}

/// Whether a link that dropped `after` connecting, with no `Ready` heard, reads as the C6
/// dropping a central that holds no bond.
#[must_use]
pub fn dropped_as_unbonded(after: Duration, announced: bool) -> bool {
    !announced && UNBONDED_DROP.contains(&after)
}

/// Whether a peripheral at `address`, with the OS identifier `id`, is the `remembered` one.
#[must_use]
pub fn is_remembered(remembered: &str, address: &str, id: &str) -> bool {
    remembered.eq_ignore_ascii_case(address) || remembered.eq_ignore_ascii_case(id)
}

/// The bytes of one host frame for each write to Down.
#[derive(Debug)]
pub struct DownWriter {
    /// The next frame leads with a `0x00`: the first since this connect.
    lead: bool,
    /// The most bytes one write carries.
    chunk: usize,
}

impl DownWriter {
    /// A writer for a connection just made, at ATT MTU `mtu`.
    #[must_use]
    pub fn new(mtu: u16) -> Self {
        let chunk = usize::from(mtu.saturating_sub(ATT_HEADER)).max(MIN_WRITE);
        Self { lead: true, chunk }
    }

    /// `cmd` encoded as on USB and cut into writes, the first behind a `0x00` when it is the
    /// first since the connect.
    ///
    /// # Errors
    /// [`LinkError`] if `cmd` does not fit a frame.
    pub fn writes(&mut self, cmd: &HostToBridge) -> Result<Vec<Vec<u8>>, LinkError> {
        let mut buf = [0u8; MAX_HOST_FRAME + 1];
        let len = encode_for_wire(cmd, self.lead, &mut buf)?;
        self.lead = false;
        Ok(buf[..len].chunks(self.chunk).map(<[u8]>::to_vec).collect())
    }
}

/// What one notification's bytes came to, in order.
// Large because a message carries an inline node record, as `LinkEvent::Message` does. Each is
// handed on at once, and boxing it would allocate for every frame.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    /// For the engine.
    Event(LinkEvent),
    /// This connection's first `Ready`, naming the bridge it proves: remember it.
    Proved(Mac),
    /// A `Ready` from a bridge the spec does not name, which shares only its advertised name.
    WrongBridge(Mac),
}

/// One subscription's reading of Up: notifications split frames anywhere, so bytes go through a
/// [`FrameAccumulator`] exactly as a serial port's do, then this connection's [`Gate`].
#[derive(Debug)]
pub struct Inbound {
    acc: FrameAccumulator<MAX_FRAME>,
    gate: Gate,
    spec: Option<BridgeSpec>,
    /// Whether a `Ready` has proved the board on this connection.
    proved: bool,
}

impl Inbound {
    /// A subscription that has heard nothing, for the bridge `spec` names.
    #[must_use]
    pub fn new(spec: Option<BridgeSpec>) -> Self {
        Self { acc: FrameAccumulator::new(), gate: Gate::new(), spec, proved: false }
    }

    /// Whether the stream has started: a `Ready` since the subscription or the last takeover.
    #[must_use]
    pub fn announced(&self) -> bool {
        self.gate.announced()
    }

    /// Take one notification's bytes, arriving at `now`.
    pub fn feed(&mut self, bytes: &[u8], now: Instant) -> Vec<Heard> {
        let mut heard = Vec::new();
        for &byte in bytes {
            let Some(frame) = self.acc.push(byte) else { continue };
            match decode_frame::<BridgeToHost>(frame) {
                Ok(msg) if is_takeover(&msg) && self.gate.announced() => {
                    // The stream starts afresh at the handback, with a `Ready` of the same life.
                    self.gate.restart();
                    heard.push(Heard::Event(LinkEvent::Disconnected {
                        reason: USB_TOOK_BRIDGE.to_owned(),
                    }));
                }
                Ok(msg) => match self.gate.admit(msg, now) {
                    Admitted::Ready { info, fresh, .. } => {
                        if let Some(spec) = &self.spec
                            && !spec.matches_address(&info.mac)
                        {
                            heard.push(Heard::WrongBridge(info.mac));
                            return heard;
                        }
                        if !self.proved {
                            self.proved = true;
                            heard.push(Heard::Proved(info.mac));
                        }
                        if fresh {
                            heard.push(Heard::Event(LinkEvent::Connected(info)));
                        }
                    }
                    Admitted::Message(msg) => heard.push(Heard::Event(LinkEvent::Message(msg))),
                    Admitted::Stale => {}
                },
                Err(e) => {
                    tracing::debug!(error = %e, "undecodable frame over Bluetooth");
                    heard.push(Heard::Event(LinkEvent::Garbled(e)));
                }
            }
        }
        heard
    }
}

/// How one connection ended.
enum End {
    /// The engine dropped its handle.
    HandleDropped,
    /// It ended, for this reason, after announcing a bridge or before.
    Over { reason: String, announced: bool },
}

impl End {
    fn failed(reason: impl Into<String>) -> Self {
        Self::Over { reason: reason.into(), announced: false }
    }
}

/// Keep a link up: find the bridge, serve the connection, report the failure, try again.
async fn supervise(transport: BleTransport, mut plumbing: LinkPlumbing) {
    let mut previous: Option<String> = None;
    let mut delay = transport.reconnect_delay;
    let mut adapter: Option<Adapter> = None;
    // The peripheral this run settled on, once it announced a bridge; never another after.
    let mut settled: Option<Peripheral> = None;
    loop {
        let unreported = previous.is_none();
        let end = attempt(&transport, &mut plumbing, &mut adapter, &mut settled, unreported);
        let reason = match end.await {
            End::HandleDropped => return,
            End::Over { reason, announced: true } => {
                tracing::warn!(reason = %reason, "Bluetooth link down");
                previous = None;
                delay = transport.reconnect_delay;
                if plumbing.events.send(LinkEvent::Disconnected { reason }).await.is_err() {
                    return;
                }
                tokio::time::sleep(delay).await;
                continue;
            }
            End::Over { reason, announced: false } => reason,
        };
        if previous.as_deref() == Some(reason.as_str()) {
            tracing::debug!(reason = %reason, "Bluetooth link still down");
        } else {
            tracing::warn!(reason = %reason, retry_in = ?delay, "Bluetooth link down");
            previous = Some(reason.clone());
        }
        if plumbing.events.send(LinkEvent::Disconnected { reason }).await.is_err() {
            return;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// One try: the adapter, the peripheral, then the connection.
async fn attempt(
    transport: &BleTransport,
    plumbing: &mut LinkPlumbing,
    adapter: &mut Option<Adapter>,
    settled: &mut Option<Peripheral>,
    unreported: bool,
) -> End {
    let central = match adapter {
        Some(central) => central.clone(),
        None => match open_adapter().await {
            Ok(central) => adapter.insert(central).clone(),
            Err(e) => return End::failed(e.to_string()),
        },
    };
    let peripheral = match settled {
        Some(peripheral) => peripheral.clone(),
        None => match find(&central, transport).await {
            Ok(peripheral) => peripheral,
            Err(e) => return End::failed(e.to_string()),
        },
    };
    connect(&central, &peripheral, transport, plumbing, settled, unreported).await
}

/// The first Bluetooth adapter the OS has.
async fn open_adapter() -> Result<Adapter, TransportError> {
    let manager = Manager::new().await.map_err(bluetooth)?;
    let adapters = manager.adapters().await.map_err(bluetooth)?;
    adapters.into_iter().next().ok_or(TransportError::NoAdapter)
}

/// btleplug's error as [`TransportError::Bluetooth`].
fn bluetooth(error: btleplug::Error) -> TransportError {
    TransportError::Bluetooth(error.to_string())
}

/// The name a peripheral advertises, if any.
async fn name_of(peripheral: &Peripheral) -> Option<(String, bool)> {
    let props = peripheral.properties().await.ok()??;
    let live = props.rssi.is_some();
    props.local_name.or(props.advertisement_name).map(|name| (name, live))
}

/// How a peripheral is remembered: its address, or the OS's identifier where the address is
/// hidden, as on macOS.
fn address_of(peripheral: &Peripheral) -> String {
    let address = peripheral.address();
    if address == BDAddr::default() { peripheral.id().to_string() } else { address.to_string() }
}

/// The peripheral to connect to: the remembered one when the OS lists it, else the one a scan
/// hears advertising the name.
async fn find(adapter: &Adapter, transport: &BleTransport) -> Result<Peripheral, TransportError> {
    let spec = transport.spec.as_ref();
    let remembered_mac = transport.memory.recall();
    // The remembered address stands for the remembered bridge, so only when that is the one
    // wanted.
    let wanted_remembered =
        spec.is_none_or(|spec| remembered_mac.is_some_and(|mac| spec.matches_address(&mac)));
    if let Some(remembered) = transport.memory.recall_bluetooth()
        && wanted_remembered
    {
        let listed = adapter.peripherals().await.map_err(bluetooth)?;
        if let Some(known) = listed
            .into_iter()
            .find(|p| is_remembered(&remembered, &p.address().to_string(), &p.id().to_string()))
        {
            return Ok(known);
        }
    }
    // Unnamed, the remembered bridge is the one to look for, and any other only in its absence.
    let preferred = match spec {
        Some(spec) => Some(spec.clone()),
        None => remembered_mac.map(BridgeSpec::Mac),
    };
    scan(adapter, spec, preferred.as_ref()).await
}

/// Scan for up to [`SCAN_FOR`] for a bridge `spec` names, preferring one `preferred` names.
async fn scan(
    adapter: &Adapter,
    spec: Option<&BridgeSpec>,
    preferred: Option<&BridgeSpec>,
) -> Result<Peripheral, TransportError> {
    adapter.start_scan(ScanFilter::default()).await.map_err(bluetooth)?;
    let started = Instant::now();
    let found = loop {
        let mut heard = Vec::new();
        for peripheral in adapter.peripherals().await.map_err(bluetooth)? {
            if let Some((name, live)) = name_of(&peripheral).await
                && names_bridge(spec, &name)
            {
                heard.push((name, live, peripheral));
            }
        }
        // Advertising now, when anything is: BlueZ also lists the bridges it has bonded, heard
        // or not.
        if heard.iter().any(|(_, live, _)| *live) {
            heard.retain(|(_, live, _)| *live);
        }
        let listened = started.elapsed() >= UNNAMED_LISTEN || spec.is_some();
        if !heard.is_empty() && listened {
            break heard;
        }
        if started.elapsed() >= SCAN_FOR {
            let _ = adapter.stop_scan().await;
            return Err(TransportError::NotFound {
                name: spec
                    .and_then(advertised_name)
                    .unwrap_or_else(|| format!("{NAME_PREFIX}XX:YY")),
                secs: SCAN_FOR.as_secs(),
            });
        }
        tokio::time::sleep(SCAN_POLL).await;
    };
    let _ = adapter.stop_scan().await;
    pick(found, preferred)
}

/// The one bridge among those heard, preferring those `preferred` names.
fn pick(
    mut heard: Vec<(String, bool, Peripheral)>,
    preferred: Option<&BridgeSpec>,
) -> Result<Peripheral, TransportError> {
    if heard.iter().any(|(name, _, _)| names_bridge(preferred, name)) {
        heard.retain(|(name, _, _)| names_bridge(preferred, name));
    }
    if heard.len() == 1 {
        return Ok(heard.remove(0).2);
    }
    let mut names: Vec<String> = heard.into_iter().map(|(name, _, _)| name).collect();
    names.sort();
    Err(TransportError::SeveralAdvertising { names: names.join(", ") })
}

/// The bridge as the operator names it, for the messages: its advertised octets.
async fn label_of(peripheral: &Peripheral, transport: &BleTransport) -> String {
    if let Some((name, _)) = name_of(peripheral).await
        && let Some(tail) = advertised_tail(&name)
    {
        return mac::octets(&tail).to_string();
    }
    match (&transport.spec, transport.memory.recall()) {
        (Some(spec), _) => spec.to_string(),
        (None, Some(mac)) => mac::short(&mac).to_string(),
        (None, None) => "XX:YY".to_owned(),
    }
}

/// Connect, subscribe to Up and serve the connection until it ends.
async fn connect(
    adapter: &Adapter,
    peripheral: &Peripheral,
    transport: &BleTransport,
    plumbing: &mut LinkPlumbing,
    settled: &mut Option<Peripheral>,
    unreported: bool,
) -> End {
    // Retrying a failure already reported is not news; `RUST_LOG=debug` keeps them.
    macro_rules! progress {
        ($($arg:tt)*) => {
            if unreported { tracing::info!($($arg)*) } else { tracing::debug!($($arg)*) }
        };
    }
    let label = label_of(peripheral, transport).await;
    let not_bonded = || TransportError::NotBonded { bridge: label.clone() }.to_string();
    progress!(bridge = %label, "connecting to the bridge over Bluetooth");

    let connected_at = Instant::now();
    if !peripheral.is_connected().await.unwrap_or(false) {
        match tokio::time::timeout(CONNECT_WITHIN, peripheral.connect()).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) if refused_for_bonding(&e.to_string()) => return End::failed(not_bonded()),
            Ok(Err(e)) => return End::failed(format!("could not connect to {label}: {e}")),
            Err(_) => {
                return End::failed(format!(
                    "could not connect to {label} in {}s",
                    CONNECT_WITHIN.as_secs()
                ));
            }
        }
    }
    let end = serve(adapter, peripheral, transport, plumbing, settled, &label).await;
    let end = match end {
        // The C6 drops an unbonded central about 10 s after it connects.
        End::Over { announced: false, .. }
            if dropped_as_unbonded(connected_at.elapsed(), false)
                && !peripheral.is_connected().await.unwrap_or(false) =>
        {
            End::failed(not_bonded())
        }
        End::Over { reason, announced } if refused_for_bonding(&reason) && !announced => {
            End::Over { reason: not_bonded(), announced }
        }
        end => end,
    };
    let _ = peripheral.disconnect().await;
    end
}

/// The subscription: discover, subscribe, then pump both ways until something ends it.
async fn serve(
    adapter: &Adapter,
    peripheral: &Peripheral,
    transport: &BleTransport,
    plumbing: &mut LinkPlumbing,
    settled: &mut Option<Peripheral>,
    label: &str,
) -> End {
    match tokio::time::timeout(CONNECT_WITHIN, peripheral.discover_services()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return End::failed(format!("could not read {label}'s services: {e}")),
        Err(_) => return End::failed(format!("{label} did not list its services")),
    }
    let chars = peripheral.characteristics();
    let pick = |uuid: u128| chars.iter().find(|c| c.uuid.as_u128() == uuid).cloned();
    let (Some(up), Some(down)) = (pick(UP_UUID), pick(DOWN_UUID)) else {
        return End::failed(format!("{label} offers no wartui link service"));
    };
    let mut notes = match peripheral.notifications().await {
        Ok(notes) => notes,
        Err(e) => return End::failed(format!("could not read {label}'s notifications: {e}")),
    };
    let mut events = match adapter.events().await {
        Ok(events) => events,
        Err(e) => return End::failed(bluetooth(e).to_string()),
    };
    if let Err(e) = peripheral.subscribe(&up).await {
        return End::failed(format!("could not subscribe to {label}: {e}"));
    }
    let mtu = peripheral.mtu();
    tracing::info!(bridge = %label, mtu, "subscribed to the bridge over Bluetooth");

    let end =
        pump(peripheral, transport, plumbing, settled, (&up, &down), (&mut notes, &mut events));
    let end = end.await;
    if matches!(end, End::HandleDropped) {
        let _ = peripheral.unsubscribe(&up).await;
    }
    end
}

/// The notifications stream, and the adapter's events.
type Notes =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = btleplug::api::ValueNotification> + Send>>;
type Events = std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>;

/// Pump a subscribed connection: notifications up, commands down, until either side ends.
async fn pump(
    peripheral: &Peripheral,
    transport: &BleTransport,
    plumbing: &mut LinkPlumbing,
    settled: &mut Option<Peripheral>,
    (up, down): (&Characteristic, &Characteristic),
    (notes, events): (&mut Notes, &mut Events),
) -> End {
    let id = peripheral.id();
    let mut inbound = Inbound::new(transport.spec.clone());
    let mut writer = DownWriter::new(peripheral.mtu());
    let mut alive = tokio::time::interval(ALIVE_EVERY);
    alive.reset();
    let ready_warn = tokio::time::sleep(READY_WARN_AFTER);
    tokio::pin!(ready_warn);
    let mut warned = false;
    let mut ever_announced = false;
    let over = |reason: &str, announced| End::Over { reason: reason.to_owned(), announced };

    loop {
        tokio::select! {
            biased;
            note = notes.next() => {
                let Some(note) = note else { return over("notifications ended", ever_announced) };
                if note.uuid.as_u128() != up.uuid.as_u128() {
                    continue;
                }
                for heard in inbound.feed(&note.value, Instant::now()) {
                    match heard {
                        Heard::Proved(mac) => {
                            ever_announced = true;
                            *settled = Some(peripheral.clone());
                            transport.memory.remember_bluetooth(mac, address_of(peripheral));
                        }
                        Heard::WrongBridge(mac) => {
                            let named = transport.spec.as_ref().map(ToString::to_string);
                            return End::failed(format!(
                                "the bridge that answered is {}, not {}",
                                mac::full(&mac),
                                named.unwrap_or_default()
                            ));
                        }
                        Heard::Event(event) => {
                            if let LinkEvent::Disconnected { reason } = &event {
                                tracing::warn!(reason = %reason, "the bridge let this host go");
                                // The handback is expected, and is no reason to warn again.
                                warned = true;
                            }
                            if plumbing.events.send(event).await.is_err() {
                                return End::HandleDropped;
                            }
                        }
                    }
                }
            }
            event = events.next() => match event {
                Some(CentralEvent::DeviceDisconnected(gone)) if gone == id => {
                    return over("the bridge disconnected", ever_announced);
                }
                Some(_) => {}
                None => return over("the Bluetooth adapter stopped reporting", ever_announced),
            },
            _ = alive.tick() => {
                if !peripheral.is_connected().await.unwrap_or(false) {
                    return over("the bridge disconnected", ever_announced);
                }
            }
            () = &mut ready_warn, if !warned && !inbound.announced() => {
                warned = true;
                tracing::warn!(
                    "no Ready {}s after subscribing; the bridge may be held by a USB host, \
                     and is waited for",
                    READY_WARN_AFTER.as_secs()
                );
            }
            cmd = plumbing.commands.recv() => {
                let Some(cmd) = cmd else { return End::HandleDropped };
                let Ok(writes) = writer.writes(&cmd) else {
                    // Only reachable if a command outgrew MAX_HOST_FRAME.
                    tracing::error!("command did not fit in a frame; dropping it");
                    continue;
                };
                for write in writes {
                    if let Err(e) = peripheral.write(down, &write, WriteType::WithoutResponse).await {
                        return over(&format!("write failed: {e}"), ever_announced);
                    }
                }
            }
        }
    }
}
