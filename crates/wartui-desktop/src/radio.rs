//! [`BleRadio`], the host's Bluetooth radio through `btleplug`, on the tokio runtime the crate
//! already runs. It keeps [`Radio`]'s contract over BlueZ and CoreBluetooth, and holds every rule
//! that is the OS's rather than the link's.
//!
//! **Each connect takes a fresh handle on macOS.** btleplug's CoreBluetooth backend drops a
//! peripheral from its own table on every disconnect, and keeps it in the adapter's. So
//! `peripheral(&id)`, `peripherals()` and a rescan all hand back a handle whose connect fails or
//! whose notifications go nowhere (`docs/ble-host-link-findings.md` § "macOS"). Asking for the
//! peripheral by its identifier (`retrieve_peripherals`) builds a live handle for one a disconnect
//! dropped, as long as no rescan has re-added it to the backend's table, so every connect asks
//! that way first (`fresh`), and a scan clears both of btleplug's tables before it starts. BlueZ
//! keeps its device object across a disconnect, so Linux needs neither. The same clear leaves
//! CoreBluetooth's listing empty, so [`Radio::recall`] finds nothing on macOS and the driver
//! scans.
//!
//! **A scan reads what it hears.** Its listing is unfiltered, and in dense RF the OS hears
//! hundreds of devices, so it reads the properties of a device only when the adapter reports it
//! discovered or heard again, at most `ID_READS` times while they carry no id, since each read
//! is a serial D-Bus round trip (`Rationing`). A device left alone that way is read again
//! whenever the adapter reports service data under the id from it, since that is the bridge being
//! heard: BlueZ keeps no service data across its own restart, so the id can come late. A bridge is
//! read again until it is heard advertising, because BlueZ first replays every device it knows as
//! discovered, so a bonded bridge is read before the scan has heard it. That holds for every
//! bridge, one the driver's spec does not name included: the radio knows no spec, and a second
//! bridge costs a few reads. [`Radio::known`] reads the OS's listing against the same rationing as
//! the scan under way.

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use btleplug::api::bleuuid::uuid_from_u16;
use btleplug::api::{
    BDAddr, Central as _, CentralEvent, Characteristic, Manager as _, Peripheral as _,
    PeripheralProperties, RetrievePeripheralsOptions, ScanFilter, ValueNotification, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use futures_util::{FutureExt as _, Stream, StreamExt as _};
use tokio::sync::OnceCell;
use wartui_bridge::pipe::{
    Connected, Connection, Incoming, Radio, RadioError, RadioErrorKind, RadioTraits, Recalled,
    Sighting, WriteKind,
};
use wartui_proto::ble::{DOWN_UUID, ID_UUID, UP_UUID, advertised_octets};

/// How many times a scan reads a device that carries no id before it leaves it alone.
const ID_READS: u8 = 3;

/// What the driver needs to know of this OS.
#[must_use]
pub const fn radio_traits() -> RadioTraits {
    RadioTraits { pairs_on_connect: !cfg!(target_os = "linux"), macos: cfg!(target_os = "macos") }
}

/// The host's Bluetooth radio, through `btleplug`.
#[derive(Debug, Default)]
pub struct BleRadio {
    /// The first adapter the OS has, opened on the first call that needs it.
    adapter: OnceCell<Adapter>,
    /// The peripheral each key a sighting or a recall gave stands for.
    peers: Arc<Mutex<HashMap<String, Peripheral>>>,
    /// What the scan under way has read.
    rationing: Arc<Mutex<Rationing<PeripheralId>>>,
}

/// A new [`BleRadio`], shared the way the Bluetooth transport takes it.
#[must_use]
pub fn ble_radio() -> Arc<dyn Radio> {
    Arc::new(BleRadio::default())
}

impl BleRadio {
    /// The adapter, opened once it opens. A failure is retried on the next call.
    async fn adapter(&self) -> Result<Adapter, RadioError> {
        self.adapter.get_or_try_init(open_adapter).await.cloned()
    }

    /// The peripheral `peer` stands for.
    fn peer(&self, peer: &str) -> Result<Peripheral, RadioError> {
        lock(&self.peers).get(peer).cloned().ok_or_else(|| {
            RadioError::new(
                RadioErrorKind::Other,
                format!("no Bluetooth device is known as {peer}"),
            )
        })
    }
}

impl Radio for BleRadio {
    fn traits(&self) -> RadioTraits {
        radio_traits()
    }

    fn recall<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Recalled>, RadioError>> {
        async move {
            // Every scan clears CoreBluetooth's tables, so its listing holds nothing to recall
            // (§ "Each connect takes a fresh handle on macOS").
            if cfg!(target_os = "macos") {
                return Ok(None);
            }
            let adapter = self.adapter().await?;
            let listed = adapter.peripherals().await.map_err(other)?;
            let Some(known) = listed
                .into_iter()
                .find(|p| is_remembered(key, &p.address().to_string(), &p.id().to_string()))
            else {
                return Ok(None);
            };
            let octets = read_of(&known).await.and_then(|read| read.octets);
            let peer = address_of(&known);
            lock(&self.peers).insert(peer.clone(), known);
            Ok(Some(Recalled { peer, octets }))
        }
        .boxed()
    }

    fn scan(&self) -> BoxFuture<'_, Result<BoxStream<'static, Sighting>, RadioError>> {
        async move {
            let adapter = self.adapter().await?;
            // A scan's discovery builds a live handle only for a peripheral btleplug holds none
            // for (§ "Each connect takes a fresh handle on macOS").
            if cfg!(target_os = "macos")
                && let Err(e) = adapter.clear_peripherals().await
            {
                tracing::debug!(error = %e, "could not clear the Bluetooth peripherals");
            }
            // Opened before the scan starts, so nothing it hears goes by unreported.
            let events = adapter.events().await.map_err(other)?;
            adapter.start_scan(ScanFilter::default()).await.map_err(other)?;
            *lock(&self.rationing) = Rationing::default();
            Ok(sightings(events, adapter, Arc::clone(&self.rationing), Arc::clone(&self.peers)))
        }
        .boxed()
    }

    fn stop_scan(&self) -> BoxFuture<'_, ()> {
        async move {
            // An adapter that never opened has no scan to stop, and is not opened to stop one.
            if let Some(adapter) = self.adapter.get() {
                let _ = adapter.stop_scan().await;
            }
        }
        .boxed()
    }

    fn known(&self) -> BoxFuture<'_, Result<Vec<Sighting>, RadioError>> {
        async move {
            let adapter = self.adapter().await?;
            let mut seen = Vec::new();
            for peripheral in adapter.peripherals().await.map_err(other)? {
                if let Some(sighting) = note(&self.rationing, &self.peers, peripheral, false).await
                {
                    seen.push(sighting);
                }
            }
            Ok(seen)
        }
        .boxed()
    }

    fn connect<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, Result<Connected, RadioError>> {
        async move {
            let adapter = self.adapter().await?;
            let mut peripheral = self.peer(peer)?;
            if cfg!(target_os = "macos") {
                peripheral = fresh(&adapter, peripheral).await;
                // So `abandon` reaches the handle this connect uses.
                lock(&self.peers).insert(peer.to_owned(), peripheral.clone());
            }
            let held = peripheral.is_connected().await.unwrap_or(false);
            if !held {
                peripheral.connect().await.map_err(other)?;
            }
            let connection = BleConnection { adapter, peripheral, up: None, down: None };
            Ok(Connected { connection: Box::new(connection), made: !held })
        }
        .boxed()
    }

    fn abandon<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, ()> {
        async move {
            // Unlike a link that dropped, this one never came up: BlueZ's `Connect` is still
            // pending, and could complete after this process has gone. `Disconnect` cancels it,
            // and BlueZ arms its 2 s disconnect timer only for a connected device, so no later
            // connection inherits it.
            if let Ok(peripheral) = self.peer(peer) {
                let _ = peripheral.disconnect().await;
            }
        }
        .boxed()
    }
}

