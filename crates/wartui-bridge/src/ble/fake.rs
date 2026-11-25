//! A [`Radio`] a test scripts, watches and steers, so the driver's finding, connecting,
//! subscribing and serving run against something that answers as a bridge does, or fails as an
//! OS does.
//!
//! Everything runs on tokio's channels and clock, with no thread and no real sleep, so a test runs
//! on a paused clock and reads exact times. Each bridge decodes the host frames written to Down
//! and, when told to, answers each `Identify` with a `Ready`. A test reads back what the driver
//! did in [`FakeRadio::did`].

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::stream::{self, BoxStream};
use futures_util::{FutureExt as _, StreamExt as _};
use tokio::sync::mpsc;
use tokio::time::Instant;
use wartui_proto::ble::ATT_MTU;
use wartui_proto::bus::NodeBoard;
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LogStr, LoopPhase, MAX_FRAME, ShortStr,
    USB_TOOK_BRIDGE, decode_frame, encode_frame,
};
use wartui_proto::mac::Mac;
use wartui_proto::reset::ResetCause;

use crate::pipe::{
    Connected, Connection, Incoming, Radio, RadioError, RadioErrorKind, RadioTraits, Recalled,
    Sighting, WriteKind,
};

/// The OS a fake radio stands for when a test names none: one that pairs beforehand, as BlueZ.
pub(super) const LINUX: RadioTraits = RadioTraits { pairs_on_connect: false, macos: false };

/// macOS, which pairs on the connection behind a dialog of its own.
pub(super) const MACOS: RadioTraits = RadioTraits { pairs_on_connect: true, macos: true };

/// How a connect to a fake bridge goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Connects {
    /// It completes at once.
    Completes,
    /// It fails, in these words.
    Fails(String),
    /// It never completes, as CoreBluetooth's to a bridge that dropped this host's bond.
    Never,
    /// The OS already holds the link, so the call makes none.
    AlreadyHeld,
}

/// How a fake bridge's service discovery goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Discovers {
    /// It offers the link service.
    Found,
    /// It offers none.
    NoService,
}

/// How a fake bridge answers the subscription to Up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Subscribes {
    /// At once.
    Answers,
    /// After this long, as an OS behind its pairing dialog.
    After(Duration),
    /// With the C6's refusal for want of a bond.
    NotBonded,
    /// Never.
    Never,
}

/// How a fake bridge answers a write request. A write without response always returns at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Writes {
    /// At once.
    Answered,
    /// Never.
    NeverAnswered,
    /// With the C6's refusal for want of a bond, and the write's bytes are not taken.
    Refused,
}

/// One fake bridge, as a test describes it.
#[derive(Debug, Clone)]
pub(super) struct FakeBridge {
    /// The key the fake OS knows it by.
    pub(super) peer: String,
    /// The address its `Ready` names, whose last two octets its id carries.
    pub(super) mac: Mac,
    /// The address its `Ready` names, when another than `mac`.
    pub(super) ready_mac: Option<Mac>,
    pub(super) name: Option<String>,
    /// Heard advertising in a scan, this long after it starts.
    pub(super) advertises: Option<Duration>,
    /// Listed by the OS, and so reported by `known`.
    pub(super) listed: bool,
    /// Known by its peer key without a scan.
    pub(super) recalled: bool,
    pub(super) connects: Connects,
    pub(super) discovers: Discovers,
    pub(super) subscribes: Subscribes,
    pub(super) writes: Writes,
    /// Answers each `Identify` with a `Ready` after ignoring this many; `None` never answers.
    pub(super) answers: Option<u32>,
}

impl FakeBridge {
    /// A bridge at `mac`, known as `peer`, heard advertising at once, that takes every call and
    /// answers every `Identify`.
    pub(super) fn new(peer: &str, mac: Mac) -> Self {
        Self {
            peer: peer.to_owned(),
            mac,
            ready_mac: None,
            name: None,
            advertises: Some(Duration::ZERO),
            listed: false,
            recalled: false,
            connects: Connects::Completes,
            discovers: Discovers::Found,
            subscribes: Subscribes::Answers,
            writes: Writes::Answered,
            answers: Some(0),
        }
    }

