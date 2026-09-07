//! A simulated fleet, so the engine and the TUI can be built without hardware.
//!
//! The fake nodes behave the way the real firmware does, including the parts
//! that are inconvenient: they park doing nothing until they are told what to
//! scan, they heartbeat once per completed sweep rather than on a timer, they
//! stagger their transmissions, they adopt an assignment only when its version
//! *differs* from the one they hold, and they suppress a BSSID they have
//! already reported until it falls out of a 200-entry ring. Modelling that last
//! one matters — it is why a real fleet's observation stream goes quiet after
//! the first pass, and a simulator that streamed endlessly would teach the
//! wrong lesson.
//!
//! Two things here are models of a *failure* rather than of correct behaviour,
//! and both are off unless asked for. [`SimConfig::ble_coexistence_failure`]
//! reproduces what a stock node did on the bench — hold the shared antenna
//! through its own admin window and acknowledge nothing — which is the only way
//! to exercise the host's `no admin ack` path without a second radio and a
//! reflash. It is off by default because it is *not* what wartui's own node
//! firmware measured: that one acknowledged every time with BLE on.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wartui_proto::air::{AdminMsg, Frame, MsgType, TextMsg};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, EspNowPayload, HostToBridge, LogLevel, LogStr, Mac, SendStatus,
};
use wartui_proto::plan::{
    ADMIN_WAIT_MS, CHANNEL_DWELL_MS, ChannelSet, DEDUP_RING, IDLE_BEAT_MS, NODE_STAGGER_WINDOW_MS,
    NUM_SCAN_CHANNELS, SCAN_CHANNELS,
};

use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// How the simulated fleet should behave.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// How many nodes to pretend are out there.
    pub node_count: u8,
    /// Time scale. 1.0 is real time; larger runs the fleet faster, which is
    /// what tests want.
    pub speed: f64,
    /// Seeds the fake world, so a run is reproducible.
    pub seed: u64,
    /// Size of the imaginary neighbourhood.
    pub wifi_networks: u16,
    /// Chance per channel dwell that a BLE advertiser shows up, for the one
    /// node holding the Bluetooth assignment. BLE addresses rotate for privacy,
    /// so these never dedup and keep the stream alive.
    ///
    /// A rate rather than a model: the real firmware runs one bounded scan
    /// every few seconds and reports what it heard in a burst. What is faithful
    /// here is *who* emits these — only the node the host gave the flag to, so
    /// a fleet where nobody was asked reports no Bluetooth at all.
    pub ble_chance: f64,
    /// Make the node holding the Bluetooth assignment miss its admin window.
    ///
    /// The Phase 0 failure, reproducible without hardware: a node whose radio
    /// is away on the shared 2.4 GHz antenna acknowledges nothing, so the
    /// bridge reports `AckFail` and the host shows `no admin ack`. Off by
    /// default, because wartui's own node firmware does not do this — it bounds
    /// the scan, switches the controller off, and acknowledged every assignment
    /// on the bench (`docs/phase-1-findings.md`).
    ///
    /// Note that it is self-latching, exactly as it was on the bench: the frame
    /// that turns Bluetooth on is acknowledged, because the node was not yet
    /// holding the antenna, and nothing after it is. The operator cannot take
    /// the assignment back.
    pub ble_coexistence_failure: bool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            node_count: 3,
            speed: 1.0,
            seed: 0x5EED,
            wifi_networks: 120,
            ble_chance: 0.15,
            ble_coexistence_failure: false,
        }
    }
}

/// A simulated bridge with a simulated fleet behind it.
#[derive(Debug, Clone)]
pub struct SimTransport {
    config: SimConfig,
}

impl SimTransport {
    /// Build a simulator.
    #[must_use]
    pub const fn new(config: SimConfig) -> Self {
        Self { config }
    }

    /// The MAC the simulator gives node `index`.
    ///
    /// Locally administered (bit 1 of the first octet) so it can never collide
    /// with a real device if a simulated and a real fleet are ever compared.
    #[must_use]
    pub const fn node_mac(index: u8) -> Mac {
        [0x02, 0x00, 0x00, 0x00, 0x00, index]
    }

