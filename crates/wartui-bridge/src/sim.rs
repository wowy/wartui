//! A simulated fleet, so the engine and the TUI can be built without hardware.
//!
//! The fake nodes behave the way the real firmware does, including the parts
//! that are inconvenient: they heartbeat once per completed sweep rather than
//! on a timer, they stagger their transmissions, they adopt an assignment only
//! when its version *differs* from the one they hold, and they suppress a BSSID
//! they have already reported until it falls out of a 200-entry ring. Modelling
//! that last one matters — it is why a real fleet's observation stream goes
//! quiet after the first pass, and a simulator that streamed endlessly would
//! teach the wrong lesson.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wartui_proto::air::{AdminMsg, Frame, MsgType, TextMsg};
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, EspNowPayload, HostToBridge, LogLevel, LogStr, Mac, SendStatus,
};
use wartui_proto::plan::{NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS, SCAN_CHANNELS};

use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// `CHANNEL_TIMER`, `src/configs.h:159` — how long a node dwells per channel.
const CHANNEL_DWELL_MS: u64 = 80;

/// `ADMIN_WAIT_MS`, `src/WiFiOps.h:58`.
const ADMIN_WAIT_MS: u64 = 300;

/// `mac_history_len`, `src/configs.h:158`.
const DEDUP_RING: usize = 200;

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
    /// Chance per channel dwell that a BLE advertiser shows up. BLE addresses
    /// rotate for privacy, so these never dedup and keep the stream alive.
    pub ble_chance: f64,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self { node_count: 3, speed: 1.0, seed: 0x5EED, wifi_networks: 120, ble_chance: 0.15 }
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

        let mut admin_txs = Vec::new();
        for index in 0..self.config.node_count {
            let (admin_tx, admin_rx) = mpsc::channel(8);
            admin_txs.push((Self::node_mac(index), admin_tx));
            tokio::spawn(run_node(
                SimNode::new(index),
                Arc::clone(&world),
                plumbing.events.clone(),
                admin_rx,
                self.config.speed,
                started,
            ));
        }

        tokio::spawn(run_bridge(plumbing, admin_txs, started, self.config.node_count));
        Ok(handle)
    }
}

/// The dongle half: announces itself, routes admin frames, answers status.
async fn run_bridge(
    mut plumbing: crate::LinkPlumbing,
    admin_txs: Vec<(Mac, mpsc::Sender<AdminMsg>)>,
    started: Instant,
    node_count: u8,
) {
    let info = BridgeInfo {
        chip: Chip::Esp32C6,
        mac: [0x02, 0x00, 0x00, 0x00, 0xBB, 0x01],
        fw_version: format!("{}-sim", env!("CARGO_PKG_VERSION")),
    };
    if plumbing.events.send(LinkEvent::Connected(info)).await.is_err() {
        return;
    }

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
                let status = deliver(&admin_txs, dst, &payload, &peers).await;
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
    admin_txs: &[(Mac, mpsc::Sender<AdminMsg>)],
    dst: Mac,
    payload: &[u8],
    peers: &[Mac],
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
    match admin_txs.iter().find(|(mac, _)| *mac == dst) {
        Some((_, tx)) if tx.send(admin).await.is_ok() => SendStatus::AckOk,
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
    start_idx: u8,
    end_idx: u8,
    hb_counter: u32,
    seen: VecDeque<Mac>,
    rng: Xorshift,
}

impl SimNode {
    fn new(index: u8) -> Self {
        Self {
            mac: SimTransport::node_mac(index),
            assignment_version: 0,
            node_index: 0,
            node_count: 1,
            // A node that has never heard a core scans everything
            // (`src/WiFiOps.cpp:77-80`).
            start_idx: 0,
            end_idx: NUM_SCAN_CHANNELS - 1,
            hb_counter: 0,
            seen: VecDeque::with_capacity(DEDUP_RING),
            rng: Xorshift::new(0xA5A5_0000 ^ u64::from(index).wrapping_mul(0x9E37_79B9)),
        }
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
        self.start_idx = admin.start_channel_idx;
        self.end_idx = admin.end_channel_idx;
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
) {
    let dwell = scaled(CHANNEL_DWELL_MS, speed);
    loop {
        // A node walks its assigned range one channel per step
        // (`startNextNodeAssignedScan`, `src/WiFiOps.cpp:741-760`), so the
        // sweep — and therefore the heartbeat period — is proportional to how
        // many channels it was given.
        let (start, end) = (node.start_idx, node.end_idx.max(node.start_idx));
        for idx in start..=end.min(NUM_SCAN_CHANNELS - 1) {
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
            if node.rng.next_f64() < world.ble_chance {
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

        node.hb_counter = node.hb_counter.wrapping_add(1);
        let beat = TextMsg::new(MsgType::Heartbeat, node.hb_counter, b"")
            .expect("an empty heartbeat payload always fits");
        if send_frame(&events, node.mac, &beat.encode(), started).await.is_err() {
            return;
        }

        if nap(scaled(ADMIN_WAIT_MS, speed), &mut admin_rx, &mut node).await.is_break() {
            return;
        }
    }
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

fn elapsed_us(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_micros() % u128::from(u32::MAX)).unwrap_or(0)
}

fn elapsed_ms(started: Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis() % u128::from(u32::MAX)).unwrap_or(0)
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