    fn octets(&self) -> [u8; 2] {
        [self.mac[4], self.mac[5]]
    }
}

/// What the driver did, in order.
// Large because a host frame carries an inline node record. A test's record is short.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Did {
    Recall(String),
    ScanStarted,
    ScanStopped,
    Known,
    Connect(String),
    Abandon(String),
    Write(Vec<u8>, WriteKind),
    /// A host frame a bridge decoded from Down.
    Frame(HostToBridge),
    Close {
        unsubscribe: bool,
    },
}

/// One link to a bridge.
#[derive(Debug)]
struct Link {
    bridge: usize,
    /// Whether the OS holds it.
    up: bool,
    /// Up's notifications and the drop, once subscribing.
    notify: Option<mpsc::UnboundedSender<Incoming>>,
    /// The host's bytes on Down, framed.
    acc: FrameAccumulator<MAX_FRAME>,
}

#[derive(Debug)]
struct Shared {
    traits: RadioTraits,
    no_adapter: bool,
    bridges: Vec<FakeBridge>,
    /// `Identify`s each bridge has heard, against its `answers`.
    identifies: Vec<u32>,
    links: Vec<Link>,
    /// The running scan's sightings.
    sightings: Option<mpsc::UnboundedSender<Sighting>>,
    did: Vec<(Instant, Did)>,
    /// When the fake was made, which every `Ready`'s uptime counts from.
    booted: Instant,
}

/// Fake bridges and the radio that reaches them, shared by every clone, so a test keeps one to
/// watch and steer.
#[derive(Debug, Clone)]
pub(super) struct FakeRadio {
    shared: Arc<Mutex<Shared>>,
}

impl FakeRadio {
    /// A radio on the OS `traits` describe, with no bridge in range.
    pub(super) fn new(traits: RadioTraits) -> Self {
        let shared = Shared {
            traits,
            no_adapter: false,
            bridges: Vec::new(),
            identifies: Vec::new(),
            links: Vec::new(),
            sightings: None,
            did: Vec::new(),
            booted: Instant::now(),
        };
        Self { shared: Arc::new(Mutex::new(shared)) }
    }

    /// A radio whose OS has no adapter: every call fails so.
    pub(super) fn without_adapter() -> Self {
        let radio = Self::new(LINUX);
        radio.lock().no_adapter = true;
        radio
    }

    /// Put `bridge` in range.
    pub(super) fn add(&self, bridge: FakeBridge) -> &Self {
        let mut shared = self.lock();
        shared.bridges.push(bridge);
        shared.identifies.push(0);
        self
    }

    /// As a trait object, the way the transport takes it.
    pub(super) fn radio(&self) -> Arc<dyn Radio> {
        Arc::new(self.clone())
    }

    /// What the driver did, in order.
    pub(super) fn did(&self) -> Vec<Did> {
        self.lock().did.iter().map(|(_, did)| did.clone()).collect()
    }

    /// What the driver did, each with when, in order.
    pub(super) fn did_at(&self) -> Vec<(Instant, Did)> {
        self.lock().did.clone()
    }

    /// The host frames the bridges decoded, in order.
    pub(super) fn frames(&self) -> Vec<HostToBridge> {
        self.did()
            .into_iter()
            .filter_map(|did| match did {
                Did::Frame(frame) => Some(frame),
                _ => None,
            })
            .collect()
    }

    /// When each connect was asked for.
    pub(super) fn connects_at(&self) -> Vec<Instant> {
        self.did_at()
            .into_iter()
            .filter_map(|(at, did)| matches!(did, Did::Connect(_)).then_some(at))
            .collect()
    }

