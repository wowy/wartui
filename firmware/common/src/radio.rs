//! The ESP-NOW half of a bridge: what the radio is told and what it hears, with no
//! knowledge of the link that carries either to the host.
//!
//! One copy for every firmware that owns a radio and bridges it. The single-chip bridge
//! connects it to its USB link in memory; a radio chip behind a P4 connects it to the SPI
//! link. Two copies would drift, and the rules here — the broadcast-slot give-up, the
//! 24 Mbps rate, which status a send earns — are ones the host reads meaning into.
//!
//! The functions return what to say as a [`Said`] rather than saying it, because each
//! caller has its own way to reach the host.
//!
//! A send runs in its own future, [`run_jobs`], joined with the caller's loop. Its
//! transmit callback takes 2–4 ms when acknowledged and up to 35 ms when not, and
//! `esp-radio`'s receive queue holds ten frames and drops the oldest when full. So nothing
//! here blocks while a send is in flight: [`queue_send`] hands the job over without waiting,
//! the loop goes on draining the receive queue, and the result comes back as a [`Done`].
//!
//! Peer and transmit-power commands take the same queue, though each is a call of
//! microseconds. The host orders them against its sends: a peer removed behind a send to
//! it, or a send meant for the power before a change, depends on the radio acting in the
//! order the link delivered. The queue's entries are small, so it absorbs the host removing
//! a whole fleet's peers in one batch, and a send's payload waits in a pool of its own.
//!
//! The air is plaintext ESP-NOW in both directions: no pairing handshake and no key
//! ([`peer`]).

use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::channel::Channel;
use esp_hal::time::Instant;
use esp_radio::esp_now::{
    EspNowError, EspNowManager, EspNowSender, EspNowWifiInterface, PeerInfo, PhyMode, RateConfig,
    ReceivedData, WifiPhyRate,
};
use esp_radio::wifi::WifiController;
/// The channel the fleet speaks on, and the only one a bridge ever sits on. Shared
/// with the node firmware and the host planner rather than spelled again here: nothing
/// on the air negotiates this number, so the only thing keeping the three ends on the
/// same channel is that they read it from the same place.
use wartui_proto::air::{CONTROL_CHANNEL, MAGIC};
use wartui_proto::heapless::Vec;
use wartui_proto::link::{
    BROADCAST, BridgeToHost, LogLevel, MAX_NODE_PAYLOAD, SendPayload, SendStatus,
};
use wartui_proto::mac::Mac;
use wartui_proto::tx_power::DEFAULT_TX_POWER_QUARTER_DBM;