/// The first Bluetooth adapter the OS has.
async fn open_adapter() -> Result<Adapter, RadioError> {
    let manager = Manager::new().await.map_err(other)?;
    let adapters = manager.adapters().await.map_err(other)?;
    adapters
        .into_iter()
        .next()
        .ok_or_else(|| RadioError::new(RadioErrorKind::NoAdapter, "no Bluetooth adapter"))
}

/// btleplug's error, in its own words.
fn other(error: btleplug::Error) -> RadioError {
    RadioError::new(RadioErrorKind::Other, error.to_string())
}

/// btleplug's error from a subscription or a write, as [`RadioErrorKind::NotBonded`] when it is
/// the C6's refusal for want of a bond ([`refused_for_bonding`]).
fn refused_or_other(error: btleplug::Error) -> RadioError {
    let text = error.to_string();
    let kind =
        if refused_for_bonding(&text) { RadioErrorKind::NotBonded } else { RadioErrorKind::Other };
    RadioError::new(kind, text)
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A live handle for `peripheral`. On macOS, CoreBluetooth is asked for it by its identifier:
/// that returns the same handle while it is live, and a new one once a disconnect has dropped it,
/// unless a rescan has re-added it (§ "Each connect takes a fresh handle on macOS"). Elsewhere,
/// and when CoreBluetooth returns none, it is `peripheral` itself.
async fn fresh(adapter: &Adapter, peripheral: Peripheral) -> Peripheral {
    if !cfg!(target_os = "macos") {
        return peripheral;
    }
    let id = peripheral.id();
    let options =
        RetrievePeripheralsOptions { identifiers: Some(vec![id.clone()]), services: None };
    match adapter.retrieve_peripherals(options).await {
        // btleplug's handles carry no identity to compare, so every retrieval is logged.
        Ok(retrieved) => match retrieved.into_iter().find(|p| p.id() == id) {
            Some(found) => {
                tracing::debug!(%id, "took the bridge's peripheral from CoreBluetooth");
                return found;
            }
            None => tracing::debug!(%id, "CoreBluetooth does not know the bridge's peripheral"),
        },
        Err(e) => tracing::debug!(error = %e, "could not retrieve the bridge's peripheral"),
    }
    peripheral
}

/// How a peripheral is remembered: its address, or the OS's identifier where the address is
/// hidden, as on macOS.
fn address_of(peripheral: &Peripheral) -> String {
    let address = peripheral.address();
    if address == BDAddr::default() { peripheral.id().to_string() } else { address.to_string() }
}

/// Whether a peripheral at `address`, with the OS identifier `id`, is the `remembered` one.
#[must_use]
pub fn is_remembered(remembered: &str, address: &str, id: &str) -> bool {
    remembered.eq_ignore_ascii_case(address) || remembered.eq_ignore_ascii_case(id)
}

/// Whether an OS error from a subscription or a write request is the C6's ATT refusal of a
/// link that is not encrypted with a bond it holds. The C6 answers both with Insufficient
/// Authentication (`0x05`), and each sign is how an OS words that:
///
/// - `not paired`: BlueZ's text for an ATT Insufficient Authentication, Encryption or Encryption
///   Key Size error (`org.bluez.Error.NotPermitted`), and the text btleplug passes on.
/// - `insufficient` beside `authentication` or `encryption`: CoreBluetooth's for
///   `CBATTError.insufficientAuthentication` and `.insufficientEncryption`.
///
/// Windows reports any ATT error as a bare protocol error, so it gives no sign.
#[must_use]
pub fn refused_for_bonding(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("not paired")
        || (error.contains("insufficient")
            && (error.contains("authentication") || error.contains("encryption")))
}

/// What one read of a peripheral's properties found.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Read {
    /// The bridge's last two octets, when it carries the id.
    octets: Option<[u8; 2]>,
    /// The name it advertises, when the OS heard one: a label only.
    name: Option<String>,
    /// Whether it is advertising now.
    live: bool,
}