    /// Notify `bytes` on Up of `peer`'s link, as one notification.
    pub(super) fn notify(&self, peer: &str, bytes: &[u8]) {
        let shared = self.lock();
        if let Some(notify) = shared.link_of(peer).and_then(|link| link.notify.as_ref()) {
            let _ = notify.send(Incoming::Notified(bytes.to_vec()));
        }
    }

    /// Send the P4's notice that a USB host took the bridge, as one notification.
    pub(super) fn take_over(&self, peer: &str) {
        let takeover = BridgeToHost::Error { message: LogStr::try_from(USB_TOOK_BRIDGE).unwrap() };
        self.notify(peer, &frame(&takeover));
    }

    /// Drop `peer`'s link, as the peer does.
    pub(super) fn drop_link(&self, peer: &str) {
        let mut shared = self.lock();
        if let Some(link) = shared.link_of_mut(peer) {
            link.up = false;
            if let Some(notify) = link.notify.take() {
                let dropped = RadioError::new(RadioErrorKind::Disconnected, "disconnected");
                let _ = notify.send(Incoming::Dropped(dropped));
            }
        }
    }

    /// Lose `peer`'s link with no event: only asking the OS says so.
    pub(super) fn lose_link(&self, peer: &str) {
        if let Some(link) = self.lock().link_of_mut(peer) {
            link.up = false;
        }
    }

    /// Change how `peer` answers write requests from now on.
    pub(super) fn set_writes(&self, peer: &str, writes: Writes) {
        let mut shared = self.lock();
        if let Some(bridge) = shared.bridges.iter_mut().find(|bridge| bridge.peer == peer) {
            bridge.writes = writes;
        }
    }

    /// Change how `peer` answers the subscription from now on.
    pub(super) fn set_subscribes(&self, peer: &str, subscribes: Subscribes) {
        let mut shared = self.lock();
        if let Some(bridge) = shared.bridges.iter_mut().find(|bridge| bridge.peer == peer) {
            bridge.subscribes = subscribes;
        }
    }

    /// End the running scan's sightings, as an adapter that stops reporting.
    pub(super) fn end_sightings(&self) {
        self.lock().sightings = None;
    }

    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    fn record(&self, did: Did) {
        self.lock().record(did);
    }

    /// `NoAdapter` when the OS has none.
    fn adapter(&self) -> Result<(), RadioError> {
        if self.lock().no_adapter {
            return Err(RadioError::new(RadioErrorKind::NoAdapter, "no Bluetooth adapter"));
        }
        Ok(())
    }
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Shared {
    fn record(&mut self, did: Did) {
        self.did.push((Instant::now(), did));
    }

    fn bridge(&self, peer: &str) -> Option<usize> {
        self.bridges.iter().position(|bridge| bridge.peer == peer)
    }

    /// The latest link to `peer`.
    fn link_of(&self, peer: &str) -> Option<&Link> {
        let bridge = self.bridge(peer)?;
        self.links.iter().rev().find(|link| link.bridge == bridge)
    }

    fn link_of_mut(&mut self, peer: &str) -> Option<&mut Link> {
        let bridge = self.bridge(peer)?;
        self.links.iter_mut().rev().find(|link| link.bridge == bridge)
    }

    fn sighting(&self, bridge: usize, live: bool) -> Sighting {
        let bridge = &self.bridges[bridge];
        Sighting {
            peer: bridge.peer.clone(),
            octets: bridge.octets(),
            name: bridge.name.clone(),
            live,
        }
    }
}

impl Radio for FakeRadio {
    fn traits(&self) -> RadioTraits {
        self.lock().traits
    }

    fn recall<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Recalled>, RadioError>> {
        async move {
            self.adapter()?;
            let mut shared = self.lock();
            shared.record(Did::Recall(key.to_owned()));
            let recalled =
                shared.bridges.iter().find(|bridge| bridge.recalled && bridge.peer == key);
            Ok(recalled.map(|bridge| Recalled {
                peer: bridge.peer.clone(),
                octets: Some(bridge.octets()),
            }))
        }
        .boxed()
    }

