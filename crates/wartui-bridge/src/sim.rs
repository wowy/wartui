//! A simulated fleet, so the engine and the TUI can be built without hardware.
//!
//! The fake nodes behave the way the real firmware does, including the inconvenient
//! parts — idling until told what to scan, heartbeating on a timer independent of
//! how often they sweep, scanning Bluetooth and nothing else when that is the
//! node's job, adopting only on a differing epoch, and suppressing an address through
//! the same dedup ring the firmware links for its chip, on node time scaled by
//! [`SimConfig::speed`].
//! That last one matters most: it is why a real fleet's observation stream thins to a
//! trickle after the first pass, and a simulator that streamed endlessly would teach
//! the wrong lesson. Simulated signal is fixed per network, so only the refresh ever
//! re-reports one here.
//!
//! The bridge's microsecond clock is tokio's, so a paused test holds still the clock
//! latency and uptime are measured on, as it does node time. [`SimConfig::speed`]
//! does not scale it: it is the bridge's clock, not a node's.
//!
//! Node `i` sits in node-bus slot `i` (`A1` up, then `B1`), on a fourteen-node carrier, so the
//! view names it by the silk, `N1` up. The bridge answers each `ToNode` with `Delivered` once
//! the node has it, or `Undelivered` for a slot with no node. A `PowerCycle` restarts the node in
//! that slot, which heartbeats again from a fresh boot, still on trial if it was, and is answered
//! `Done`.
//!
//! A node takes updates with the receiver the firmware links, into a slot held in memory,
//! and follows the firmware's rules around it: it listens continuously in update mode,
//! reboots into a verified image on trial, and confirms it on the first record from the
//! bridge it reads. [`SimConfig::lossy_node`] and
//! [`SimConfig::rejecting_node`] are the failures an update has to survive or report.
//!
//! The bridge keeps the P4's node image cache, the same [`NodeImageCache`] over memory: a
//! `CacheImage` upload fills it, and a `ToNodeChunk` is built from the verified image with the
//! same [`cache_chunk_body`], then routed as a `ToNode` is, loss model included. Until an upload
//! verifies, and from the next `Begin` on, a chunk command is answered `NoImage`.
//!
//! Its board is a carrier, so it has a lid fan: a `SetFan` sets the duty `Status` reports, with a
//! speed made up from it. A `SetPool` sets the pool `Status` reports, which starts at
//! [`SimConfig::pool`]. It has no flash, and connects once and never reboots, so both last the
//! simulator's life, and the duty starts at full.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use wartui_proto::bus::{
    Assign, Board, Capabilities, DieTemp, DownMsg, DownType, Heartbeat, MAX_SLOTS, NODE_VERSION,
    NodeBoard, RecordKind, Security, SightingBatchWriter, SightingMsg, Slot, UPDATE_CHUNK_MAX,
    UpType, UpdateState, UpdateStatus, Vdd, Version,
};
use wartui_proto::dedup::{DEDUP_RSSI_GAIN_BLE_DB, DEDUP_RSSI_GAIN_WIFI_DB, DedupRing};
use wartui_proto::fan::{FanDuty, FanStats};
use wartui_proto::link::{
    BridgeToHost, FromNodeBody, HostToBridge, LogStr, LoopPhase, NO_GPS_RECEIVER, NodeImageCache,
    PowerOutcome, RadioInfo, RadioLinkStats, UPDATE_STATUS_MAX, Undelivered, UpdateStatusBytes,
    cache_chunk_body,
};
use wartui_proto::mac::Mac;
use wartui_proto::node::{ASSIGNED_BEAT_MS, CHANNEL_DWELL_MS, IDLE_BEAT_MS};
use wartui_proto::ota::{Receiver, SectorSink};
use wartui_proto::plan::{ChannelPool, ChannelSet, NUM_SCAN_CHANNELS, SCAN_CHANNELS};
use wartui_proto::radio_link::update::Op;
use wartui_proto::reset::ResetCause;

use crate::{BridgeInfo, LinkEvent, LinkHandle, TransportError, link_pair};