impl Read {
    /// What `props` say: the id from their service data under [`ID_UUID`]
    /// ([`advertised_octets`]).
    fn of(props: PeripheralProperties) -> Self {
        let id = props.service_data.get(&uuid_from_u16(ID_UUID));
        Self {
            octets: advertised_octets(id.map(Vec::as_slice)),
            name: props.local_name.or(props.advertisement_name),
            live: props.rssi.is_some(),
        }
    }
}

/// Read a peripheral's properties, if the OS has them.
async fn read_of(peripheral: &Peripheral) -> Option<Read> {
    peripheral.properties().await.ok()?.map(Read::of)
}

/// A device a scan has identified as a bridge.
#[derive(Debug, Clone, Copy)]
struct Identified {
    octets: [u8; 2],
    /// Whether a read has found it advertising.
    live: bool,
}

/// What one scan has read, by the OS's identifier `I` for each device: which reads are worth
/// their round trip (§ "A scan reads what it hears").
#[derive(Debug)]
struct Rationing<I> {
    /// The devices that carried the id.
    identified: HashMap<I, Identified>,
    /// Devices read [`ID_READS`] times with no id, read again only when heard with one.
    left_alone: HashSet<I>,
    /// How many times each device not yet identified has been read.
    unidentified: HashMap<I, u8>,
}

impl<I> Default for Rationing<I> {
    fn default() -> Self {
        Self {
            identified: HashMap::new(),
            left_alone: HashSet::new(),
            unidentified: HashMap::new(),
        }
    }
}

