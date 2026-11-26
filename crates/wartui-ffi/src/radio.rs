//! The phone's Bluetooth stack, as the link driver's radio.
//!
//! `wartui_bridge::pipe` is the seam: the Bluetooth driver (`wartui_bridge::ble`) is the link's
//! protocol, and runs on a `pipe::Radio`. [`Radio`] is that radio over a phone's stack, and owes
//! the driver every point of the adapter contract in `pipe`'s `//!`.
//!
//! **A phone's stack answers later.** `BluetoothGattCallback` and `CBPeripheralDelegate` take a
//! request, return at once and report its result on another thread. So the phone implements
//! [`RadioDriver`], whose requests carry a `request` id and return at once, and reports each
//! result through [`Radio::completed`]. Each request waits on a `oneshot` kept under its id. A
//! request the driver gave up on (its future dropped, on the driver's own timeout) leaves the map,
//! and a completion for an id no longer waiting is ignored. A driver method that throws fails its
//! request at once, as does a completion of the wrong shape for it, each as
//! [`RadioErrorKind::Other`] with the detail.
//!
//! **What the contract asks, and how this meets it:**
//!
//! - A sighting is only of a bridge. [`Heard::id`] is read for the bridge's octets. One that
//!   carries none is a sighting only of a peer this radio has already read an id from, since a
//!   phone's listing of bonded or connected peers holds no advertisement; any other is dropped.
//! - A scan yields a bridge when first heard, and again when what is known of it changes. Its name
//!   is the last one heard, and it is live once heard advertising in this scan. A repeat that
//!   changes neither is dropped. [`Radio::heard`] outside a scan is dropped too.
//! - A subscription's stream is made before the driver is asked, so nothing sent on its answer is
//!   missed. Notifications and the drop share one ordered channel, so a notification that came
//!   before the drop is yielded first. [`Radio::dropped`] ends the stream, once.
//! - `mtu` is never below 23 and reads 23 when the driver throws. `is_connected` reads `false`
//!   when it throws, and `pairs_on_connect` reads `true`.
//!
//! **Nothing here retries, times out or decides.** The driver in `wartui-bridge` does, so a phone
//! and a desktop follow one policy. No lock is held across a call into the phone's code.
//!
//! **[`bridges_nearby`] is a picker's listen**, `wartui_bridge::ble::survey` on the library's
//! runtime: every bridge heard or listed, none chosen. A radio holds one scan, so a link searching
//! over the same radio meanwhile loses its scan, and the survey or the link's try fails.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use futures_util::{FutureExt as _, StreamExt as _};
use tokio::sync::{mpsc, oneshot};
use wartui_bridge::pipe::{
    self, Connected, Connection, Incoming, RadioError, RadioErrorKind, RadioTraits, Recalled,
    Sighting, WriteKind,
};
use wartui_proto::ble::{DOWN_UUID, ID_UUID, SERVICE_UUID, UP_UUID, advertised_octets};
use wartui_proto::mac;

use crate::runtime::spawn;

/// The smallest ATT MTU, which the driver assumes when the OS says nothing.
const MIN_MTU: u16 = 23;

/// What kind of failure a [`RadioError`] is. The driver acts on the kind.
#[uniffi::remote(Enum)]
pub enum RadioErrorKind {
    /// The OS has no Bluetooth adapter to use.
    NoAdapter,
    /// The peer's ATT Insufficient Authentication or Encryption answer to a subscription or a
    /// write request.
    NotBonded,
    /// The peer's link dropped.
    Disconnected,
    /// The peer offers no wartui link service.
    NoService,
    /// The OS's Bluetooth is turned off.
    PoweredOff,
    /// The OS does not let this app use Bluetooth.
    Unauthorized,
    /// The OS holds a bond for the peer, and the peer answers that it holds none.
    PeerForgotBond,
    /// Anything else, in the stack's words.
    Other,
}

/// A radio call that failed.
#[uniffi::remote(Record)]
pub struct RadioError {
    /// What the driver acts on.
    pub kind: RadioErrorKind,
    /// The stack's words.
    pub text: String,
}

/// The phone's code threw from a [`RadioDriver`] method. The request it made fails.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum DriverError {
    #[error("the radio driver failed: {detail}")]
    Failed { detail: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for DriverError {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed { detail: error.reason }
    }
}