/// How the simulated fleet should behave.
#[derive(Debug, Clone)]
pub struct SimConfig {
    /// How many nodes to pretend are out there, one per slot. Nodes past [`MAX_SLOTS`] are not
    /// started.
    pub node_count: u8,
    /// Time scale. 1.0 is real time; larger runs the fleet faster, which is
    /// what tests want.
    pub speed: f64,
    /// Seeds the fake world, so a run is reproducible.
    pub seed: u64,
    /// Size of the imaginary neighborhood.
    pub wifi_networks: u16,
    /// Chance per advertiser slot in a scan that a BLE advertiser shows up, for the
    /// one node holding the Bluetooth assignment. BLE addresses rotate for privacy,
    /// so these never dedup and keep the stream alive.
    ///
    /// Per scan rather than per dwell, because such a node dwells on nothing: it
    /// runs one scan after another, and each has [`ADVERTISERS_PER_SCAN`] slots
    /// in it.
    ///
    /// A rate rather than a model; what is faithful is *who* emits these, so a
    /// fleet where nobody was asked reports no Bluetooth at all.
    pub ble_chance: f64,
    /// Give a network a new address once it has been heard this many times, as a moving
    /// fleet leaves networks behind and meets new ones.
    ///
    /// `None`, the default, keeps the neighborhood the same forever, which is a parked
    /// fleet. Counted per network across the whole fleet, and on every hearing rather than
    /// every report, so a node whose ring suppresses a network still moves it on.
    pub sightings_per_address: Option<u32>,
    /// The index of one node that heartbeats a minor version behind [`NODE_VERSION`].
    ///
    /// `None` by default. A test knob: no CLI flag sets it. The host shows such a node as
    /// `needs update` and never assigns it.
    pub outdated_node: Option<u8>,
    /// The firmware version every node but [`Self::outdated_node`] heartbeats.
    /// [`NODE_VERSION`] by default; `wartui ota --sim` sets an older one, so the fleet has
    /// something to update to.
    pub fleet_version: Version,
    /// The index of one node that never acknowledges this share of the update chunks sent to it,
    /// so the bridge answers them `Undelivered`, as a node bus transfer that fails every try is.
    /// `None` by default. A test knob, for the host's go-back.
    pub lossy_node: Option<(u8, f64)>,
    /// The index of one node whose updated image is never confirmed: it ignores the host
    /// while on trial and rolls back to its old image after three boots. `None` by default.
    /// A test knob, for the host's rollback detection.
    pub rejecting_node: Option<u8>,
    /// The index of one node whose `Verified` status is lost on the way to the host: it
    /// switches and reboots all the same. `None` by default. A test knob, for the host's
    /// recovery from a lost `Verified`.
    pub mute_verified_node: Option<u8>,
    /// Counts the chunk commands answered `NoImage`: each sent before the cache verified the
    /// image it names. `None` by default. A test knob, for the host's wait on the upload.
    pub no_image_answers: Option<Arc<AtomicU32>>,
    /// The bridge's cache drops its image once it has built this many chunks from it, as a P4
    /// that reboots or whose host gate closes does. `None` by default. A test knob, for the
    /// host's answer to a lost cache.
    pub drop_cache_after: Option<u32>,
    /// The channel pool the bridge holds when the simulator starts, which `Status` reports until
    /// a `SetPool` changes it. [`ChannelPool::All`] by default, as a P4 with none stored.
    pub pool: ChannelPool,
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            node_count: 3,
            speed: 1.0,
            seed: 0x5EED,
            wifi_networks: 120,
            ble_chance: 0.15,
            sightings_per_address: None,
            outdated_node: None,
            fleet_version: NODE_VERSION,
            lossy_node: None,
            rejecting_node: None,
            mute_verified_node: None,
            no_image_answers: None,
            drop_cache_after: None,
            pool: ChannelPool::All,
        }
    }
}

/// `version` one minor back, patch kept: what [`SimConfig::outdated_node`] announces, and
/// what tests use for a node that needs an update.
///
/// # Panics
/// When `version`'s minor is 0, which has no minor before it.
#[must_use]
pub fn minor_behind(version: Version) -> Version {
    let Some(minor) = version.minor.checked_sub(1) else {
        panic!("{version} has no minor before it; give the outdated node another version");
    };
    Version { minor, ..version }
}

/// Advertisers one simulated scan can turn up, each subject to
/// [`SimConfig::ble_chance`].
///
/// A scan and not a dwell: the node holding the Bluetooth assignment sniffs no Wi-Fi,
/// so it runs one scan after another, and one advertiser a scan would be a trickle
/// where an ordinary room is a stream — a real scan hears about fifty
/// (`docs/phase-1-findings.md`), most of them addresses it has never seen.
const ADVERTISERS_PER_SCAN: usize = 12;

/// How long one simulated Bluetooth scan holds the antenna, mirroring
/// `firmware/node/src/ble.rs`'s `SCAN_MS`. Simulated time rather than wall time, so
/// the fake scan costs nothing but a tick of the paused clock.
const SCAN_MS: u32 = 500;

/// How long a simulated node stays in update mode without an update record, mirroring the
/// firmware's `SILENCE`.
const UPDATE_SILENCE_MS: u64 = 30_000;

/// Boots a trial image gets before the node rolls back, the firmware's own
/// [`wartui_proto::ota::TRIAL_BOOTS`]. A [`SimConfig::rejecting_node`] spends one idle beat on
/// each.
const TRIAL_BOOTS: u64 = wartui_proto::ota::TRIAL_BOOTS as u64;

/// The bytes an update slot holds, as `firmware/node/partitions.csv` sizes it.
const SLOT_CAPACITY: u32 = 0x1E_0000;

/// The bytes the bridge's node image cache holds, the P4's `CACHE_MAX`.
const CACHE_CAPACITY: usize = 2 * 1024 * 1024;

/// What the bus delivers to a simulated node.
///
/// Mirrors what the firmware reads from the P4's records: an assignment to adopt, a request
/// to empty the dedup ring, an update record, as received, or the confirmation of an image on
/// trial.
#[derive(Debug, Clone)]
enum SimCommand {
    Assign(Assign),
    Clear,
    Update(u8, Vec<u8>),
    Confirm,
    /// The carrier switched the node off and on.
    PowerCycle,
}

/// What the bridge needs to know to route a record to one node.
#[derive(Debug)]
struct Route {
    slot: Slot,
    tx: mpsc::Sender<SimCommand>,
    /// The share of update chunks undelivered ([`SimConfig::lossy_node`]).
    chunk_loss: f64,
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