impl<I: Clone + Eq + Hash> Rationing<I> {
    /// Whether a hearing of `id` is worth reading its properties for: a bridge until a read has
    /// found it advertising, and any other device unless it was left alone and not
    /// `heard_with_id`.
    fn wants(&self, id: &I, heard_with_id: bool) -> bool {
        match self.identified.get(id) {
            Some(bridge) => !bridge.live,
            None => heard_with_id || !self.left_alone.contains(id),
        }
    }

    /// Take what a read of `id`, known as `peer`, found, and the sighting it makes. A bridge
    /// already identified keeps its octets through a read that carries no id. A read with no id
    /// of any other device counts towards [`ID_READS`].
    fn noted(&mut self, id: I, peer: String, read: Option<Read>) -> Option<Sighting> {
        if let Some(bridge) = self.identified.get_mut(&id) {
            let Read { name, live, .. } = read?;
            bridge.live |= live;
            return Some(Sighting { peer, octets: bridge.octets, name, live });
        }
        let Some((octets, read)) = read.and_then(|read| Some((read.octets?, read))) else {
            if !self.left_alone.contains(&id) {
                let reads = self.unidentified.entry(id.clone()).or_default();
                *reads += 1;
                if *reads >= ID_READS {
                    self.unidentified.remove(&id);
                    self.left_alone.insert(id);
                }
            }
            return None;
        };
        self.unidentified.remove(&id);
        self.left_alone.remove(&id);
        let Read { name, live, .. } = read;
        self.identified.insert(id, Identified { octets, live });
        Some(Sighting { peer, octets, name, live })
    }
}

/// Read `peripheral`'s properties, when `rationing` wants them, and note what the read found. A
/// peripheral `heard_with_id` is read even once left alone. A sighting puts the peripheral in
/// `peers` under its key.
async fn note(
    rationing: &Mutex<Rationing<PeripheralId>>,
    peers: &Mutex<HashMap<String, Peripheral>>,
    peripheral: Peripheral,
    heard_with_id: bool,
) -> Option<Sighting> {
    let id = peripheral.id();
    if !lock(rationing).wants(&id, heard_with_id) {
        return None;
    }
    let read = read_of(&peripheral).await;
    let sighting = lock(rationing).noted(id, address_of(&peripheral), read)?;
    lock(peers).insert(sighting.peer.clone(), peripheral);
    Some(sighting)
}

/// The adapter's events.
type Events = std::pin::Pin<Box<dyn Stream<Item = CentralEvent> + Send>>;

/// The bridges `events` report, read as `rationing` allows. Each read happens while the driver
/// polls the stream.
fn sightings(
    events: Events,
    adapter: Adapter,
    rationing: Arc<Mutex<Rationing<PeripheralId>>>,
    peers: Arc<Mutex<HashMap<String, Peripheral>>>,
) -> BoxStream<'static, Sighting> {
    let id_key = uuid_from_u16(ID_UUID);
    events
        .filter_map(move |event| {
            let (adapter, rationing, peers) =
                (adapter.clone(), Arc::clone(&rationing), Arc::clone(&peers));
            async move {
                let (id, with_id) = match event {
                    // BlueZ reports a device it already knew, heard again, as an RSSI change.
                    CentralEvent::DeviceDiscovered(id)
                    | CentralEvent::DeviceUpdated(id)
                    | CentralEvent::RssiUpdate { id, .. } => (id, false),
                    // The bridge heard: BlueZ reports its service data on every advertisement.
                    CentralEvent::ServiceDataAdvertisement { id, service_data } => {
                        let with_id = service_data.contains_key(&id_key);
                        (id, with_id)
                    }
                    _ => return None,
                };
                if !lock(&rationing).wants(&id, with_id) {
                    return None;
                }
                let peripheral = adapter.peripheral(&id).await.ok()?;
                note(&rationing, &peers, peripheral, with_id).await
            }
        })
        .boxed()
}

/// One link to a bridge, through `btleplug`.
struct BleConnection {
    adapter: Adapter,
    peripheral: Peripheral,
    /// The link service's Up and Down, once discovered.
    up: Option<Characteristic>,
    down: Option<Characteristic>,
}

fn no_service() -> RadioError {
    RadioError::new(RadioErrorKind::NoService, "the bridge offers no wartui link service")
}