/// What a phone's Bluetooth stack does for the link. Every method returns at once. A method that
/// takes a `request` answers later, exactly once, through [`Radio::completed`] with that id, from
/// any thread. Calls arrive on arbitrary threads.
///
/// A notification or a drop is reported for the peer's current link only, since [`Radio`] finds
/// the subscription by peer alone. Ignore the callbacks of a link closed, or one the OS replaced:
/// on Android those of an old `BluetoothGatt`, on iOS those for a peripheral since reconnected.
/// Report a link's drop before answering a later `connect` to the same peer.
#[uniffi::export(with_foreign)]
pub trait RadioDriver: Send + Sync {
    /// Whether the OS pairs on the first connection behind a dialog of its own (iOS and Android
    /// both do).
    fn pairs_on_connect(&self) -> Result<bool, DriverError>;
    /// Whether the OS knows the peer `key` without a scan. Answers [`Completion::Recalled`].
    fn recall(&self, request: u64, key: String) -> Result<(), DriverError>;
    /// The peers the OS lists now (connected or bonded). Answers [`Completion::Known`].
    fn known(&self, request: u64) -> Result<(), DriverError>;
    /// Start an unfiltered scan, reporting each device that carries the id through
    /// [`Radio::heard`]. Answers [`Completion::Done`] once scanning.
    fn start_scan(&self, request: u64) -> Result<(), DriverError>;
    /// Stop the scan.
    fn stop_scan(&self) -> Result<(), DriverError>;
    /// Connect to `peer`. Answers [`Completion::Connected`]. On Android, ask for a larger MTU
    /// before answering.
    fn connect(&self, request: u64, peer: String) -> Result<(), DriverError>;
    /// Cancel a connect to `peer` that has not completed.
    fn abandon(&self, peer: String) -> Result<(), DriverError>;
    /// Find the link service's Up and Down on `peer`. Answers [`Completion::Done`], or
    /// [`Completion::Failed`] with [`RadioErrorKind::NoService`].
    fn discover(&self, request: u64, peer: String) -> Result<(), DriverError>;
    /// Subscribe to Up. Answers [`Completion::Done`]; from then each notification goes to
    /// [`Radio::notified`] and the link's drop to [`Radio::dropped`].
    fn subscribe(&self, request: u64, peer: String) -> Result<(), DriverError>;
    /// The ATT MTU of the link to `peer`, 23 when the OS does not say.
    fn mtu(&self, peer: String) -> Result<u16, DriverError>;
    /// Write `bytes` to Down. Answers [`Completion::Done`] once the OS has taken it (without
    /// response) or the peer has answered (with response).
    fn write(
        &self,
        request: u64,
        peer: String,
        bytes: Vec<u8>,
        with_response: bool,
    ) -> Result<(), DriverError>;
    /// Whether the OS still holds the link to `peer`.
    fn is_connected(&self, peer: String) -> Result<bool, DriverError>;
    /// Unsubscribe from Up when `unsubscribe`, then disconnect a link that is still up.
    fn close(&self, peer: String, unsubscribe: bool) -> Result<(), DriverError>;
}

/// The answer to one [`RadioDriver`] request.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum Completion {
    /// Done: `start_scan`, `discover`, `subscribe` and `write`.
    Done,
    /// Whether the OS knows the key `recall` asked for.
    Recalled { known: bool },
    /// The peers `known` lists.
    Known { bridges: Vec<Heard> },
    /// `connect` holds a link. `made`: this call made it, rather than finding one the OS held.
    Connected { made: bool },
    /// The request failed.
    Failed { error: RadioError },
}

impl Completion {
    const fn name(&self) -> &'static str {
        match self {
            Self::Done => "Done",
            Self::Recalled { .. } => "Recalled",
            Self::Known { .. } => "Known",
            Self::Connected { .. } => "Connected",
            Self::Failed { .. } => "Failed",
        }
    }
}

/// A device the OS heard or lists.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct Heard {
    /// The key the OS knows it by: an address on Android, the peripheral's identifier on iOS.
    /// Stable across scans and runs, since the settings remember it.
    pub peer: String,
    /// The service data it advertises under the id UUID ([`LinkUuids::id`]), when the OS has it.
    pub id: Option<Vec<u8>>,
    /// The name it advertises, when the OS heard one: a label, never matched.
    pub name: Option<String>,
    /// Heard advertising now, rather than only listed.
    pub live: bool,
}