    /// Start the fleet.
    ///
    /// # Errors
    /// Never fails today; the signature matches the serial transport so the two
    /// are interchangeable at the call site.
    pub fn start(self) -> Result<LinkHandle, TransportError> {
        let (handle, plumbing) = link_pair();
        let world = Arc::new(World::new(&self.config));
        let started = Instant::now();

        // Nothing a node says is observable until the dongle is attached and
        // has announced itself, so the fake fleet waits for that rather than
        // racing it. Without the gate a parked node — which heartbeats the
        // moment it starts — can put a frame on the link ahead of `Connected`,
        // and a host that has not seen a bridge yet has nowhere to file it.
        let (attached_tx, attached_rx) = tokio::sync::watch::channel(false);

        let mut admin_txs = Vec::new();
        let mut fleet = Vec::new();
        for index in 0..self.config.node_count {
            let (admin_tx, admin_rx) = mpsc::channel(8);
            // Shared with the bridge rather than inferred from the frames it
            // routes, because the bridge has to know the node is deaf *before*
            // it decides what to report — the whole point of the failure is
            // that nothing arrives to be inferred from.
            let holds_ble = Arc::new(AtomicBool::new(false));
            admin_txs.push((Self::node_mac(index), admin_tx, Arc::clone(&holds_ble)));
            fleet.push((SimNode::new(index, holds_ble), admin_rx));
        }

        let events = plumbing.events.clone();
        tokio::spawn(run_bridge(
            plumbing,
            admin_txs,
            started,
            self.config.node_count,
            self.config.ble_coexistence_failure,
            attached_tx,
        ));
        for (node, admin_rx) in fleet {
            tokio::spawn(run_node(
                node,
                Arc::clone(&world),
                events.clone(),
                admin_rx,
                self.config.speed,
                started,
                attached_rx.clone(),
            ));
        }
        Ok(handle)
    }
}

/// The dongle half: announces itself, routes admin frames, answers status.
async fn run_bridge(
    mut plumbing: crate::LinkPlumbing,
    admin_txs: Vec<(Mac, mpsc::Sender<AdminMsg>, Arc<AtomicBool>)>,
    started: Instant,
    node_count: u8,
    ble_coexistence_failure: bool,
    attached: tokio::sync::watch::Sender<bool>,
) {
    let info = BridgeInfo {
        chip: Chip::Esp32C6,
        mac: [0x02, 0x00, 0x00, 0x00, 0xBB, 0x01],
        fw_version: format!("{}-sim", env!("CARGO_PKG_VERSION")),
    };
    if plumbing.events.send(LinkEvent::Connected(info)).await.is_err() {
        return;
    }
    // Only now is there anything for the fleet to be heard by.
    let _ = attached.send(true);

    let mut channel = 6u8;
    let mut peers: Vec<Mac> = Vec::new();

    while let Some(cmd) = plumbing.commands.recv().await {
        let reply = match cmd {
            // The real bridge replies with `Ready`; the simulator has already
            // reported `Connected` above, so re-announcing would only confuse a
            // host that is counting connections.
            HostToBridge::Identify => None,
            HostToBridge::SetChannel { channel: ch } => {
                channel = ch;
                None
            }
            HostToBridge::AddPeer { mac } => {
                if !peers.contains(&mac) {
                    peers.push(mac);
                }
                None
            }
            HostToBridge::RemovePeer { mac } => {
                peers.retain(|p| *p != mac);
                None
            }
            HostToBridge::GetStatus => Some(BridgeToHost::Status {
                channel,
                peer_count: u8::try_from(peers.len()).unwrap_or(u8::MAX),
                rx_count: 0,
                dropped_tx: 0,
                uptime_ms: elapsed_ms(started),
            }),
            HostToBridge::Reset => Some(BridgeToHost::Log {
                level: LogLevel::Info,
                message: LogStr::try_from("simulated reset ignored").unwrap_or_default(),
            }),
            HostToBridge::SendEspNow { id, dst, ensure_peer, payload } => {
                if ensure_peer && !peers.contains(&dst) {
                    peers.push(dst);
                }
                let status =
                    deliver(&admin_txs, dst, &payload, &peers, ble_coexistence_failure).await;
                Some(BridgeToHost::SendResult { id, status, tx_us: elapsed_us(started) })
            }
        };
        if let Some(msg) = reply
            && plumbing.events.send(LinkEvent::Message(msg)).await.is_err()
        {
            return;
        }
    }
    let _ = node_count;
}

