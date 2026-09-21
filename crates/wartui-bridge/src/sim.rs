//! A simulated fleet, so the engine and the TUI can be built without hardware.
//!
//! The fake nodes behave the way the real firmware does, including the inconvenient
//! parts — parking until told what to scan, heartbeating once per completed sweep,
//! scanning Bluetooth and nothing else when that is the node's job, staggering,
//! adopting only on a differing epoch, and suppressing a BSSID through the same
//! [`MacRing`] the firmware links, on node time scaled by [`SimConfig::speed`].
//! That last one matters most: it is why a real fleet's observation stream thins to a
//! trickle after the first pass, and a simulator that streamed endlessly would teach
//! the wrong lesson. Simulated signal is fixed per network, so only the refresh ever
//! re-reports one here.
//!
//! [`SimConfig::ble_coexistence_failure`] models a *failure* — what a stock node did
//! on the bench, and the only way to exercise the host's `no admin ack` path without
//! a second radio — and [`SimConfig::c6_nodes`] a mixed fleet. Both are off unless
//! asked for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use wartui_proto::air::{
    AdminMsg, Capabilities, Frame, HeartbeatMsg, RecordKind, SIGHTING_MSG_MAX, Security,
    SightingMsg,
};
use wartui_proto::dedup::MacRing;
use wartui_proto::link::{
    BROADCAST, BridgeToHost, Chip, EspNowPayload, HostToBridge, LogLevel, LogStr, LoopPhase, Mac,
    Panel, ResetCause, SendStatus,
};
use wartui_proto::plan::{
    ADMIN_WAIT_MS, BLE_BEAT_MS, CHANNEL_DWELL_MS, ChannelSet, DEDUP_RING, IDLE_BEAT_MS,
    NODE_STAGGER_WINDOW_MS, NUM_SCAN_CHANNELS, SCAN_CHANNELS,
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
    /// Chance per advertiser slot in a scan that a BLE advertiser shows up, for the
    /// one node holding the Bluetooth assignment. BLE addresses rotate for privacy,
    /// so these never dedup and keep the stream alive.
    ///
    /// Per scan rather than per dwell, because such a node dwells on nothing: its
    /// whole second is one scan, and it has [`ADVERTISERS_PER_SCAN`] slots in it.
    ///
    /// A rate rather than a model; what is faithful is *who* emits these, so a
    /// fleet where nobody was asked reports no Bluetooth at all.
    pub ble_chance: f64,
    /// Make the node holding the Bluetooth assignment miss its admin window.
    ///
    /// A node whose radio is away on the shared 2.4 GHz antenna acknowledges
    /// nothing, so the bridge reports `AckFail` and the host shows `no admin ack` —
    /// the only way to exercise that path without a second radio and a reflash. Off
    /// by default, because wartui's own firmware bounds the scan and acknowledged
    /// every assignment (`docs/phase-0-findings.md`, `docs/phase-1-findings.md`).
    ///
    /// Self-latching, exactly as it was on the bench: the frame that turns Bluetooth
    /// on is acknowledged, because the node was not yet holding the antenna, and
    /// nothing after it is. The operator cannot take the assignment back.
    pub ble_coexistence_failure: bool,
    /// How many of the fleet are ESP32-C6s, counting from the last index.
    ///
    /// Zero by default, so the simulator models a fleet of C5s. This is the only way
    /// to see a mixed fleet without two kinds of board on the desk, and the failure
    /// it guards against is invisible without one: dealt 5 GHz, a C6 acknowledges
    /// and scans the part it can reach, so the plan looks healthy while a third of
    /// the pool is uncovered.
    pub c6_nodes: u8,
    /// Give a network a new address once it has been heard this many times, as a moving
    /// fleet leaves networks behind and meets new ones.
    ///
    /// `None`, the default, keeps the neighbourhood the same for ever, which is a parked
    /// fleet. Counted per network across the whole fleet, and on every hearing rather than
    /// every report, so a node whose ring suppresses a network still moves it on.
    pub sightings_per_address: Option<u32>,
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
            c6_nodes: 0,
            sightings_per_address: None,
        }
    }
}