/// The UUIDs a phone's adapter needs, so none is written twice.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct LinkUuids {
    /// The wartui link service.
    pub service: String,
    /// Its Up characteristic, notify: the bridge's frame stream.
    pub up: String,
    /// Its Down characteristic, written with or without response: the host's frames.
    pub down: String,
    /// The 16-bit UUID of the service data that marks a bridge and carries its last two octets.
    pub id: u16,
}

/// The link service's UUIDs, lowercase, as `f2410001-d870-…`.
#[uniffi::export]
#[must_use]
pub fn link_uuids() -> LinkUuids {
    LinkUuids { service: uuid(SERVICE_UUID), up: uuid(UP_UUID), down: uuid(DOWN_UUID), id: ID_UUID }
}

/// `value` in the canonical 8-4-4-4-12 form, lowercase.
fn uuid(value: u128) -> String {
    let hex = format!("{value:032x}");
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

/// A bridge the radio hears or the OS lists.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BridgeNearby {
    /// The key to connect by.
    pub peer: String,
    /// The last two octets of its address, as `44:C0`: what [`Link::bluetooth`] takes as
    /// `bridge`.
    ///
    /// [`Link::bluetooth`]: crate::Link::bluetooth
    pub tail: String,
    /// The name it advertises, when the OS heard one: a label, never matched.
    pub name: Option<String>,
    /// Heard advertising now, rather than only listed.
    pub live: bool,
}

/// Why [`bridges_nearby`] listed nothing: the radio's kind where it names a remedy.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum NearbyError {
    /// The OS has no Bluetooth adapter to use.
    #[error("no Bluetooth adapter")]
    NoAdapter,
    /// The OS's Bluetooth is turned off.
    #[error("Bluetooth is off")]
    PoweredOff,
    /// The OS does not let this app use Bluetooth.
    #[error("Bluetooth is not authorized")]
    Unauthorized,
    /// Anything else: the stack's failure, or a call it did not answer in time.
    #[error("the survey failed: {detail}")]
    Failed { detail: String },
}

impl From<RadioError> for NearbyError {
    fn from(error: RadioError) -> Self {
        match error.kind {
            RadioErrorKind::NoAdapter => Self::NoAdapter,
            RadioErrorKind::PoweredOff => Self::PoweredOff,
            RadioErrorKind::Unauthorized => Self::Unauthorized,
            RadioErrorKind::NotBonded
            | RadioErrorKind::Disconnected
            | RadioErrorKind::NoService
            | RadioErrorKind::PeerForgotBond
            | RadioErrorKind::Other => Self::Failed { detail: error.text },
        }
    }
}

/// Listen for `for_ms` and list every bridge heard or listed, for a picker, sorted by octets.
///
/// # Errors
/// Why the radio could not listen or list.
#[uniffi::export]
pub async fn bridges_nearby(
    radio: Arc<Radio>,
    for_ms: u32,
) -> Result<Vec<BridgeNearby>, NearbyError> {
    let listen_for = Duration::from_millis(u64::from(for_ms));
    let surveyed = spawn(async move { wartui_bridge::ble::survey(&*radio, listen_for).await });
    let heard = match surveyed.await {
        Ok(heard) => heard?,
        Err(error) => return Err(NearbyError::Failed { detail: error.to_string() }),
    };
    Ok(heard
        .into_iter()
        .map(|Sighting { peer, octets, name, live }| BridgeNearby {
            peer,
            tail: mac::octets(&octets).to_string(),
            name,
            live,
        })
        .collect())
}

/// The phone's Bluetooth stack as the link driver's radio. The phone reports what its stack heard
/// and answered through the methods here.
#[derive(uniffi::Object)]
pub struct Radio {
    inner: Arc<Inner>,
}

struct Inner {
    driver: Arc<dyn RadioDriver>,
    /// The last request id given; the first is 1.
    requests: AtomicU64,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The requests waiting on their answer.
    pending: HashMap<u64, oneshot::Sender<Completion>>,
    /// The octets each peer's id carried, from every [`Heard`] that had one.
    octets: HashMap<String, [u8; 2]>,
    /// The scan under way.
    scan: Option<Scan>,
    /// Each peer's subscription.
    subscriptions: HashMap<String, Subscription>,
    /// The last scan or connection number given, so a late cleanup leaves a newer one alone.
    serial: u64,
}

/// A scan under way.
struct Scan {
    serial: u64,
    sightings: mpsc::UnboundedSender<Sighting>,
    /// What this scan has yielded of each peer.
    seen: HashMap<String, Sighting>,
}