/// Hand a transmitted frame to whichever simulated node it is addressed to.
async fn deliver(
    admin_txs: &[(Mac, mpsc::Sender<AdminMsg>, Arc<AtomicBool>)],
    dst: Mac,
    payload: &[u8],
    peers: &[Mac],
    ble_coexistence_failure: bool,
) -> SendStatus {
    if dst == BROADCAST {
        return SendStatus::Broadcast;
    }
    if !peers.contains(&dst) {
        return SendStatus::NoPeer;
    }
    let Ok(Frame::Admin(admin)) = Frame::decode(payload) else {
        // The real radio would happily transmit it; nobody is listening.
        return SendStatus::AckOk;
    };
    match admin_txs.iter().find(|(mac, _, _)| *mac == dst) {
        // The radio is away on the Bluetooth antenna. An 802.11 acknowledgement
        // comes from the receiver's MAC hardware, so this is indistinguishable
        // from a node that is not there — and the frame is genuinely dropped,
        // not merely unreported, which is why the node can never be told to
        // stop scanning.
        Some((_, _, holds_ble)) if ble_coexistence_failure && holds_ble.load(Ordering::Relaxed) => {
            SendStatus::AckFail
        }
        Some((_, tx, _)) if tx.send(admin).await.is_ok() => SendStatus::AckOk,
        // Addressed to a node that is not out there, so nothing acknowledges.
        _ => SendStatus::AckFail,
    }
}

/// One simulated node's own state, mirroring the firmware's node-side globals.
#[derive(Debug)]
struct SimNode {
    mac: Mac,
    /// `assignment_version` starts at 0 on a fresh node (`src/WiFiOps.cpp:81`),
    /// so the first ADMIN of any version always takes.
    assignment_version: u8,
    node_index: u8,
    node_count: u8,
    channels: ChannelSet,
    /// Whether this node was asked to scan Bluetooth, mirrored where the
    /// bridge can read it.
    holds_ble: Arc<AtomicBool>,
    hb_counter: u32,
    seen: VecDeque<Mac>,
    rng: Xorshift,
}

impl SimNode {
    fn new(index: u8, holds_ble: Arc<AtomicBool>) -> Self {
        Self {
            mac: SimTransport::node_mac(index),
            assignment_version: 0,
            node_index: 0,
            node_count: 1,
            // Nothing until it is told. The vendor default is all forty
            // channels (`src/WiFiOps.cpp:77-80`); wartui's node parks on the
            // control channel and collects nothing, so that a node which has
            // never heard a core is not quietly duplicating the fleet's work
            // and transmitting where the pool exists to keep it off.
            channels: ChannelSet::empty(),
            holds_ble,
            hb_counter: 0,
            seen: VecDeque::with_capacity(DEDUP_RING),
            rng: Xorshift::new(0xA5A5_0000 ^ u64::from(index).wrapping_mul(0x9E37_79B9)),
        }
    }

    /// Whether the node has been told what to scan.
    const fn assigned(&self) -> bool {
        self.assignment_version != 0
    }

    /// Apply an assignment, but only when its version differs — the same `!=`
    /// comparison the firmware makes at `src/WiFiOps.cpp:1198`.
    fn apply(&mut self, admin: AdminMsg) {
        if admin.assignment_version == self.assignment_version {
            return;
        }
        self.assignment_version = admin.assignment_version;
        self.node_index = admin.node_index;
        self.node_count = admin.node_count;
        self.channels = admin.channels;
        self.holds_ble.store(admin.scan_ble(), Ordering::Relaxed);
    }

    /// Whether this node is the one scanning Bluetooth.
    fn scanning_ble(&self) -> bool {
        self.holds_ble.load(Ordering::Relaxed)
    }

    /// The firmware's 200-entry insertion-order ring (`src/WiFiOps.cpp:1803`).
    /// Returns true the first time a MAC is offered.
    fn first_sighting(&mut self, mac: Mac) -> bool {
        if self.seen.contains(&mac) {
            return false;
        }
        if self.seen.len() == DEDUP_RING {
            self.seen.pop_front();
        }
        self.seen.push_back(mac);
        true
    }
}