    fn scan(&self) -> BoxFuture<'_, Result<BoxStream<'static, Sighting>, RadioError>> {
        async move {
            self.adapter()?;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let mut shared = self.lock();
            shared.record(Did::ScanStarted);
            for bridge in 0..shared.bridges.len() {
                let Some(after) = shared.bridges[bridge].advertises else { continue };
                if after.is_zero() {
                    let _ = tx.send(shared.sighting(bridge, true));
                    continue;
                }
                let shared = Arc::clone(&self.shared);
                // This scan's own sender, held weakly: an advertisement due after the scan has
                // stopped is heard by no later scan.
                let tx = tx.downgrade();
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    if let Some(tx) = tx.upgrade() {
                        let _ = tx.send(lock(&shared).sighting(bridge, true));
                    }
                });
            }
            shared.sightings = Some(tx);
            let stream = stream::poll_fn(move |cx| rx.poll_recv(cx));
            Ok(stream.boxed())
        }
        .boxed()
    }

    fn stop_scan(&self) -> BoxFuture<'_, ()> {
        async move {
            let mut shared = self.lock();
            shared.record(Did::ScanStopped);
            shared.sightings = None;
        }
        .boxed()
    }

    fn known(&self) -> BoxFuture<'_, Result<Vec<Sighting>, RadioError>> {
        async move {
            self.adapter()?;
            let mut shared = self.lock();
            shared.record(Did::Known);
            let listed = (0..shared.bridges.len()).filter(|&b| shared.bridges[b].listed);
            Ok(listed.map(|bridge| shared.sighting(bridge, false)).collect())
        }
        .boxed()
    }

    fn connect<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, Result<Connected, RadioError>> {
        async move {
            self.adapter()?;
            let connects = {
                let mut shared = self.lock();
                shared.record(Did::Connect(peer.to_owned()));
                let Some(bridge) = shared.bridge(peer) else {
                    let unknown = format!("no Bluetooth device is known as {peer}");
                    return Err(RadioError::new(RadioErrorKind::Other, unknown));
                };
                shared.bridges[bridge].connects.clone()
            };
            let made = match connects {
                Connects::Completes => true,
                Connects::AlreadyHeld => false,
                Connects::Fails(text) => return Err(RadioError::new(RadioErrorKind::Other, text)),
                Connects::Never => std::future::pending().await,
            };
            let mut shared = self.lock();
            let bridge = shared.bridge(peer).expect("checked above");
            shared.links.push(Link {
                bridge,
                up: true,
                notify: None,
                acc: FrameAccumulator::new(),
            });
            let link = shared.links.len() - 1;
            let connection = FakeConnection { shared: Arc::clone(&self.shared), link };
            Ok(Connected { connection: Box::new(connection), made })
        }
        .boxed()
    }

    fn abandon<'a>(&'a self, peer: &'a str) -> BoxFuture<'a, ()> {
        async move { self.record(Did::Abandon(peer.to_owned())) }.boxed()
    }
}

/// A link [`FakeRadio::connect`] made.
struct FakeConnection {
    shared: Arc<Mutex<Shared>>,
    link: usize,
}

impl FakeConnection {
    fn lock(&self) -> MutexGuard<'_, Shared> {
        lock(&self.shared)
    }

    fn bridge(&self) -> FakeBridge {
        let shared = self.lock();
        shared.bridges[shared.links[self.link].bridge].clone()
    }
}