impl Connection for BleConnection {
    fn discover(&mut self) -> BoxFuture<'_, Result<(), RadioError>> {
        async move {
            self.peripheral.discover_services().await.map_err(other)?;
            let chars = self.peripheral.characteristics();
            let pick = |uuid: u128| chars.iter().find(|c| c.uuid.as_u128() == uuid).cloned();
            let (Some(up), Some(down)) = (pick(UP_UUID), pick(DOWN_UUID)) else {
                return Err(no_service());
            };
            (self.up, self.down) = (Some(up), Some(down));
            Ok(())
        }
        .boxed()
    }

    fn subscribe(&mut self) -> BoxFuture<'_, Result<BoxStream<'static, Incoming>, RadioError>> {
        async move {
            let up = self.up.clone().ok_or_else(no_service)?;
            // Both open before the subscription, so the `Ready` its answer brings is heard.
            let notes = self.peripheral.notifications().await.map_err(other)?;
            let events = self.adapter.events().await.map_err(other)?;
            self.peripheral.subscribe(&up).await.map_err(refused_or_other)?;
            Ok(incoming(notes, events, &up, self.peripheral.id()))
        }
        .boxed()
    }

    fn mtu(&self) -> u16 {
        self.peripheral.mtu()
    }

    fn write<'a>(
        &'a mut self,
        bytes: &'a [u8],
        kind: WriteKind,
    ) -> BoxFuture<'a, Result<(), RadioError>> {
        async move {
            let down = self.down.as_ref().ok_or_else(no_service)?;
            let kind = match kind {
                WriteKind::Command => WriteType::WithoutResponse,
                WriteKind::Request => WriteType::WithResponse,
            };
            self.peripheral.write(down, bytes, kind).await.map_err(refused_or_other)
        }
        .boxed()
    }

    fn is_connected(&self) -> BoxFuture<'_, bool> {
        async move { self.peripheral.is_connected().await.unwrap_or(false) }.boxed()
    }

    fn close(self: Box<Self>, unsubscribe: bool) -> BoxFuture<'static, ()> {
        async move {
            if unsubscribe && let Some(up) = &self.up {
                let _ = self.peripheral.unsubscribe(up).await;
            }
            // A link that already dropped is not disconnected again: BlueZ runs the request on
            // its 2 s timer, where it lands on the connection the next try makes, and ends it.
            if self.peripheral.is_connected().await.unwrap_or(false) {
                let _ = self.peripheral.disconnect().await;
            }
        }
        .boxed()
    }
}

/// The notifications stream.
type Notes = std::pin::Pin<Box<dyn Stream<Item = ValueNotification> + Send>>;