async fn run_node(
    mut node: SimNode,
    world: Arc<World>,
    events: mpsc::Sender<LinkEvent>,
    mut admin_rx: mpsc::Receiver<AdminMsg>,
    speed: f64,
    started: Instant,
    mut attached: tokio::sync::watch::Receiver<bool>,
) {
    if attached.wait_for(|up| *up).await.is_err() {
        return;
    }
    let dwell = scaled(u64::from(CHANNEL_DWELL_MS), speed);
    loop {
        if !node.assigned() {
            // Parked on the control channel, reachable the whole time and
            // collecting nothing. Heartbeat first and listen afterwards,
            // because the host only ever transmits an assignment in answer to
            // one — so this is how long joining a fleet takes.
            if beat(&events, &mut node, started).await.is_err() {
                return;
            }
            if nap(scaled(u64::from(IDLE_BEAT_MS), speed), &mut admin_rx, &mut node)
                .await
                .is_break()
            {
                return;
            }
            continue;
        }

        // A node walks its assigned channels one per step
        // (`startNextNodeAssignedScan`, `src/WiFiOps.cpp:741-760`), so the
        // sweep — and therefore the heartbeat period — is proportional to how
        // many channels it was given.
        for idx in node.channels.indices() {
            if nap(dwell, &mut admin_rx, &mut node).await.is_break() {
                return;
            }
            let channel = SCAN_CHANNELS[usize::from(idx)];
            for net in world.on_channel(channel) {
                if node.first_sighting(net.bssid)
                    && emit(&events, &node, &net.line(), started).await.is_err()
                {
                    return;
                }
            }
            // Only the node that was given the Bluetooth assignment, which is
            // the fleet-wide property worth being able to see in the simulator:
            // revoking it should stop these arriving.
            if node.scanning_ble() && node.rng.next_f64() < world.ble_chance {
                let ble = world.ble_sighting(&mut node.rng);
                if node.first_sighting(ble.bssid)
                    && emit(&events, &node, &ble.line(), started).await.is_err()
                {
                    return;
                }
            }
        }

        // Stagger, then heartbeat, then hold the admin window open.
        let stagger = u64::from(wartui_proto::plan::stagger_offset_ms(
            node.node_index,
            node.node_count,
            NODE_STAGGER_WINDOW_MS,
        ));
        if nap(scaled(stagger, speed), &mut admin_rx, &mut node).await.is_break() {
            return;
        }

        if beat(&events, &mut node, started).await.is_err() {
            return;
        }

        if nap(scaled(u64::from(ADMIN_WAIT_MS), speed), &mut admin_rx, &mut node).await.is_break() {
            return;
        }
    }
}

/// Broadcast one heartbeat and advance the counter.
async fn beat(
    events: &mpsc::Sender<LinkEvent>,
    node: &mut SimNode,
    started: Instant,
) -> Result<(), ()> {
    node.hb_counter = node.hb_counter.wrapping_add(1);
    let msg = TextMsg::new(MsgType::Heartbeat, node.hb_counter, b"")
        .expect("an empty heartbeat payload always fits");
    send_frame(events, node.mac, &msg.encode(), started).await
}

/// Sleep, adopting any assignment that arrives meanwhile.
///
/// The real node is blocked in `delay()` during its admin window but the radio
/// callback still fires, so an assignment lands mid-sleep. This reproduces that.
async fn nap(
    duration: Duration,
    admin_rx: &mut mpsc::Receiver<AdminMsg>,
    node: &mut SimNode,
) -> std::ops::ControlFlow<()> {
    let deadline = tokio::time::Instant::now() + duration;
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return std::ops::ControlFlow::Continue(()),
            received = admin_rx.recv() => match received {
                Some(admin) => node.apply(admin),
                None => return std::ops::ControlFlow::Break(()),
            },
        }
    }
}

async fn emit(
    events: &mpsc::Sender<LinkEvent>,
    node: &SimNode,
    line: &str,
    started: Instant,
) -> Result<(), ()> {
    let msg = TextMsg::new(MsgType::Text, 0, line.as_bytes())
        .expect("generated lines are well under the payload limit");
    send_frame(events, node.mac, &msg.encode(), started).await
}