/// Advertisers one simulated scan can turn up, each subject to
/// [`SimConfig::ble_chance`].
///
/// A scan and not a dwell: the node holding the Bluetooth assignment sniffs no Wi-Fi,
/// so its whole second is one scan, and one advertiser a second would be a trickle
/// where an ordinary room is a stream — a real scan hears about fifty
/// (`docs/phase-1-findings.md`), most of them addresses it has never seen.
const ADVERTISERS_PER_SCAN: usize = 12;

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

        // Nothing a node says is observable until the dongle has announced itself,
        // so the fake fleet waits rather than racing it: a parked node heartbeats
        // the moment it starts, and a host that has not seen a bridge yet has
        // nowhere to file that.
        let (attached_tx, attached_rx) = tokio::sync::watch::channel(false);

        let mut admin_txs = Vec::new();
        let mut fleet = Vec::new();
        for index in 0..self.config.node_count {
            let (admin_tx, admin_rx) = mpsc::channel(8);
            // Shared with the bridge rather than inferred from the frames it
            // routes: the bridge has to know the node is deaf *before* it decides
            // what to report, and nothing arrives to infer it from.
            let holds_ble = Arc::new(AtomicBool::new(false));
            admin_txs.push((Self::node_mac(index), admin_tx, Arc::clone(&holds_ble)));
            // Counted from the end, so the nodes below the count keep their
            // radios as the fleet grows.
            let five_ghz = self.config.node_count - index > self.config.c6_nodes;
            fleet.push((SimNode::new(index, holds_ble, five_ghz), admin_rx));
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
        // A simulated dongle has only ever just been switched on, so anything else
        // here is a fault on screen that no amount of looking could explain.
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        // Not modelled: nothing in the simulator allocates on a device heap,
        // and a made-up figure would be read as a measurement.
        heap_free: 0,
        // Announced the instant it came up, and it never reboots.
        uptime_ms: 0,
        // A simulated C6 with a T-Dongle-C5's screen, which no board is. The point is
        // that `--sim` exercises the whole push path — render, rate limit, encode, send
        // — with nothing attached, and a simulator that reported no panel would leave
        // that path untested until a board was on the desk.
        //
        // The geometry is the shipping board's rather than a roomy invention, so what
        // `--sim` puts on the wire is what a dongle would be sent, truncation and all.
        // It tracked the font once and did not follow it to `FONT_9X15`, which left
        // the simulator rehearsing a width of 26 that nothing has.
        panel: Some(Panel { cols: 17, rows: 5 }),
    };
    if plumbing.events.send(LinkEvent::Connected(info)).await.is_err() {
        return;
    }
    let _ = attached.send(true);

    let mut channel = 6u8;
    let mut peers: Vec<Mac> = Vec::new();

    while let Some(cmd) = plumbing.commands.recv().await {
        let reply = match cmd {
            // The real bridge replies with `Ready`, but `Connected` has already
            // gone out above and a host counting connections would be confused.
            HostToBridge::Identify => None,
            // Nothing to draw on, and nothing to say about it: a real bridge without a
            // screen would do the same. The host only sends these because this simulator
            // claims a panel above.
            HostToBridge::ShowPanel { .. } => None,
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
        // The radio is away on the Bluetooth antenna, which is indistinguishable
        // from a node that is not there. The frame is genuinely dropped rather than
        // merely unreported, which is why the node can never be told to stop.
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
    /// The epoch starts at 0 on a fresh node, so the first assignment of any
    /// epoch always takes.
    epoch: u8,
    node_index: u8,
    node_count: u8,
    channels: ChannelSet,
    /// Whether this node was asked to scan Bluetooth, mirrored where the
    /// bridge can read it.
    holds_ble: Arc<AtomicBool>,
    /// Whether its radio reaches 5 GHz. Announced in every heartbeat, and the
    /// planner's only reason to treat one node differently from another.
    five_ghz: bool,
    hb_counter: u32,
    seen: Box<MacRing<DEDUP_RING>>,
    /// When this node booted, on tokio's clock so a paused test controls it.
    boot: tokio::time::Instant,
    speed: f64,
    rng: Xorshift,
}

impl SimNode {
    fn new(index: u8, holds_ble: Arc<AtomicBool>, five_ghz: bool) -> Self {
        Self {
            mac: SimTransport::node_mac(index),
            five_ghz,
            epoch: 0,
            node_index: 0,
            node_count: 1,
            // Nothing until it is told. A wartui node parks on the control channel,
            // so one that has never heard a core is not quietly duplicating the
            // fleet's work.
            channels: ChannelSet::empty(),
            holds_ble,
            hb_counter: 0,
            seen: Box::default(),
            boot: tokio::time::Instant::now(),
            speed: 1.0,
            rng: Xorshift::new(0xA5A5_0000 ^ u64::from(index).wrapping_mul(0x9E37_79B9)),
        }
    }

    /// Whether the node has been told what to scan.
    ///
    /// The channels, or the Bluetooth scan. An empty mask with the flag clear counts
    /// as nothing, the way the firmware counts it: no frame means "scan nothing", so
    /// reaching this needs a confused host, and parking is the same answer as never
    /// having been told anything at all.
    fn assigned(&self) -> bool {
        self.epoch != 0 && (!self.channels.is_empty() || self.bluetooth_only())
    }

    /// Whether this node's whole job is Bluetooth.
    fn bluetooth_only(&self) -> bool {
        self.channels.is_empty() && self.scanning_ble()
    }

    /// Apply an assignment, but only when its epoch differs — the same `!=`
    /// comparison the node firmware makes.
    fn apply(&mut self, admin: AdminMsg) {
        if admin.epoch == self.epoch {
            return;
        }
        self.epoch = admin.epoch;
        self.node_index = admin.node_index;
        self.node_count = admin.node_count;
        self.channels = admin.channels;
        self.holds_ble.store(admin.scan_ble(), Ordering::Relaxed);
    }

    /// Whether this node is the one scanning Bluetooth.
    fn scanning_ble(&self) -> bool {
        self.holds_ble.load(Ordering::Relaxed)
    }

    /// Whether the firmware would transmit this sighting now, recording it if so.
    /// A simulated broadcast cannot fail, so asking and recording are one step.
    fn worth_reporting(&mut self, network: &Network) -> bool {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let now_ms = (self.boot.elapsed().as_secs_f64() * 1000.0 * self.speed) as u64 as u32;
        self.seen.offer(network.bssid, Some(network.rssi), now_ms)
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
    // Here rather than in `SimNode::new`, which may run outside the runtime and so off
    // the clock a paused test is moving.
    node.boot = tokio::time::Instant::now();
    node.speed = speed;
    let dwell = scaled(u64::from(CHANNEL_DWELL_MS), speed);
    loop {
        if !node.assigned() {
            // Parked on the control channel, reachable throughout and collecting
            // nothing. Heartbeat first and listen after, because the host only
            // transmits in answer to one — so this is how long joining takes.
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

        // Bluetooth is a whole node's job, so this one dwells on nothing: one scan
        // per BLE_BEAT_MS, scan start to scan start, and what is left of the second
        // after the reports and the heartbeat is the window it answers in.
        if node.bluetooth_only() {
            let cycle = tokio::time::Instant::now() + scaled(u64::from(BLE_BEAT_MS), speed);
            for _ in 0..ADVERTISERS_PER_SCAN {
                if node.rng.next_f64() >= world.ble_chance {
                    continue;
                }
                let ble = world.ble_sighting(&mut node.rng);
                if node.worth_reporting(&ble) && emit(&events, &node, &ble, started).await.is_err()
                {
                    return;
                }
            }
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
            // To the deadline rather than for a duration, so a busy second leaves
            // little here and the next scan starts at once — but never less than a
            // whole admin window, which is the floor the firmware holds too. Without
            // it a node whose reports outran the deadline would loop with no window
            // and, on a lone node, no sleep at all.
            let window = tokio::time::Instant::now() + scaled(u64::from(ADMIN_WAIT_MS), speed);
            if nap_until(cycle.max(window), &mut admin_rx, &mut node).await.is_break() {
                return;
            }
            continue;
        }

        // A node walks its assigned channels one per step, so the
        // sweep — and therefore the heartbeat period — is proportional to how
        // many channels it was given.
        for idx in node.channels.indices() {
            if nap(dwell, &mut admin_rx, &mut node).await.is_break() {
                return;
            }
            let channel = SCAN_CHANNELS[usize::from(idx)];
            for slot in world.on_channel(channel) {
                let net = world.hear(slot);
                if node.worth_reporting(&net) && emit(&events, &node, &net, started).await.is_err()
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
    // Capabilities are what tell the host this is a node it can drive, so a fleet
    // without them is a fleet the planner ignores. Bluetooth is always claimed; the
    // band is not, because a fleet where everything reaches 5 GHz never makes the
    // planner leave a node out of a channel.
    let msg = HeartbeatMsg {
        counter: node.hb_counter,
        capabilities: Capabilities::here(true, node.five_ghz),
    };
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
    nap_until(tokio::time::Instant::now() + duration, admin_rx, node).await
}

/// Sleep until a deadline, adopting any assignment that arrives meanwhile.
///
/// The primitive [`nap`] is a wrapper over, because a Bluetooth node's cycle is a
/// deadline rather than a duration: what is left of its second after the scan and the
/// heartbeat is whatever is left, and a scan that overran leaves nothing.
async fn nap_until(
    deadline: tokio::time::Instant,
    admin_rx: &mut mpsc::Receiver<AdminMsg>,
    node: &mut SimNode,
) -> std::ops::ControlFlow<()> {
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
    network: &Network,
    started: Instant,
) -> Result<(), ()> {
    let mut frame = [0u8; SIGHTING_MSG_MAX];
    let len = network
        .as_msg()
        .encode_into(&mut frame)
        .expect("SIGHTING_MSG_MAX is sized for the longest SSID there is");
    send_frame(events, node.mac, &frame[..len], started).await
}

async fn send_frame(
    events: &mpsc::Sender<LinkEvent>,
    src: Mac,
    frame: &[u8],
    started: Instant,
) -> Result<(), ()> {
    let mut payload = EspNowPayload::new();
    payload.extend_from_slice(frame).expect("the longest sighting fits the 250-byte payload");
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
    /// How many times each network's slot has been heard, for turnover.
    heard: Vec<AtomicU32>,
    ble_chance: f64,
    sightings_per_address: Option<u32>,
}

impl World {
    fn new(config: &SimConfig) -> Self {
        let mut rng = Xorshift::new(config.seed);
        let securities = [
            Security::Open,
            Security::Wpa2Psk,
            Security::WpaWpa2Psk,
            Security::Wpa3Psk,
            Security::Wpa2Wpa3Psk,
        ];
        let networks: Vec<Network> = (0..config.wifi_networks)
            .map(|i| {
                let channel = SCAN_CHANNELS[rng.below(NUM_SCAN_CHANNELS.into())];
                Network {
                    bssid: rng.mac(),
                    ssid: format!("net-{i:03}"),
                    security: securities[rng.below(securities.len())],
                    channel,
                    rssi: -30 - i8::try_from(rng.below(60)).unwrap_or(0),
                    // Every fourth network is a Passpoint one, so both the
                    // with-trailer and without paths run on every capture.
                    ext: if i % 4 == 0 { OPEN_ROAMING.to_vec() } else { Vec::new() },
                }
            })
            .collect();
        let heard = networks.iter().map(|_| AtomicU32::new(0)).collect();
        Self {
            networks,
            heard,
            ble_chance: config.ble_chance,
            // Zero would turn every network over before it was heard at all; read it as
            // no turnover rather than divide by it.
            sightings_per_address: config.sightings_per_address.filter(|&n| n > 0),
        }
    }

    /// The slots holding the networks on a channel.
    fn on_channel(&self, channel: u8) -> impl Iterator<Item = usize> + '_ {
        (0..self.networks.len()).filter(move |&slot| self.networks[slot].channel == channel)
    }

    /// The network in `slot` as it is heard now, counting the hearing.
    ///
    /// With [`SimConfig::sightings_per_address`], every that-many hearings the slot holds a
    /// different device: the same channel, signal and name under a new address. The count
    /// is the world's, shared by the fleet, so it is the neighbourhood that moves on rather
    /// than one node's view of it.
    fn hear(&self, slot: usize) -> Network {
        let network = &self.networks[slot];
        let Some(per_address) = self.sightings_per_address else { return network.clone() };
        let generation = self.heard[slot].fetch_add(1, Ordering::Relaxed) / per_address;
        if generation == 0 {
            return network.clone();
        }
        Network { bssid: moved(network.bssid, generation), ..network.clone() }
    }

    /// BLE advertisers use rotating private addresses, so every sighting is a
    /// new MAC and none of them ever dedup.
    fn ble_sighting(&self, rng: &mut Xorshift) -> Network {
        Network {
            bssid: rng.mac(),
            ssid: String::new(),
            security: Security::Ble,
            channel: 0,
            rssi: -40 - i8::try_from(rng.below(50)).unwrap_or(0),
            // Half of them carry a manufacturer identifier — 76, which a
            // person can check against the SIG company list — so both BLE
            // trailer paths run too.
            ext: if rng.below(2) == 0 { 76u16.to_le_bytes().to_vec() } else { Vec::new() },
        }
    }
}

/// The address a network's slot holds after its `generation`th turnover.
///
/// Mixed rather than counted, so consecutive devices in a slot look no more related than
/// any two real ones, and derived from the slot's first address rather than drawn from the
/// world's generator, so turning over cannot change the neighbourhood a seed produces.
/// Locally administered, like every simulated address.
fn moved(bssid: Mac, generation: u32) -> Mac {
    let mut first = [0u8; 8];
    first[..6].copy_from_slice(&bssid);
    let v = mix(mix(u64::from_le_bytes(first)) ^ u64::from(generation)).to_le_bytes();
    [0x02, v[1], v[2], v[3], v[4], v[5]]
}

/// SplitMix64's finaliser: every input bit reaches every output bit.
const fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

#[derive(Debug, Clone)]
struct Network {
    bssid: Mac,
    ssid: String,
    security: Security,
    channel: u8,
    rssi: i8,
    /// The sighting's trailer, as bytes: the roaming consortium body for
    /// Wi-Fi, the manufacturer identifier for BLE. Stored rendered so `as_msg`
    /// needs no kind-dependent logic of its own.
    ext: Vec<u8>,
}

/// The OpenRoaming roaming consortium triple, so a simulated Passpoint
/// neighbourhood exercises the trailer the way a real one would.
const OPEN_ROAMING: [u8; 17] = [
    0x02, 0x55, //
    0x5A, 0x03, 0xBA, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x20, 0x00,
];

impl Network {
    /// The observation a node would broadcast about it.
    fn as_msg(&self) -> SightingMsg<'_> {
        SightingMsg {
            kind: if self.channel == 0 { RecordKind::Ble } else { RecordKind::Wifi },
            bssid: self.bssid,
            channel: self.channel,
            rssi: self.rssi,
            security: self.security,
            ssid: self.ssid.as_bytes(),
            ext: &self.ext,
        }
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