/// A subscription to one peer's Up.
struct Subscription {
    /// The connection that made it.
    connection: u64,
    incoming: mpsc::UnboundedSender<Incoming>,
}

/// What a request asked for, named in a failure.
#[derive(Debug, Clone, Copy)]
enum Asked {
    Recall,
    Known,
    StartScan,
    Connect,
    Discover,
    Subscribe,
    Write,
}

impl fmt::Display for Asked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Recall => "recall",
            Self::Known => "known",
            Self::StartScan => "start_scan",
            Self::Connect => "connect",
            Self::Discover => "discover",
            Self::Subscribe => "subscribe",
            Self::Write => "write",
        })
    }
}

#[uniffi::export]
impl Radio {
    /// A radio over `driver`, the phone's Bluetooth stack.
    #[uniffi::constructor]
    pub fn new(driver: Arc<dyn RadioDriver>) -> Arc<Self> {
        let inner = Inner { driver, requests: AtomicU64::new(0), state: Mutex::default() };
        Arc::new(Self { inner: Arc::new(inner) })
    }

    /// A device heard by the scan.
    pub fn heard(&self, device: Heard) {
        self.inner.heard(device);
    }

    /// The answer to the request `request`. One for a request no longer waiting is ignored.
    pub fn completed(&self, request: u64, outcome: Completion) {
        let waiting = self.inner.lock().pending.remove(&request);
        match waiting {
            // The driver may have given up between the two; nothing waits then.
            Some(reply) => drop(reply.send(outcome)),
            None => tracing::debug!(request, "a completion for no waiting request"),
        }
    }

    /// One notification's value from `peer`'s Up, on its current link.
    pub fn notified(&self, peer: String, bytes: Vec<u8>) {
        let state = self.inner.lock();
        if let Some(subscription) = state.subscriptions.get(&peer) {
            // A stream the driver let go of takes nothing.
            let _ = subscription.incoming.send(Incoming::Notified(bytes));
        }
    }

    /// The link to `peer` dropped. A second drop, or one for a peer with no subscription, is
    /// ignored. Report it for the peer's current link only, and before answering a later
    /// `connect` to it: a late drop for a link since replaced ends the new one's subscription.
    pub fn dropped(&self, peer: String, error: RadioError) {
        let subscription = self.inner.lock().subscriptions.remove(&peer);
        if let Some(subscription) = subscription {
            // Its sender goes with it, which ends the stream after the drop.
            let _ = subscription.incoming.send(Incoming::Dropped(error));
        }
    }
}

impl fmt::Debug for Radio {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Radio").finish_non_exhaustive()
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Every write leaves the maps whole, so a panic elsewhere leaves nothing torn.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Ask the driver through `call` with a fresh request id, and wait for its answer. A
    /// [`Completion::Failed`] is the error.
    async fn ask(
        &self,
        asked: Asked,
        call: impl FnOnce(&dyn RadioDriver, u64) -> Result<(), DriverError>,
    ) -> Result<Completion, RadioError> {
        let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, answer) = oneshot::channel();
        // Before the call, since the answer may come on another thread before it returns.
        self.lock().pending.insert(request, reply);
        let waiting = Waiting { inner: self, request };
        if let Err(DriverError::Failed { detail }) = call(self.driver.as_ref(), request) {
            return Err(RadioError::new(RadioErrorKind::Other, format!("{asked}: {detail}")));
        }
        let outcome = answer.await;
        drop(waiting);
        match outcome {
            Ok(Completion::Failed { error }) => Err(error),
            Ok(outcome) => Ok(outcome),
            // Only the map holds the sender, and only `completed` takes it out.
            Err(_) => Err(RadioError::new(RadioErrorKind::Other, format!("{asked}: unanswered"))),
        }
    }

    /// A peer's sighting, from `device`, when it is a bridge: it carries an id, or a peer this
    /// radio has read one from. Notes the octets an id carries.
    fn sighting(&self, state: &mut State, device: Heard) -> Option<Sighting> {
        let peer = device.peer;
        let octets = match advertised_octets(device.id.as_deref()) {
            Some(octets) => {
                state.octets.insert(peer.clone(), octets);
                octets
            }
            None => *state.octets.get(&peer)?,
        };
        Some(Sighting { peer, octets, name: device.name, live: device.live })
    }

    fn heard(&self, device: Heard) {
        let mut state = self.lock();
        let Some(heard) = self.sighting(&mut state, device) else { return };
        let Some(scan) = &mut state.scan else { return };
        // What is known of it: the last name heard, and live once heard advertising.
        let known = match scan.seen.get(&heard.peer) {
            Some(seen) => Sighting {
                name: heard.name.or_else(|| seen.name.clone()),
                live: heard.live || seen.live,
                ..heard
            },
            None => heard,
        };
        if scan.seen.get(&known.peer) == Some(&known) {
            return;
        }
        scan.seen.insert(known.peer.clone(), known.clone());
        // A scan the driver stopped reading takes nothing.
        let _ = scan.sightings.send(known);
    }

    fn next_serial(state: &mut State) -> u64 {
        state.serial += 1;
        state.serial
    }
}