/// What a radio call has to tell the host, in the words the bridge uses for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Said {
    /// Nothing worth a line.
    Nothing,
    /// A `BridgeToHost::Log` line.
    Log(LogLevel, &'static str),
    /// A `BridgeToHost::Error` line.
    Error(&'static str),
}

/// What [`setup`] did.
pub struct SetupReport {
    /// The transmit power the radio is known to be at, or `None` when the boot attempt
    /// was refused and what it is at is anyone's guess.
    pub tx_power: Option<i8>,
    /// Each failure, in the order it happened.
    pub said: Vec<Said, 3>,
}

/// Bring the radio to where a bridge needs it: the 2 dBm fallback power, parked on
/// [`CONTROL_CHANNEL`], and the broadcast peer at 24 Mbps.
///
/// The standalone fallback is 2 dBm. The host restores its configured value after every
/// connection, through [`set_tx_power_command`].
pub fn setup(controller: &mut WifiController<'_>, manager: &EspNowManager) -> SetupReport {
    let mut said = Vec::new();
    let tx_power = controller.set_max_tx_power(DEFAULT_TX_POWER_QUARTER_DBM).is_ok();
    if !tx_power {
        let _ = said.push(Said::Error("could not cap transmit power at 2 dBm"));
    }
    if manager.set_channel(CONTROL_CHANNEL).is_err() {
        let _ = said.push(Said::Error("could not park the radio on the default channel"));
    }
    // The one peer not added through `add_peer`: `esp-radio` registers it at init.
    if !set_peer_rate(manager, &BROADCAST) {
        let _ =
            said.push(Said::Error("could not set the broadcast peer's rate; it stays at 1 Mbps"));
    }
    SetupReport { tx_power: tx_power.then_some(DEFAULT_TX_POWER_QUARTER_DBM), said }
}

/// The host's `SetTxPower`, tracking in `known` the power the radio is at.
///
/// Only a change is worth a line: the host re-sends this with every status poll so that
/// a command lost to a full queue cannot leave the radio at its fallback for the rest of
/// the session.
pub fn set_tx_power_command(
    controller: &mut WifiController<'_>,
    power: i8,
    known: &mut Option<i8>,
) -> Said {
    if controller.set_max_tx_power(power).is_err() {
        return Said::Error("the radio refused that transmit power");
    }
    let changed = *known != Some(power);
    *known = Some(power);
    if changed { Said::Log(LogLevel::Info, "transmit power changed") } else { Said::Nothing }
}

/// The host's `AddPeer`.
pub fn add_peer_command(manager: &EspNowManager, mac: &Mac) -> Said {
    match add_peer(manager, mac) {
        Ok(true) => Said::Log(LogLevel::Debug, "peer added"),
        Ok(false) => Said::Error("peer added, but its rate was refused; it stays at 1 Mbps"),
        // Already known is the outcome the host wanted, not a failure.
        Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => Said::Nothing,
        Err(_) => Said::Error("could not add that peer"),
    }
}

/// The host's `RemovePeer`.
pub fn remove_peer_command(manager: &EspNowManager, mac: &Mac) -> Said {
    match manager.remove_peer(mac) {
        Ok(()) => Said::Log(LogLevel::Debug, "peer removed"),
        // Symmetric with `AddPeer` swallowing `PeerExists`: gone is the outcome
        // the host asked for, and freeing a slot is the idempotent path a full
        // peer table sends it down.
        Err(EspNowError::Error(esp_radio::esp_now::Error::NotFound)) => {
            Said::Log(LogLevel::Debug, "peer was already gone")
        }
        Err(_) => Said::Error("could not remove that peer"),
    }
}

/// What a received frame is to the host.
pub enum Heard {
    /// Ours: the frame went to the caller's `forward`.
    Forwarded,
    /// Longer than any frame a wartui node sends, so it is somebody else's. Counted apart
    /// from frames headed the host's way, and never logged: a busy neighbor would fill the
    /// bulk ring with log lines and evict real sightings.
    Oversize,
    /// Not ours, so never forwarded, counted or logged.
    Foreign,
}

/// Decide what one received frame is to the host, and hand `forward` its `Rx` if it is ours.
///
/// The magic is checked before the copy, so a foreign frame costs a 4-byte compare. The
/// frame is built in the call to `forward` and passed by reference: returning it would
/// copy the whole 1 KB message once more per frame.
pub fn hear(received: &ReceivedData, rx_us: u32, forward: impl FnOnce(&BridgeToHost)) -> Heard {
    let data = received.data();
    if data.len() <= MAX_NODE_PAYLOAD && !data.starts_with(&MAGIC) {
        return Heard::Foreign;
    }
    let Ok(payload) = Vec::from_slice(data) else {
        return Heard::Oversize;
    };
    forward(&BridgeToHost::Rx {
        src: received.info.src_address,
        dst: received.info.dst_address,
        // The only receive-control field every supported chip agrees on.
        //
        // It arrives unsigned: `wifi_pkt_rx_ctrl_t.rssi` is a signed 8-bit
        // bitfield, but the generated accessor extracts the bits unsigned and
        // transmutes, so -62 dBm reaches us as 194 and a clamp to `i8`
        // saturates every frame to 127. Reinterpreting the low byte recovers
        // it, and keeps working if the binding is ever fixed to sign-extend.
        rssi: (received.info.rx_control.rssi as u8) as i8,
        rx_us,
        payload,
    });
    Heard::Forwarded
}

/// One thing the host asked of the radio, in the order it arrived.
///
/// Small, so the queue holds a fleet's worth of peer commands: a send's payload waits in
/// [`Payloads`] instead, in the same order as the sends.
pub enum Job {
    /// One frame to put on the air; its payload is the next in [`Payloads`].
    Send {
        /// The host's id for the send, echoed in its result.
        id: u16,
        dst: Mac,
        /// Add `dst` as a peer if it is not one.
        ensure_peer: bool,
    },
    /// A peer or power command.
    Command(Command),
}

/// The host's peer and power commands.
#[derive(Debug, Clone, Copy)]
pub enum Command {
    /// The host's `AddPeer`, through [`add_peer_command`].
    AddPeer(Mac),
    /// The host's `RemovePeer`, through [`remove_peer_command`].
    RemovePeer(Mac),
    /// The host's `SetTxPower`, through [`set_tx_power_command`].
    SetTxPower(i8),
}

/// What became of a [`Job`].
pub enum Done {
    /// A send's result, for a `SendResult`.
    Sent(SendDone),
    /// What a peer or power command had to say.
    Said(Said),
}

/// What became of a [`Job::Send`].
pub struct SendDone {
    pub id: u16,
    pub status: SendStatus,
    /// When [`run_jobs`] saw the transmit callback, or when the job was refused.
    pub done_at: Instant,
}

/// The jobs waiting for [`run_jobs`], in arrival order.
pub type Jobs<const N: usize> = Channel<NoopRawMutex, Job, N>;

/// The payloads of the sends waiting in [`Jobs`], in the same order.
///
/// Each slot is a full ESP-NOW v2 payload, so there are few; a slot frees when
/// [`run_jobs`] takes its payload to transmit.
pub type Payloads<const P: usize> = Channel<NoopRawMutex, SendPayload, P>;

/// The results waiting for the caller's loop.
pub type Dones<const N: usize> = Channel<NoopRawMutex, Done, N>;

/// Which queue refused a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Full {
    /// [`Jobs`], which every job takes a slot of.
    Jobs,
    /// [`Payloads`], which only a send takes a slot of.
    Payloads,
}