async fn send_frame(
    events: &mpsc::Sender<LinkEvent>,
    src: Mac,
    frame: &[u8],
    started: Instant,
) -> Result<(), ()> {
    let mut payload = EspNowPayload::new();
    payload.extend_from_slice(frame).expect("212 bytes fits the 250-byte payload");
    let rx = BridgeToHost::Rx {
        src,
        dst: BROADCAST,
        rssi: -55,
        channel: 6,
        rx_us: elapsed_us(started),
        payload,
    };
    events.send(LinkEvent::Message(rx)).await.map_err(|_| ())
}

fn scaled(millis: u64, speed: f64) -> Duration {
    #[allow(clippy::cast_precision_loss)]
    let scaled = (millis as f64 / speed.max(f64::MIN_POSITIVE)).max(0.0);
    Duration::from_secs_f64(scaled / 1000.0)
}

/// The firmware's counters are `u32` and wrap at 2^32, not at `u32::MAX`.
/// Simulating the wrong modulus would drift the simulated clock a microsecond
/// per wrap away from the one host code actually has to cope with.
const WRAP: u128 = 1 << 32;

fn elapsed_us(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_micros() % WRAP).unwrap_or(0)
}

fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis() % WRAP).unwrap_or(0)
}

/// A fake neighbourhood, generated once and shared by every node — so two nodes
/// scanning the same channel really do see the same access point, which is what
/// makes cross-node deduplication testable.
#[derive(Debug)]
struct World {
    networks: Vec<Network>,
    ble_chance: f64,
}

impl World {
    fn new(config: &SimConfig) -> Self {
        let mut rng = Xorshift::new(config.seed);
        let securities =
            ["[OPEN]", "[WPA2_PSK]", "[WPA_WPA2_PSK]", "[WPA3_PSK]", "[WPA2_WPA3_PSK]"];
        let networks = (0..config.wifi_networks)
            .map(|i| {
                let channel = SCAN_CHANNELS[rng.below(NUM_SCAN_CHANNELS.into())];
                Network {
                    bssid: rng.mac(),
                    ssid: format!("net-{i:03}"),
                    security: securities[rng.below(securities.len())],
                    channel,
                    rssi: -30 - i16::try_from(rng.below(60)).unwrap_or(0),
                }
            })
            .collect();
        Self { networks, ble_chance: config.ble_chance }
    }

    fn on_channel(&self, channel: u8) -> impl Iterator<Item = &Network> {
        self.networks.iter().filter(move |n| n.channel == channel)
    }

    /// BLE advertisers use rotating private addresses, so every sighting is a
    /// new MAC and none of them ever dedup.
    fn ble_sighting(&self, rng: &mut Xorshift) -> Network {
        Network {
            bssid: rng.mac(),
            ssid: String::new(),
            security: "[BLE]",
            channel: 0,
            rssi: -40 - i16::try_from(rng.below(50)).unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone)]
struct Network {
    bssid: Mac,
    ssid: String,
    security: &'static str,
    channel: u8,
    rssi: i16,
}

impl Network {
    /// The six-field payload a node transmits (`src/WiFiOps.cpp:1777`, `:144`).
    /// Wi-Fi MACs are uppercase and BLE lowercase, matching the two code paths.
    fn line(&self) -> String {
        let b = self.bssid;
        let kind = if self.channel == 0 { 'B' } else { 'W' };
        let mac = if kind == 'B' {
            format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", b[0], b[1], b[2], b[3], b[4], b[5])
        } else {
            format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", b[0], b[1], b[2], b[3], b[4], b[5])
        };
        format!("{mac},{},{},{},{},{kind}", self.ssid, self.security, self.channel, self.rssi)
    }
}

/// Xorshift64*, so a seeded run is byte-for-byte reproducible.
#[derive(Debug)]
struct Xorshift(u64);

impl Xorshift {
    const fn new(seed: u64) -> Self {
        Self(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    const fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 { 0 } else { usize::try_from(self.next_u64() % n as u64).unwrap_or(0) }
    }

    #[allow(clippy::cast_precision_loss)]
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn mac(&mut self) -> Mac {
        let v = self.next_u64().to_le_bytes();
        // Locally administered and unicast, so simulated devices are
        // distinguishable from anything real.
        [0x02, v[1], v[2], v[3], v[4], v[5]]
    }
}