/// A request waiting on its answer. Dropped, by an answer or by the driver giving up, it leaves
/// the map.
struct Waiting<'a> {
    inner: &'a Inner,
    request: u64,
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.inner.lock().pending.remove(&self.request);
    }
}

/// `got` answered a request for `asked`, which takes another shape.
fn wrong(asked: Asked, got: &Completion) -> RadioError {
    RadioError::new(RadioErrorKind::Other, format!("{asked}: answered with {}", got.name()))
}

impl pipe::Radio for Radio {
    fn traits(&self) -> RadioTraits {
        // Both phone OSes pair on the connection, so a driver that throws is read as one.
        let pairs_on_connect = self.inner.driver.pairs_on_connect().unwrap_or(true);
        RadioTraits { pairs_on_connect, macos: false }
    }

    fn recall<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Recalled>, RadioError>> {
        async move {
            let asked = Asked::Recall;
            match self.inner.ask(asked, |driver, id| driver.recall(id, key.to_owned())).await? {
                Completion::Recalled { known: true } => {
                    let octets = self.inner.lock().octets.get(key).copied();
                    Ok(Some(Recalled { peer: key.to_owned(), octets }))
                }
                Completion::Recalled { known: false } => Ok(None),
                other => Err(wrong(asked, &other)),
            }
        }
        .boxed()
    }

    fn scan(&self) -> BoxFuture<'_, Result<BoxStream<'static, Sighting>, RadioError>> {
        async move {
            let (sightings, heard) = mpsc::unbounded_channel();
            let serial = {
                let mut state = self.inner.lock();
                let serial = Inner::next_serial(&mut state);
                state.scan = Some(Scan { serial, sightings, seen: HashMap::new() });
                serial
            };
            let asked = Asked::StartScan;
            let failed = match self.inner.ask(asked, |driver, id| driver.start_scan(id)).await {
                Ok(Completion::Done) => None,
                Ok(other) => Some(wrong(asked, &other)),
                Err(error) => Some(error),
            };
            if let Some(error) = failed {
                let mut state = self.inner.lock();
                if state.scan.as_ref().is_some_and(|scan| scan.serial == serial) {
                    state.scan = None;
                }
                return Err(error);
            }
            Ok(receive(heard))
        }
        .boxed()
    }

    fn stop_scan(&self) -> BoxFuture<'_, ()> {
        async move {
            let scan = self.inner.lock().scan.take();
            // A driver that throws has stopped as far as the link can tell.
            let _ = self.inner.driver.stop_scan();
            drop(scan);
        }
        .boxed()
    }

    fn known(&self) -> BoxFuture<'_, Result<Vec<Sighting>, RadioError>> {
        async move {
            let asked = Asked::Known;
            match self.inner.ask(asked, |driver, id| driver.known(id)).await? {
                Completion::Known { bridges } => {
                    let mut state = self.inner.lock();
                    let listed = bridges.into_iter();
                    Ok(listed
                        .filter_map(|device| self.inner.sighting(&mut state, device))
                        .collect())
                }
                other => Err(wrong(asked, &other)),
            }
        }
        .boxed()
    }

    fn connect<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, Result<Connected, RadioError>> {
        async move {
            let asked = Asked::Connect;
            match self.inner.ask(asked, |driver, id| driver.connect(id, peer.to_owned())).await? {
                Completion::Connected { made } => {
                    let serial = Inner::next_serial(&mut self.inner.lock());
                    let connection =
                        PeerLink { radio: Arc::clone(&self.inner), peer: peer.to_owned(), serial };
                    Ok(Connected { connection: Box::new(connection), made })
                }
                other => Err(wrong(asked, &other)),
            }
        }
        .boxed()
    }

    fn abandon<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, ()> {
        async move {
            let _ = self.inner.driver.abandon(peer.to_owned());
        }
        .boxed()
    }
}