/// What a subscription to `up` carries: its notifications, then the link's drop once `events`
/// report this peripheral `id` disconnected or end. It ends when the notifications do.
fn incoming(
    notes: Notes,
    events: Events,
    up: &Characteristic,
    id: PeripheralId,
) -> BoxStream<'static, Incoming> {
    struct Feed {
        notes: Notes,
        events: Events,
        up: u128,
        id: PeripheralId,
        dropped: bool,
    }
    let feed = Feed { notes, events, up: up.uuid.as_u128(), id, dropped: false };
    stream::unfold(feed, |mut feed| async move {
        if feed.dropped {
            return None;
        }
        loop {
            tokio::select! {
                // A notification ready beside the drop goes first.
                biased;
                note = feed.notes.next() => match note {
                    Some(note) if note.uuid.as_u128() == feed.up => {
                        return Some((Incoming::Notified(note.value), feed));
                    }
                    Some(_) => {}
                    None => return None,
                },
                event = feed.events.next() => {
                    let dropped = match event {
                        Some(CentralEvent::DeviceDisconnected(gone)) if gone == feed.id => {
                            RadioError::new(RadioErrorKind::Disconnected, "the bridge disconnected")
                        }
                        Some(_) => continue,
                        None => RadioError::new(
                            RadioErrorKind::Other,
                            "the Bluetooth adapter stopped reporting",
                        ),
                    };
                    feed.dropped = true;
                    return Some((Incoming::Dropped(dropped), feed));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use btleplug::api::PeripheralProperties;
    use btleplug::api::bleuuid::uuid_from_u16;
    use wartui_bridge::pipe::Sighting;
    use wartui_proto::ble::ID_UUID;
    use wartui_proto::wardrive_esp::beacon::{self, Beacon, COMPANY, STATUS_MAX};

    use super::{ID_READS, Rationing, Read};

    /// A read that found the bridge `octets`, advertising now or not.
    fn read(octets: [u8; 2], live: bool) -> Option<Read> {
        Some(Read { octets: Some(octets), name: None, live })
    }

    /// A read of a device that carries no id.
    fn unidentified(live: bool) -> Option<Read> {
        Some(Read { octets: None, name: Some("Pixel 8".to_owned()), live })
    }

    fn sighting(octets: [u8; 2], name: Option<&str>, live: bool) -> Option<Sighting> {
        Some(Sighting { peer: "peer".to_owned(), octets, name: name.map(str::to_owned), live })
    }

    #[test]
    fn ble_read_takes_octets_when_service_data_carries_the_id() {
        let mut props = PeripheralProperties {
            local_name: Some("wartui 44:C0".to_owned()),
            rssi: Some(-60),
            ..PeripheralProperties::default()
        };
        props.service_data.insert(uuid_from_u16(ID_UUID), vec![0x44, 0xC0]);
        let want =
            Read { octets: Some([0x44, 0xC0]), name: Some("wartui 44:C0".to_owned()), live: true };
        assert_eq!(Read::of(props), want);
    }

    #[test]
    fn ble_read_finds_no_octets_when_device_carries_a_status_and_no_id() {
        // The phones' pair and the status beacon carry the WardriveESP status and no id.
        let mut status = [0; STATUS_MAX];
        let len = beacon::status(&mut status, &Beacon::default(), [0x44, 0xC0], false);
        let mut props = PeripheralProperties::default();
        props.manufacturer_data.insert(COMPANY, status[..len].to_vec());
        assert_eq!(Read::of(props).octets, None);
    }

    #[test]
    fn ble_read_finds_no_octets_when_only_the_name_says_bridge() {
        // A BlueZ restarted since it last heard the bridge lists the name and no service data.
        let props = PeripheralProperties {
            local_name: Some("wartui 44:C0".to_owned()),
            ..PeripheralProperties::default()
        };
        assert_eq!(Read::of(props).octets, None);
    }

    #[test]
    fn ble_rationing_leaves_device_alone_when_reads_without_id_reach_the_cap() {
        let mut rationing = Rationing::default();
        for _ in 1..ID_READS {
            assert_eq!(rationing.noted(1, "peer".to_owned(), unidentified(true)), None);
            assert!(rationing.wants(&1, false));
        }
        rationing.noted(1, "peer".to_owned(), unidentified(true));
        assert!(!rationing.wants(&1, false), "left alone after {ID_READS} reads");
    }

    #[test]
    fn ble_rationing_reads_device_again_when_heard_with_the_id() {
        // BlueZ lists a bonded bridge before it has heard its service data, so the id can arrive
        // after the reads ran out.
        let mut rationing = Rationing::default();
        for _ in 0..ID_READS {
            rationing.noted(1, "peer".to_owned(), unidentified(false));
        }
        assert!(rationing.wants(&1, true), "the service-data event reads it");
        let noted = rationing.noted(1, "peer".to_owned(), read([0x44, 0xC0], true));
        assert_eq!(noted, sighting([0x44, 0xC0], None, true));
    }

    #[test]
    fn ble_rationing_identifies_bridge_when_a_later_read_carries_the_id() {
        let mut rationing = Rationing::default();
        assert_eq!(rationing.noted(1, "peer".to_owned(), unidentified(true)), None);
        let noted = rationing.noted(1, "peer".to_owned(), read([0x44, 0xC0], false));
        assert_eq!(noted, sighting([0x44, 0xC0], None, false));
    }

    #[test]
    fn ble_rationing_wants_identified_bridge_when_no_read_has_found_it_live() {
        // BlueZ replays a bonded bridge as discovered before the scan has heard it.
        let mut rationing = Rationing::default();
        rationing.noted(1, "peer".to_owned(), read([0x44, 0xC0], false));
        assert!(rationing.wants(&1, false), "a bridge not heard advertising is read again");
        rationing.noted(1, "peer".to_owned(), read([0x44, 0xC0], true));
        assert!(!rationing.wants(&1, false), "nor once it has been");
        assert!(!rationing.wants(&1, true), "even when heard with the id");
    }

    #[test]
    fn ble_rationing_keeps_octets_when_a_later_read_carries_no_id() {
        let mut rationing = Rationing::default();
        rationing.noted(1, "peer".to_owned(), read([0x44, 0xC0], false));
        let named = Read { octets: None, name: Some("wartui 44:C0".to_owned()), live: true };
        let noted = rationing.noted(1, "peer".to_owned(), Some(named));
        assert_eq!(noted, sighting([0x44, 0xC0], Some("wartui 44:C0"), true));
    }
}