/// Hand a peer or power command to [`run_jobs`] without waiting.
///
/// A full queue refuses it with an `Error` line for the caller to report, since the host
/// would otherwise never learn it lost the command.
pub fn queue_command<const N: usize>(jobs: &Jobs<N>, command: Command) -> Option<Done> {
    if jobs.try_send(Job::Command(command)).is_ok() {
        return None;
    }
    Some(Done::Said(Said::Error(match command {
        Command::AddPeer(_) => "could not add that peer: the queue was full",
        Command::RemovePeer(_) => "could not remove that peer: the queue was full",
        Command::SetTxPower(_) => "could not set the transmit power: the queue was full",
    })))
}

/// Hand a send to [`run_jobs`] without waiting.
///
/// It needs a slot in both queues. Without one it is refused, and the refusal comes back
/// as a `Rejected` [`SendDone`] beside the queue that was full.
pub fn queue_send<const N: usize, const P: usize>(
    jobs: &Jobs<N>,
    payloads: &Payloads<P>,
    id: u16,
    dst: Mac,
    ensure_peer: bool,
    payload: &SendPayload,
) -> Option<(Full, Done)> {
    let full = if payloads.is_full() {
        Full::Payloads
    } else if jobs.is_full() {
        Full::Jobs
    } else {
        // Both have room, and nothing runs between these lines and the checks: every
        // future touching the queues shares one task. So the payload and its job enter
        // together, and `run_jobs` finds them in step.
        let _ = payloads.try_send(payload.clone());
        let _ = jobs.try_send(Job::Send { id, dst, ensure_peer });
        return None;
    };
    let rejected = SendDone { id, status: SendStatus::Rejected, done_at: Instant::now() };
    Some((full, Done::Sent(rejected)))
}

/// Carry out every queued job, one at a time and in order, and report each result.
///
/// A send answers with the *transmit-callback* status rather than the enqueue result.
/// `esp_now_send`'s return value says only that a frame was enqueued; unicast ESP-NOW is
/// MAC-acknowledged, so waiting for the callback turns a guess about delivery into a
/// fact. The wait is an `.await`, so the caller's loop runs while it lasts.
///
/// `tx_power` is what [`setup`] left the radio at. It lives here, beside the only call
/// that changes it.
pub async fn run_jobs<const N: usize, const P: usize>(
    controller: &mut WifiController<'_>,
    manager: &EspNowManager,
    sender: &mut EspNowSender,
    mut tx_power: Option<i8>,
    jobs: &Jobs<N>,
    payloads: &Payloads<P>,
    done: &Dones<N>,
) -> ! {
    loop {
        let result = match jobs.receive().await {
            Job::Send { id, dst, ensure_peer } => {
                // `queue_send` queued it with this job, so it is always there.
                let status = match payloads.try_receive() {
                    Ok(payload) => transmit(manager, sender, &dst, ensure_peer, &payload).await,
                    Err(_) => SendStatus::Rejected,
                };
                // Stamped *after* the transmit callback, not before the send, but only when
                // this future is next polled: at the caller's next await. The bridge's loop
                // awaits when it idles, and on a busy pass only while a job waits, so under a
                // burst the stamp trails the callback until the loop next idles. A
                // T-Dongle-C5 repaint adds up to 36 ms. Against the `rx_us` of the heartbeat
                // that opened the window, this bounds from above the time from "the node is
                // listening" to "the radio says it has it".
                Done::Sent(SendDone { id, status, done_at: Instant::now() })
            }
            Job::Command(Command::AddPeer(mac)) => Done::Said(add_peer_command(manager, &mac)),
            Job::Command(Command::RemovePeer(mac)) => {
                Done::Said(remove_peer_command(manager, &mac))
            }
            Job::Command(Command::SetTxPower(power)) => {
                Done::Said(set_tx_power_command(controller, power, &mut tx_power))
            }
        };
        done.send(result).await;
    }
}