    /// The slot the simulator puts node `index` in.
    ///
    /// # Panics
    /// When `index` is past [`MAX_SLOTS`].
    #[must_use]
    pub fn node_slot(index: u8) -> Slot {
        Slot::from_u8(index).expect("a simulated node sits in a slot")
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
        let started = tokio::time::Instant::now();

        // Nothing a node says is observable until the dongle has announced itself,
        // so the fake fleet waits rather than racing it: an unassigned node heartbeats
        // the moment it starts, and a host that has not seen a bridge yet has
        // nowhere to file that.
        let (attached_tx, attached_rx) = tokio::sync::watch::channel(false);

        let mut routes: Vec<Route> = Vec::new();
        let mut fleet = Vec::new();
        let count = self.config.node_count.min(u8::try_from(MAX_SLOTS).unwrap_or(u8::MAX));
        for index in 0..count {
            let (admin_tx, admin_rx) = mpsc::channel(8);
            let chunk_loss = match self.config.lossy_node {
                Some((lossy, rate)) if lossy == index => rate,
                _ => 0.0,
            };
            routes.push(Route { slot: Self::node_slot(index), tx: admin_tx, chunk_loss });
            let version = if self.config.outdated_node == Some(index) {
                minor_behind(NODE_VERSION)
            } else {
                self.config.fleet_version
            };
            let mut node = SimNode::new(index, version);
            node.rejecting = self.config.rejecting_node == Some(index);
            node.mute_verified = self.config.mute_verified_node == Some(index);
            fleet.push((node, admin_rx));
        }

        let events = plumbing.events.clone();
        let no_image = self.config.no_image_answers.clone();
        let drop_cache = self.config.drop_cache_after;
        let pool = self.config.pool;
        tokio::spawn(run_bridge(
            plumbing,
            routes,
            started,
            attached_tx,
            no_image,
            drop_cache,
            pool,
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

/// The speed the simulated fan reports at `duty`, after the bench sweep in
/// `docs/node-bus-findings.md` § "The lid fan's duty and speed": stopped below 15 %, and about
/// 40 rpm a percent above it, from 450 rpm at 15 % to 3850 at full.
fn sim_fan_rpm(duty: FanDuty) -> u16 {
    match u16::from(duty.get()) {
        pct @ 15.. => 40 * pct - 150,
        _ => 0,
    }
}

/// The dongle half: announces itself, routes records to nodes, answers status.
async fn run_bridge(
    mut plumbing: crate::LinkPlumbing,
    routes: Vec<Route>,
    started: tokio::time::Instant,
    attached: tokio::sync::watch::Sender<bool>,
    no_image: Option<Arc<AtomicU32>>,
    mut drop_cache_after: Option<u32>,
    mut pool: ChannelPool,
) {
    let info = BridgeInfo {
        mac: [0x02, 0x00, 0x00, 0x00, 0xBB, 0x01],
        fw_version: format!("{}-sim", env!("CARGO_PKG_VERSION")),
        // A simulated bridge has only ever just been switched on, so anything else
        // here is a fault on screen that no amount of looking could explain.
        reset_cause: ResetCause::PowerOn,
        last_phase: LoopPhase::Unknown,
        // Announced the instant it came up, and it never reboots.
        uptime_ms: 0,
        // Every slot driven, so a full simulated fleet shows silk names.
        node_board: NodeBoard::Carrier14,
        // The C6 behind the P4, as the real bridge reports it.
        radio: Some(RadioInfo {
            mac: [0x02, 0x00, 0x00, 0x00, 0xBB, 0x01],
            fw_version: wartui_proto::link::ShortStr::try_from(concat!(
                env!("CARGO_PKG_VERSION"),
                "-sim"
            ))
            .unwrap_or_default(),
            reset_cause: ResetCause::PowerOn,
            last_phase: LoopPhase::Unknown,
            uptime_ms: 0,
        }),
        // No image to name: it takes no update over the link.
        image: None,
    };
    if plumbing.events.send(LinkEvent::Connected(info)).await.is_err() {
        return;
    }
    // A P4 with nothing on its GPS port says so with each announcement.
    let no_gps =
        BridgeToHost::Error { message: LogStr::try_from(NO_GPS_RECEIVER).unwrap_or_default() };
    if plumbing.events.send(LinkEvent::Message(no_gps)).await.is_err() {
        return;
    }
    let _ = attached.send(true);

    let mut rng = Xorshift::new(0x0DD5_EED5);
    let mut cache = sim_cache();
    let mut fan_duty = FanDuty::FULL;

    while let Some(cmd) = plumbing.commands.recv().await {
        let reply = match cmd {
            // The real bridge replies with `Ready`, but `Connected` has already
            // gone out above and a host counting connections would be confused.
            HostToBridge::Identify => None,
            // No receiver on its GPS port to write to.
            HostToBridge::GpsWrite { .. } => None,
            HostToBridge::GetStatus => Some(BridgeToHost::Status {
                rx_count: 0,
                dropped_tx: 0,
                uptime_ms: elapsed_ms(started),
                rx_oversize: 0,
                queues: wartui_proto::link::QueueStats::new(),
                radio_link: RadioLinkStats::default(),
                host_uart_errors: 0,
                fan: Some(FanStats { duty: fan_duty, rpm: Some(sim_fan_rpm(fan_duty)) }),
                pool,
            }),
            HostToBridge::Reset => Some(BridgeToHost::Log {
                message: LogStr::try_from("simulated reset ignored").unwrap_or_default(),
            }),
            // It holds no image to update.
            HostToBridge::Update(_) => Some(BridgeToHost::Error {
                message: LogStr::try_from("this bridge takes no update over its link")
                    .unwrap_or_default(),
            }),
            HostToBridge::ToNode { slot, id, kind, body } => {
                Some(match deliver(&routes, slot, kind, &body, &mut rng).await {
                    Ok(()) => BridgeToHost::Delivered { id },
                    Err(reason) => BridgeToHost::Undelivered { id, reason },
                })
            }
            // An empty socket is switched off and on as well, so it is `Done` too.
            HostToBridge::PowerCycle { slot } => {
                if let Some(route) = routes.iter().find(|route| route.slot == slot) {
                    let _ = route.tx.send(SimCommand::PowerCycle).await;
                }
                Some(BridgeToHost::PowerCycled { slot, outcome: PowerOutcome::Done })
            }
            // Both answered by the next `Status`, as the P4's are.
            HostToBridge::SetPool { pool: wanted } => {
                pool = wanted;
                None
            }
            HostToBridge::SetFan { duty } => {
                fan_duty = duty;
                None
            }
            HostToBridge::CacheImage(bytes) => match Op::decode(&bytes) {
                Some(op) => cache.take(op).map(cache_status),
                None => Some(BridgeToHost::Error {
                    message: LogStr::try_from("a cache op did not decode").unwrap_or_default(),
                }),
            },
            HostToBridge::ToNodeChunk { slot, id, image, session, offset, len } => {
                if drop_cache_after == Some(0) {
                    cache.reset();
                }
                let mut body = [0u8; UPDATE_CHUNK_MAX];
                let built = cache
                    .image(image)
                    .and_then(|bytes| cache_chunk_body(bytes, session, offset, len, &mut body));
                let kind = DownType::UpdateChunk.as_u8();
                let delivered = match built {
                    Some(n) => {
                        if let Some(left) = &mut drop_cache_after {
                            *left = left.saturating_sub(1);
                        }
                        deliver(&routes, slot, kind, &body[..n], &mut rng).await
                    }
                    None => {
                        if let Some(count) = &no_image {
                            count.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(Undelivered::NoImage)
                    }
                };
                Some(match delivered {
                    Ok(()) => BridgeToHost::Delivered { id },
                    Err(reason) => BridgeToHost::Undelivered { id, reason },
                })
            }
        };
        if let Some(msg) = reply
            && plumbing.events.send(LinkEvent::Message(msg)).await.is_err()
        {
            return;
        }
    }
}

/// `status` as the cache answers it.
fn cache_status(status: UpdateStatus) -> BridgeToHost {
    let mut buf = [0u8; UPDATE_STATUS_MAX];
    let len = Op::Status(status).encode(&mut buf);
    BridgeToHost::CacheImage(UpdateStatusBytes::from_slice(&buf[..len]).unwrap_or_default())
}

/// The bridge's node image cache, as the P4 keeps it in PSRAM: the receiver boxed for its sector
/// buffer.
type SimCache = NodeImageCache<Box<Receiver>, Vec<u8>>;

fn sim_cache() -> SimCache {
    NodeImageCache::new(Box::new(Receiver::new(Board::Esp32c5)), vec![0; CACHE_CAPACITY])
}

/// Hand a record to whichever simulated node sits in `slot`.
async fn deliver(
    routes: &[Route],
    slot: Slot,
    kind: u8,
    body: &[u8],
    rng: &mut Xorshift,
) -> Result<(), Undelivered> {
    let route = routes.iter().find(|route| route.slot == slot).ok_or(Undelivered::NoNode)?;
    let command = match DownMsg::decode(kind, body) {
        Ok(DownMsg::Assign(assign)) => SimCommand::Assign(assign),
        Ok(DownMsg::Clear(_)) => SimCommand::Clear,
        Ok(DownMsg::UpdateConfirm(_)) => SimCommand::Confirm,
        // Never acknowledged on the bus. A delivered record reaches the node's inbox in order.
        Ok(DownMsg::UpdateChunk(_)) if rng.next_f64() < route.chunk_loss => {
            return Err(Undelivered::Timeout);
        }
        Ok(
            DownMsg::UpdateBegin(_)
            | DownMsg::UpdateChunk(_)
            | DownMsg::UpdateCommit(_)
            | DownMsg::UpdateAbort(_),
        ) => SimCommand::Update(kind, body.to_vec()),
        // The bus carries it; the node reads nothing of it.
        Err(_) => return Ok(()),
    };
    route.tx.send(command).await.map_err(|_| Undelivered::NoNode)
}

/// One simulated node's own state, mirroring the firmware's node-side globals.
#[derive(Debug)]
struct SimNode {
    mac: Mac,
    /// Where it sits on the node bus.
    bus_slot: Slot,
    /// The epoch starts at 0 on a fresh node, so the first assignment of any
    /// epoch always takes. Reported in every heartbeat.
    epoch: u8,
    channels: ChannelSet,
    /// Whether this node was asked to scan Bluetooth.
    holds_ble: bool,
    /// The firmware version its heartbeats announce: [`NODE_VERSION`], unless
    /// [`SimConfig::outdated_node`] picked this node.
    version: Version,
    /// Counts completed sweeps — or scans, for the node holding Bluetooth — the
    /// same way the firmware's does: on the sweep or scan completing, whether or
    /// not a heartbeat goes out then.
    hb_counter: u32,
    /// The supply, die and heap readings each heartbeat carries. Fixed, so no node leaks, and
    /// a little different on each node, so the fleet table's rows differ.
    vdd: Vdd,
    die_temp: DieTemp,
    heap_free: u32,
    /// When this node next sends a heartbeat. `None` while unassigned. Set fresh every time
    /// an assignment is adopted while unassigned, not only the first, so a stale deadline from
    /// before this node was last unassigned never fires early.
    next_beat: Option<tokio::time::Instant>,
    /// This node's next sighting batch counter, mirroring `Node::seq` on the
    /// firmware: it advances only once a batch is delivered.
    seq: u16,
    seen: Box<DedupRing>,
    /// When this node booted, on tokio's clock so a paused test controls it.
    boot: tokio::time::Instant,
    speed: f64,
    rng: Xorshift,
    /// What follows is the update side, mirroring `firmware/common/src/ota.rs`.
    ///
    /// The receiver the firmware links, boxed for its sector buffer.
    receiver: Box<Receiver>,
    slot: SimSlot,
    /// When the last update record arrived; update mode lasts [`UPDATE_SILENCE_MS`] past it
    /// while the receiver holds a session.
    update_heard: tokio::time::Instant,
    /// Running an image not yet confirmed, as each heartbeat says.
    trial: bool,
    /// [`SimConfig::rejecting_node`]: a trial image here is never confirmed.
    rejecting: bool,
    /// [`SimConfig::mute_verified_node`]: this node's `Verified` never reaches the host.
    mute_verified: bool,
    /// The image to roll back to, and when, while a rejected one is on trial.
    rollback: Option<(Version, tokio::time::Instant)>,
}

impl SimNode {
    fn new(index: u8, version: Version) -> Self {
        Self {
            mac: SimTransport::node_mac(index),
            bus_slot: SimTransport::node_slot(index),
            version,
            epoch: 0,
            // Nothing until it is told, so a node that has never heard the host is not
            // quietly duplicating the fleet's work.
            channels: ChannelSet::empty(),
            holds_ble: false,
            // 1, not 0, matching the firmware's `Node::new`: the counter is never
            // zero, so a fresh node's first heartbeat already carries a value.
            hb_counter: 1,
            vdd: Vdd::from_mv(3300 - 12 * u16::from(index % 4)),
            die_temp: DieTemp::from_c(40 + i16::from(index % 8)),
            heap_free: 40_000 - 256 * u32::from(index % 8),
            next_beat: None,
            seq: 0,
            seen: Box::default(),
            boot: tokio::time::Instant::now(),
            speed: 1.0,
            rng: Xorshift::new(0xA5A5_0000 ^ u64::from(index).wrapping_mul(0x9E37_79B9)),
            receiver: Box::new(Receiver::new(Board::Esp32c5)),
            slot: SimSlot::new(SLOT_CAPACITY),
            update_heard: tokio::time::Instant::now(),
            trial: false,
            rejecting: false,
            mute_verified: false,
            rollback: None,
        }
    }

    /// Whether the node is in update mode: heartbeating every second, listening throughout.
    fn updating(&self) -> bool {
        self.receiver.is_active()
    }

    /// Leave update mode once no update record has come for [`UPDATE_SILENCE_MS`].
    fn expire_update(&mut self) {
        if self.updating() && self.update_heard.elapsed() >= scaled(UPDATE_SILENCE_MS, self.speed) {
            self.receiver.reset();
        }
    }

    /// End a trial on hearing the host, unless this node rejects its image.
    fn confirm(&mut self) {
        if self.trial && !self.rejecting {
            self.trial = false;
        }
    }

    /// Act on an update record, returning the status to send back, if any. A `Verified`
    /// image is booted once its status is sent ([`Self::boot_verified`]).
    fn update(&mut self, kind: u8, body: &[u8]) -> Option<UpdateStatus> {
        self.confirm();
        self.update_heard = tokio::time::Instant::now();
        match DownMsg::decode(kind, body).ok()? {
            // An image on trial is the only one known to work, so nothing overwrites it.
            DownMsg::UpdateBegin(msg) if self.trial => Some(UpdateStatus {
                session: msg.session,
                state: UpdateState::FlashError,
                next_offset: 0,
            }),
            DownMsg::UpdateBegin(msg) => Some(self.receiver.begin(&msg, &self.slot)),
            DownMsg::UpdateChunk(msg) => self.receiver.chunk(&msg, &mut self.slot),
            DownMsg::UpdateCommit(msg) => Some(self.receiver.commit(&msg, &mut self.slot)),
            DownMsg::UpdateAbort(msg) => Some(self.receiver.abort(&msg)),
            _ => None,
        }
    }

    /// Reboot into the image the receiver verified, on trial, as the firmware's install does.
    fn boot_verified(&mut self) {
        let Some(version) = self.receiver.version() else { return };
        let old = self.version;
        self.reboot(version);
        self.trial = true;
        if self.rejecting {
            let boots = scaled(TRIAL_BOOTS * u64::from(IDLE_BEAT_MS), self.speed);
            self.rollback = Some((old, tokio::time::Instant::now() + boots));
        }
    }

    /// Roll a rejected image back once its boots are spent.
    fn roll_back_if_due(&mut self) {
        if let Some((old, at)) = self.rollback
            && tokio::time::Instant::now() >= at
        {
            self.reboot(old);
        }
    }

    /// Restart on the image it runs, as the carrier's power switch does. A trial stays a trial,
    /// and a rejected image's rollback stays due, since the trial sector survives the cut.
    fn power_cycle(&mut self) {
        let (trial, rollback) = (self.trial, self.rollback);
        self.reboot(self.version);
        self.trial = trial;
        self.rollback = rollback;
    }

    /// Start over on `version`, forgetting everything RAM held.
    fn reboot(&mut self, version: Version) {
        self.version = version;
        self.trial = false;
        self.rollback = None;
        self.receiver.reset();
        self.epoch = 0;
        self.channels = ChannelSet::empty();
        self.holds_ble = false;
        self.hb_counter = 1;
        self.next_beat = None;
        self.forget_reported();
        self.boot = tokio::time::Instant::now();
    }

    /// Whether the node has been told what to scan.
    ///
    /// The channels, or the Bluetooth scan. An empty mask with the flag clear counts
    /// as nothing, the way the firmware counts it: no assignment means "scan nothing", so
    /// reaching this needs a confused host, and idling is the same answer as never
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
    ///
    /// Resets `seen` first when the new share changes what this node scans or its
    /// Bluetooth flag, mirroring `Node::adopt`.
    fn apply(&mut self, assign: Assign) {
        if assign.epoch == self.epoch {
            return;
        }
        if assign.channels != self.channels || assign.scan_ble() != self.scanning_ble() {
            self.forget_reported();
        }
        self.epoch = assign.epoch;
        self.channels = assign.channels;
        self.holds_ble = assign.scan_ble();
    }

    /// Whether this node is the one scanning Bluetooth.
    fn scanning_ble(&self) -> bool {
        self.holds_ble
    }

    /// Empty the dedup ring, mirroring `Node::forget_reported`.
    fn forget_reported(&mut self) {
        self.seen.clear();
    }

    /// Whether the firmware would report this sighting now, recording it if so.
    /// A simulated delivery cannot fail, so asking and recording are one step.
    fn worth_reporting(&mut self, network: &Network) -> bool {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let now_ms = (self.boot.elapsed().as_secs_f64() * 1000.0 * self.speed) as u64 as u32;
        let gain_db =
            if self.bluetooth_only() { DEDUP_RSSI_GAIN_BLE_DB } else { DEDUP_RSSI_GAIN_WIFI_DB };
        self.seen.offer(network.bssid, Some(network.rssi), gain_db, now_ms)
    }
}

/// An update slot held in memory, grown as it is written.
#[derive(Debug)]
struct SimSlot {
    flash: Vec<u8>,
    capacity: u32,
}

impl SimSlot {
    const fn new(capacity: u32) -> Self {
        Self { flash: Vec::new(), capacity }
    }

    fn reach(&mut self, end: usize) {
        if self.flash.len() < end {
            self.flash.resize(end, 0xFF);
        }
    }
}

impl SectorSink for SimSlot {
    type Error = ();

    fn capacity(&self) -> u32 {
        self.capacity
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), ()> {
        let (at, len) = (offset as usize, len as usize);
        self.reach(at + len);
        self.flash[at..at + len].fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, data: &[u8]) -> Result<(), ()> {
        let at = offset as usize;
        self.reach(at + data.len());
        self.flash[at..at + data.len()].copy_from_slice(data);
        Ok(())
    }
}

async fn run_node(
    mut node: SimNode,
    world: Arc<World>,
    events: mpsc::Sender<LinkEvent>,
    mut admin_rx: mpsc::Receiver<SimCommand>,
    speed: f64,
    started: tokio::time::Instant,
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
        node.roll_back_if_due();
        if node.updating() {
            // Update mode: listening throughout, heartbeating every second whatever the
            // assignment, which it keeps to return to.
            if beat(&events, &mut node, started).await.is_err() {
                return;
            }
            if nap(
                scaled(u64::from(IDLE_BEAT_MS), speed),
                &mut admin_rx,
                &mut node,
                &events,
                started,
            )
            .await
            .is_break()
            {
                return;
            }
            node.expire_update();
            continue;
        }
        if !node.assigned() {
            // Unassigned: reachable throughout and collecting nothing. Heartbeat first and
            // listen after, because the host assigns in answer to one — so this is how long
            // joining takes. One idle beat is this node's whole cycle, so the counter
            // advances here too.
            if beat(&events, &mut node, started).await.is_err() {
                return;
            }
            node.hb_counter = node.hb_counter.wrapping_add(1);
            if nap(
                scaled(u64::from(IDLE_BEAT_MS), speed),
                &mut admin_rx,
                &mut node,
                &events,
                started,
            )
            .await
            .is_break()
            {
                return;
            }
            // An assignment adopted mid-nap: this node has just heartbeated, so its next one
            // is a whole interval out from here rather than immediate — set every time,
            // since a stale deadline from before this node was last unassigned would
            // otherwise fire on the very next dwell.
            if node.assigned() {
                node.next_beat =
                    Some(tokio::time::Instant::now() + scaled(u64::from(ASSIGNED_BEAT_MS), speed));
            }
            continue;
        }

        // Bluetooth is a whole node's job, so this one dwells on nothing: it runs
        // one scan after another, and its counter advances once a scan, regardless
        // of whether a heartbeat goes out on this pass.
        if node.bluetooth_only() {
            if nap(scaled(u64::from(SCAN_MS), speed), &mut admin_rx, &mut node, &events, started)
                .await
                .is_break()
            {
                return;
            }
            let mut writer = SightingBatchWriter::new(node.seq);
            for _ in 0..ADVERTISERS_PER_SCAN {
                if node.rng.next_f64() >= world.ble_chance {
                    continue;
                }
                let ble = world.ble_sighting(&mut node.rng);
                if !node.worth_reporting(&ble) {
                    continue;
                }
                if !writer.push(&ble.as_msg()) {
                    if flush_batch(&events, &mut node, &writer, started).await.is_err() {
                        return;
                    }
                    writer = SightingBatchWriter::new(node.seq);
                    // An empty batch always has room for one more record.
                    let _ = writer.push(&ble.as_msg());
                }
            }
            if flush_batch(&events, &mut node, &writer, started).await.is_err() {
                return;
            }
            node.hb_counter = node.hb_counter.wrapping_add(1);
            if let Some(deadline) = node.next_beat
                && tokio::time::Instant::now() >= deadline
            {
                if beat(&events, &mut node, started).await.is_err() {
                    return;
                }
                node.next_beat = Some(next_beat_after(deadline, speed));
            }
            continue;
        }

        // A node walks its assigned channels one per step, checking the beat
        // deadline after each, as the firmware does, so the sweep, and therefore the
        // counter, is proportional to how many channels it was given regardless of how
        // the heartbeat timer lines up against it.
        for idx in node.channels.indices() {
            if nap(dwell, &mut admin_rx, &mut node, &events, started).await.is_break() {
                return;
            }
            let channel = SCAN_CHANNELS[usize::from(idx)];
            let mut writer = SightingBatchWriter::new(node.seq);
            for slot in world.on_channel(channel) {
                let net = world.hear(slot);
                if !node.worth_reporting(&net) {
                    continue;
                }
                if !writer.push(&net.as_msg()) {
                    if flush_batch(&events, &mut node, &writer, started).await.is_err() {
                        return;
                    }
                    writer = SightingBatchWriter::new(node.seq);
                    // An empty batch always has room for one more record.
                    let _ = writer.push(&net.as_msg());
                }
            }
            if flush_batch(&events, &mut node, &writer, started).await.is_err() {
                return;
            }
            if let Some(deadline) = node.next_beat
                && tokio::time::Instant::now() >= deadline
            {
                if beat(&events, &mut node, started).await.is_err() {
                    return;
                }
                node.next_beat = Some(next_beat_after(deadline, speed));
            }
        }
        // A completed sweep, whether or not a heartbeat went out along the way.
        node.hb_counter = node.hb_counter.wrapping_add(1);
    }
}

/// Push a heartbeat deadline forward by one [`ASSIGNED_BEAT_MS`], never behind
/// now — the same rule the firmware's `next_beat_after` holds, so a stalled node
/// catching up does not fire a burst of overdue beats.
fn next_beat_after(deadline: tokio::time::Instant, speed: f64) -> tokio::time::Instant {
    let next = deadline + scaled(u64::from(ASSIGNED_BEAT_MS), speed);
    let now = tokio::time::Instant::now();
    if next <= now { now + scaled(u64::from(ASSIGNED_BEAT_MS), speed) } else { next }
}

/// Send one heartbeat, carrying the node's current counter and held epoch.
///
/// The counter tracks completed sweeps or scans on its own, so it advances
/// whether or not this call goes out; this only ever reads it.
async fn beat(
    events: &mpsc::Sender<LinkEvent>,
    node: &mut SimNode,
    started: tokio::time::Instant,
) -> Result<(), ()> {
    // Capabilities are what tell the host this is a node it can drive, so a fleet
    // without them is a fleet the planner ignores.
    let msg = Heartbeat {
        capabilities: Capabilities {
            version: node.version,
            trial: node.trial,
            ..Capabilities::here(Board::Esp32c5)
        },
        mac: node.mac,
        counter: node.hb_counter,
        epoch: node.epoch,
        // A simulated node's pending buffers never fill.
        wifi_refused: 0,
        ble_refused: 0,
        // A simulated node boots only by being started.
        reset_cause: ResetCause::PowerOn,
        vdd: node.vdd,
        die_temp: node.die_temp,
        heap_free: node.heap_free,
    };
    send_record(events, node.bus_slot, UpType::Heartbeat, &msg.encode(), started).await
}

/// Sleep, adopting any assignment that arrives meanwhile.
///
/// The real node services the bus between dwells, so an assignment lands within a dwell of
/// its delivery. Adopting it mid-sleep is the same to the host.
async fn nap(
    duration: Duration,
    admin_rx: &mut mpsc::Receiver<SimCommand>,
    node: &mut SimNode,
    events: &mpsc::Sender<LinkEvent>,
    started: tokio::time::Instant,
) -> std::ops::ControlFlow<()> {
    nap_until(tokio::time::Instant::now() + duration, admin_rx, node, events, started).await
}

/// Sleep until a deadline, adopting any assignment that arrives meanwhile and answering
/// update records.
///
/// The primitive [`nap`] is a wrapper over. A node that boots a verified image returns
/// early, as the firmware's reboot leaves whatever it was doing.
async fn nap_until(
    deadline: tokio::time::Instant,
    admin_rx: &mut mpsc::Receiver<SimCommand>,
    node: &mut SimNode,
    events: &mpsc::Sender<LinkEvent>,
    started: tokio::time::Instant,
) -> std::ops::ControlFlow<()> {
    use std::ops::ControlFlow::{Break, Continue};
    loop {
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return Continue(()),
            received = admin_rx.recv() => match received {
                Some(SimCommand::Assign(assign)) => {
                    node.confirm();
                    node.apply(assign);
                }
                Some(SimCommand::Clear) => {
                    node.confirm();
                    node.forget_reported();
                }
                Some(SimCommand::Confirm) => node.confirm(),
                // A fresh boot of the image it runs, still on trial if it was.
                Some(SimCommand::PowerCycle) => {
                    node.power_cycle();
                    return Continue(());
                }
                Some(SimCommand::Update(kind, body)) => {
                    let Some(status) = node.update(kind, &body) else { continue };
                    let verified = status.state == UpdateState::Verified;
                    let body = status.encode();
                    if !(verified && node.mute_verified)
                        && send_record(events, node.bus_slot, UpType::UpdateStatus, &body, started)
                            .await
                            .is_err()
                    {
                        return Break(());
                    }
                    if verified {
                        node.boot_verified();
                        return Continue(());
                    }
                }
                None => return Break(()),
            },
        }
    }
}

/// Send `writer`'s batch if it holds anything, advancing `node.seq` only once it is
/// delivered — the reason a dwell or scan that heard nothing sends no batch at all.
async fn flush_batch(
    events: &mpsc::Sender<LinkEvent>,
    node: &mut SimNode,
    writer: &SightingBatchWriter,
    started: tokio::time::Instant,
) -> Result<(), ()> {
    if writer.is_empty() {
        return Ok(());
    }
    send_record(events, node.bus_slot, UpType::Sightings, writer.as_bytes(), started).await?;
    node.seq = node.seq.wrapping_add(1);
    Ok(())
}

async fn send_record(
    events: &mpsc::Sender<LinkEvent>,
    slot: Slot,
    kind: UpType,
    body: &[u8],
    started: tokio::time::Instant,
) -> Result<(), ()> {
    let body = FromNodeBody::from_slice(body).expect("a full batch fits the 1024-byte body");
    let msg = BridgeToHost::FromNode { slot, rx_us: elapsed_us(started), kind: kind.as_u8(), body };
    events.send(LinkEvent::Message(msg)).await.map_err(|_| ())
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

fn elapsed_us(started: tokio::time::Instant) -> u32 {
    u32::try_from(started.elapsed().as_micros() % WRAP).unwrap_or(0)
}

fn elapsed_ms(started: tokio::time::Instant) -> u32 {
    u32::try_from(started.elapsed().as_millis() % WRAP).unwrap_or(0)
}

/// A fake neighborhood, generated once and shared by every node — so two nodes
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
    /// is the world's, shared by the fleet, so it is the neighborhood that moves on rather
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
/// world's generator, so turning over cannot change the neighborhood a seed produces.
/// Locally administered, like every simulated address.
fn moved(bssid: Mac, generation: u32) -> Mac {
    let mut first = [0u8; 8];
    first[..6].copy_from_slice(&bssid);
    let v = mix(mix(u64::from_le_bytes(first)) ^ u64::from(generation)).to_le_bytes();
    [0x02, v[1], v[2], v[3], v[4], v[5]]
}

/// SplitMix64's finalizer: every input bit reaches every output bit.
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
/// neighborhood exercises the trailer the way a real one would.
const OPEN_ROAMING: [u8; 17] = [
    0x02, 0x55, //
    0x5A, 0x03, 0xBA, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x20, 0x00,
];

impl Network {
    /// The observation a node would report about it.
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

#[cfg(test)]
mod tests {
    use super::*;
    use wartui_proto::ota::SECTOR;

    /// The capabilities in the heartbeat `node` sends now.
    async fn heartbeat_capabilities(node: &mut SimNode) -> Capabilities {
        let (tx, mut rx) = mpsc::channel(1);
        beat(&tx, node, tokio::time::Instant::now()).await.expect("the channel is open");
        let Some(LinkEvent::Message(BridgeToHost::FromNode { body, .. })) = rx.recv().await else {
            panic!("a heartbeat record");
        };
        Heartbeat::decode(&body).expect("a valid heartbeat").capabilities
    }

    #[tokio::test(start_paused = true)]
    async fn sim_node_reports_trial_in_heartbeat_when_power_cycled_on_trial_image() {
        let mut node = SimNode::new(0, NODE_VERSION);
        node.trial = true;
        node.power_cycle();
        assert!(heartbeat_capabilities(&mut node).await.trial);
    }

    #[tokio::test(start_paused = true)]
    async fn sim_node_rolls_back_when_power_cycled_on_rejected_image() {
        let old = minor_behind(NODE_VERSION);
        let mut node = SimNode::new(0, NODE_VERSION);
        node.trial = true;
        node.rejecting = true;
        let due = Duration::from_secs(3);
        node.rollback = Some((old, tokio::time::Instant::now() + due));
        node.power_cycle();
        tokio::time::advance(due).await;
        node.roll_back_if_due();
        let capabilities = heartbeat_capabilities(&mut node).await;
        assert_eq!((capabilities.version, capabilities.trial), (old, false));
    }

    /// Two sectors and a bit, so the upload ends on a partial sector.
    fn cache_image() -> (Vec<u8>, [u8; 32]) {
        use sha2::{Digest, Sha256};
        let bytes: Vec<u8> = (0..2 * SECTOR + 100).map(|i| (i % 251) as u8).collect();
        let sha256 = Sha256::digest(&bytes).into();
        (bytes, sha256)
    }

    async fn started() -> LinkHandle {
        let mut link = SimTransport::new(SimConfig { node_count: 1, ..SimConfig::default() })
            .start()
            .expect("starting the simulator");
        while !matches!(link.recv().await, Some(LinkEvent::Connected(_))) {}
        link
    }

    fn cache_op(link: &LinkHandle, op: &Op<'_>) {
        let mut buf = [0u8; wartui_proto::radio_link::update::OP_MAX];
        let len = op.encode(&mut buf);
        let op = wartui_proto::link::UpdateOp::from_slice(&buf[..len]).expect("an op fits");
        link.send_urgent(HostToBridge::CacheImage(op)).expect("queued");
    }

    /// The cache's next status.
    async fn next_cache_status(link: &mut LinkHandle) -> UpdateStatus {
        loop {
            if let Some(LinkEvent::Message(BridgeToHost::CacheImage(bytes))) = link.recv().await
                && let Some(Op::Status(status)) = Op::decode(&bytes)
            {
                return status;
            }
        }
    }

    /// Upload `bytes` into the cache, until it reports them verified.
    async fn upload(link: &mut LinkHandle, bytes: &[u8], sha256: [u8; 32]) {
        let len = u32::try_from(bytes.len()).expect("a small image");
        cache_op(link, &Op::Begin { session: 1, len, sha256 });
        assert_eq!(next_cache_status(link).await.state, UpdateState::Receiving);
        let chunk = wartui_proto::bus::UPDATE_CHUNK;
        for (i, data) in bytes.chunks(chunk).enumerate() {
            let offset = u32::try_from(i * chunk).expect("a small image");
            cache_op(link, &Op::Chunk { session: 1, offset, data });
        }
        cache_op(link, &Op::Commit { session: 1 });
        while next_cache_status(link).await.state != UpdateState::Verified {}
    }

    /// Ask for the first chunk of the image tagged `image`, returning the bridge's answer.
    async fn first_chunk(link: &mut LinkHandle, image: u32) -> BridgeToHost {
        let slot = SimTransport::node_slot(0);
        let cmd =
            HostToBridge::ToNodeChunk { slot, id: 7, image, session: 9, offset: 0, len: 1024 };
        link.send_urgent(cmd).expect("queued");
        loop {
            match link.recv().await {
                Some(LinkEvent::Message(
                    answer @ (BridgeToHost::Delivered { id: 7 }
                    | BridgeToHost::Undelivered { id: 7, .. }),
                )) => return answer,
                Some(_) => {}
                None => panic!("the link closed"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn sim_delivers_tonodechunk_after_verified_upload() {
        let mut link = started().await;
        let (bytes, sha256) = cache_image();
        upload(&mut link, &bytes, sha256).await;
        let answer = first_chunk(&mut link, wartui_proto::link::image_tag(&sha256)).await;
        assert_eq!(answer, BridgeToHost::Delivered { id: 7 });
        // The node read it: it holds no session 9, and says so.
        loop {
            if let Some(LinkEvent::Message(BridgeToHost::FromNode { kind, body, .. })) =
                link.recv().await
                && kind == UpType::UpdateStatus.as_u8()
            {
                let status = UpdateStatus::decode(&body).expect("a status");
                assert_eq!((status.session, status.state), (9, UpdateState::Idle));
                return;
            }
        }
    }
}