impl Connection for FakeConnection {
    fn discover(&mut self) -> BoxFuture<'_, Result<(), RadioError>> {
        async move {
            match self.bridge().discovers {
                Discovers::Found => Ok(()),
                Discovers::NoService => {
                    Err(RadioError::new(RadioErrorKind::NoService, "no link service"))
                }
            }
        }
        .boxed()
    }

    fn subscribe(&mut self) -> BoxFuture<'_, Result<BoxStream<'static, Incoming>, RadioError>> {
        async move {
            // Opened before the subscription is asked for, as the adapter's contract says.
            let (tx, mut rx) = mpsc::unbounded_channel();
            self.lock().links[self.link].notify = Some(tx);
            match self.bridge().subscribes {
                Subscribes::Answers => {}
                Subscribes::After(after) => tokio::time::sleep(after).await,
                Subscribes::NotBonded => {
                    let refused = "Insufficient Authentication";
                    return Err(RadioError::new(RadioErrorKind::NotBonded, refused));
                }
                Subscribes::Never => std::future::pending().await,
            }
            Ok(stream::poll_fn(move |cx| rx.poll_recv(cx)).boxed())
        }
        .boxed()
    }

    fn mtu(&self) -> u16 {
        ATT_MTU
    }

    fn write<'a>(
        &'a mut self,
        bytes: &'a [u8],
        kind: WriteKind,
    ) -> BoxFuture<'a, Result<(), RadioError>> {
        async move {
            let writes = {
                let mut shared = self.lock();
                shared.record(Did::Write(bytes.to_vec(), kind));
                if !shared.links[self.link].up {
                    return Err(RadioError::new(RadioErrorKind::Other, "not connected"));
                }
                let bridge = shared.links[self.link].bridge;
                let writes = shared.bridges[bridge].writes;
                if kind == WriteKind::Command || writes != Writes::Refused {
                    take(&mut shared, self.link, bytes);
                }
                writes
            };
            match (kind, writes) {
                (WriteKind::Command, _) | (WriteKind::Request, Writes::Answered) => Ok(()),
                (WriteKind::Request, Writes::NeverAnswered) => std::future::pending().await,
                (WriteKind::Request, Writes::Refused) => {
                    Err(RadioError::new(RadioErrorKind::NotBonded, "Insufficient Authentication"))
                }
            }
        }
        .boxed()
    }

    fn is_connected(&self) -> BoxFuture<'_, bool> {
        async move { self.lock().links[self.link].up }.boxed()
    }

    fn close(self: Box<Self>, unsubscribe: bool) -> BoxFuture<'static, ()> {
        async move {
            let mut shared = self.lock();
            shared.record(Did::Close { unsubscribe });
            let link = &mut shared.links[self.link];
            link.up = false;
            link.notify = None;
        }
        .boxed()
    }
}

/// A bridge takes `bytes` written to Down on `link`: it decodes each frame, and answers an
/// `Identify` as its `answers` says.
fn take(shared: &mut Shared, link: usize, bytes: &[u8]) {
    let bridge = shared.links[link].bridge;
    for &byte in bytes {
        let Some(cmd) = shared.links[link].acc.push(byte).and_then(|f| decode_frame(f).ok()) else {
            continue;
        };
        if cmd == HostToBridge::Identify
            && let Some(ignore) = shared.bridges[bridge].answers
        {
            shared.identifies[bridge] += 1;
            if shared.identifies[bridge] > ignore {
                let mac = shared.bridges[bridge].ready_mac.unwrap_or(shared.bridges[bridge].mac);
                let answer = ready(mac, shared.booted);
                if let Some(notify) = &shared.links[link].notify {
                    let _ = notify.send(Incoming::Notified(answer));
                }
            }
        }
        shared.record(Did::Frame(cmd));
    }
}

/// `msg`, framed.
pub(super) fn frame(msg: &BridgeToHost) -> Vec<u8> {
    let mut buf = [0; MAX_FRAME];
    let len = encode_frame(msg, &mut buf).expect("the message fits a frame");
    buf[..len].to_vec()
}

/// A `Ready` from a bridge at `mac` that booted at `booted`. Its uptime advances with the clock,
/// so the answers to two `Identify`s read as one life.
fn ready(mac: Mac, booted: Instant) -> Vec<u8> {
    let msg = BridgeToHost::Ready {
        mac,
        fw_version: ShortStr::new(),
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        uptime_ms: u32::try_from(booted.elapsed().as_millis()).unwrap_or(u32::MAX),
        node_board: NodeBoard::NoCarrier,
        radio: None,
        image: None,
    };
    frame(&msg)
}