/// A stream of what `receiver` is sent, ending once every sender has gone.
fn receive<T: Send + 'static>(receiver: mpsc::UnboundedReceiver<T>) -> BoxStream<'static, T> {
    stream::unfold(receiver, |mut receiver| async move {
        let item = receiver.recv().await?;
        Some((item, receiver))
    })
    .boxed()
}

/// One link [`pipe::Radio::connect`] made.
struct PeerLink {
    radio: Arc<Inner>,
    peer: String,
    /// Which connection this is, so closing it leaves a later one's subscription alone.
    serial: u64,
}

impl PeerLink {
    /// Drop this connection's subscription, ending its stream.
    fn unsubscribed(&self) {
        let mut state = self.radio.lock();
        if state.subscriptions.get(&self.peer).is_some_and(|sub| sub.connection == self.serial) {
            state.subscriptions.remove(&self.peer);
        }
    }

    /// Ask for `asked`, which answers [`Completion::Done`].
    async fn done(
        &self,
        asked: Asked,
        call: impl FnOnce(&dyn RadioDriver, u64) -> Result<(), DriverError>,
    ) -> Result<(), RadioError> {
        match self.radio.ask(asked, call).await? {
            Completion::Done => Ok(()),
            other => Err(wrong(asked, &other)),
        }
    }
}

impl Connection for PeerLink {
    fn discover(&mut self) -> BoxFuture<'_, Result<(), RadioError>> {
        async move {
            let peer = self.peer.clone();
            self.done(Asked::Discover, |driver, id| driver.discover(id, peer)).await
        }
        .boxed()
    }

    fn subscribe(&mut self) -> BoxFuture<'_, Result<BoxStream<'static, Incoming>, RadioError>> {
        async move {
            // Unbounded: the transport drains it into the link's events, which a capture or a
            // parked link's task always reads (`wartui_app::ops::park`), so the link's own rate
            // bounds it.
            let (incoming, received) = mpsc::unbounded_channel();
            let subscription = Subscription { connection: self.serial, incoming };
            // Before the request, so nothing sent on its answer is missed.
            self.radio.lock().subscriptions.insert(self.peer.clone(), subscription);
            let peer = self.peer.clone();
            match self.done(Asked::Subscribe, |driver, id| driver.subscribe(id, peer)).await {
                Ok(()) => Ok(receive(received)),
                Err(error) => {
                    self.unsubscribed();
                    Err(error)
                }
            }
        }
        .boxed()
    }

    fn mtu(&self) -> u16 {
        self.radio.driver.mtu(self.peer.clone()).map_or(MIN_MTU, |mtu| mtu.max(MIN_MTU))
    }

    fn write<'a>(
        &'a mut self,
        bytes: &'a [u8],
        kind: WriteKind,
    ) -> BoxFuture<'a, Result<(), RadioError>> {
        async move {
            let with_response = kind == WriteKind::Request;
            let (peer, bytes) = (self.peer.clone(), bytes.to_vec());
            let call = |driver: &dyn RadioDriver, id| driver.write(id, peer, bytes, with_response);
            self.done(Asked::Write, call).await
        }
        .boxed()
    }

    fn is_connected(&self) -> BoxFuture<'_, bool> {
        async move { self.radio.driver.is_connected(self.peer.clone()).unwrap_or(false) }.boxed()
    }

    fn close(self: Box<Self>, unsubscribe: bool) -> BoxFuture<'static, ()> {
        async move {
            // A driver that throws has let the peer go as far as the link can tell.
            let _ = self.radio.driver.close(self.peer.clone(), unsubscribe);
            self.unsubscribed();
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::link_uuids;

    #[test]
    fn link_uuids_spell_the_service_when_formatted() {
        let uuids = link_uuids();
        assert_eq!(uuids.service, "f2410001-d870-4eb6-a5a0-ed645732a877");
        assert_eq!(uuids.up, "f2410002-d870-4eb6-a5a0-ed645732a877");
        assert_eq!(uuids.down, "f2410003-d870-4eb6-a5a0-ed645732a877");
        assert_eq!(uuids.id, 0x5754);
    }
}