/// Put one frame on the air and report what the radio made of it.
async fn transmit(
    manager: &EspNowManager,
    sender: &mut EspNowSender,
    dst: &Mac,
    ensure_peer: bool,
    payload: &[u8],
) -> SendStatus {
    if !manager.peer_exists(dst) {
        if !ensure_peer {
            return SendStatus::NoPeer;
        }
        match add_peer(manager, dst) {
            // A refused rate leaves the peer at 1 Mbps, which still delivers, and a
            // `SendStatus` has no way to say it.
            Ok(_) => {}
            // The radio's table holds twenty entries, one of them the broadcast
            // peer `esp-radio` registers at init (`esp_now/mod.rs:726`). That slot
            // is worth more to a twentieth node: a bridge only ever *receives*
            // broadcasts, and ESP-NOW delivers a received frame whether or not its
            // sender is a peer. So give it up and retry once. A second refusal is a
            // full table.
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                if manager.remove_peer(&BROADCAST).is_err() {
                    return SendStatus::PeerTableFull;
                }
                match add_peer(manager, dst) {
                    Ok(_) | Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
                    Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
                        return SendStatus::PeerTableFull;
                    }
                    Err(_) => return SendStatus::Rejected,
                }
            }
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists)) => {}
            Err(_) => return SendStatus::Rejected,
        }
    }

    // One future carries both outcomes: the enqueue's refusal as an `Error`, and a
    // callback that reports failure as `SendFailed`.
    match sender.send_async(dst, payload).await {
        // Broadcast is never acknowledged, so success here means only that the
        // frame was sent.
        Ok(()) if *dst == BROADCAST => SendStatus::Broadcast,
        Ok(()) => SendStatus::AckOk,
        Err(EspNowError::SendFailed) => SendStatus::AckFail,
        Err(EspNowError::Error(esp_radio::esp_now::Error::NotFound)) => SendStatus::NoPeer,
        Err(EspNowError::Error(esp_radio::esp_now::Error::PeerListFull)) => {
            SendStatus::PeerTableFull
        }
        Err(_) => SendStatus::Rejected,
    }
}

/// A plaintext station peer on whatever channel the radio is already using.
///
/// `channel: None` becomes 0, which ESP-NOW reads as "the current one", so a peer
/// follows the radio rather than naming `CONTROL_CHANNEL` a second time.
#[must_use]
pub const fn peer(mac: &Mac) -> PeerInfo {
    PeerInfo {
        interface: EspNowWifiInterface::Station,
        peer_address: *mac,
        // wartui does no encrypted ESP-NOW at all, so there is no PMK and no LMK.
        // Nodes must have `use_encryption` off.
        lmk: None,
        channel: None,
        encrypt: false,
    }
}

/// Register a peer and set the rate it is sent at, saying whether the rate took.
///
/// The funnel for every peer a bridge adds. The rate belongs to the peer entry,
/// so a peer registered any other way — or removed and registered again — is sent to
/// at 1 Mbps with nothing to say so. A refused rate is not an error here: that peer
/// is slower, not unreachable.
///
/// # Errors
/// The radio's refusal to register the peer.
pub fn add_peer(manager: &EspNowManager, mac: &Mac) -> Result<bool, EspNowError> {
    manager.add_peer(peer(mac))?;
    Ok(set_peer_rate(manager, mac))
}

/// Send to `mac` at 802.11g 24 Mbps rather than ESP-NOW's 802.11b 1 Mbps default.
///
/// For airtime on the control channel. A 90-byte frame is about 910 µs at 1 Mbps
/// with its long preamble and about 50 µs at 24 Mbps, and every frame the fleet sends
/// is about that short, so the rate and preamble are the whole cost: a heartbeat is a
/// sliver of the 100 ms admin window it opens, not most of it. Not 802.11ax, whose
/// preamble costs more than it saves on frames this short. Receivers need nothing;
/// any 802.11b/g rate decodes unannounced.
///
/// The price is sensitivity, roughly 10 dB against 1 Mbps, which a fleet sharing a
/// car has at the 2 dBm default (`tx_power::DEFAULT_TX_POWER_QUARTER_DBM`): on the
/// bench, a C5 and a C6 beside a bridge arrived at −43 and −58 dBm and lost 0%
/// and 2.3% of their heartbeats over ten minutes.
///
/// Through `esp-radio`'s per-peer `set_peer_rate`, which wraps IDF's
/// `esp_now_set_peer_rate_config`. The interface-wide `esp_wifi_config_espnow_rate` is
/// refused by the C5 and C6 Wi-Fi libraries in Wi-Fi 6 mode (esp-hal #1612).
/// `firmware/node/src/radio.rs` has the node's copy.
#[must_use]
pub fn set_peer_rate(manager: &EspNowManager, mac: &Mac) -> bool {
    let config =
        RateConfig { phy_mode: PhyMode::_11g, rate: WifiPhyRate::Rate24m, ersu: false, dcm: false };
    manager.set_peer_rate(mac, config).is_ok()
}
