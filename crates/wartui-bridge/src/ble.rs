//! Talking to a real bridge over Bluetooth, through its C6's GATT service
//! (`wartui_proto::ble`), with `btleplug` on the tokio runtime the crate already runs.
//!
//! **Finding the bridge.** The C6 advertises `wartui XX:YY`, the bridge's last two octets, so a
//! bridge is found by scanning for that name, for at most [`SCAN_FOR`]. The name carries two
//! octets, so the first `Ready` is checked against the whole spec, and a bridge that only shares
//! the name is refused. The rule is the serial transport's ([`look`]):
//!
//! - **A bridge named with `--bridge` is the only one taken**, remembered or not.
//! - **A remembered bridge is tried first**, as the serial sweep opens its CH343 first: its
//!   Bluetooth address is connected to without a scan when it stands for a bridge the spec names
//!   and the OS lists it. BlueZ lists every bonded device, and CoreBluetooth only what it has
//!   seen, so macOS scans instead.
//! - **Once a connection straight to it has failed, every later try scans**, as the sweep moves
//!   on to the other CH343s. The scan still prefers the remembered bridge while it advertises, so
//!   one that is off or out of range blocks no other bridge, and one that answers is taken back.
//! - **A connection that announced a bridge settles the run on it**, and no other is looked for.
//!
//! **A scan reads what it hears.** Its listing is unfiltered, and in dense RF the OS hears
//! hundreds of devices, so it reads the properties of a device only when the adapter reports it
//! discovered or heard again: once it has a name, and at most [`NAME_READS`] times while it has
//! none, since each read is a serial D-Bus round trip. A bridge is read again until it is heard
//! advertising, because BlueZ first replays every device it knows as discovered, so a bonded
//! bridge is read before the scan has heard it. A scan ends at once only on a bridge heard
//! advertising whose whole advertised name the spec gives. Any other listens [`UNNAMED_LISTEN`]
//! and lets [`choose`] decide, so a second bridge advertising nearby is seen rather than passed
//! over, and a bonded bridge that is off is not taken while one that answers advertises. At the
//! end of that listen it reads the OS's listing once, for a bridge that raised no event: one BlueZ
//! lists as bonded and has not heard, or one that was advertising before the scan began. A bridge
//! listed and never heard is still tried when it is the only one, since one BlueZ still holds a
//! connection to does not advertise.
//!
//! **The P4 serves a central only once it owns the host queue**, which it takes on a subscription
//! to Up from a bonded, encrypted link (`wartui_proto::host_owner`). The stream starts with a
//! `Ready`, and what decodes ahead of it is dropped, as on USB (`crate::gate`).
//!
//! **`Identify` until a `Ready`, and no keepalive.** A subscription is the attachment, and the
//! bridge announces itself when it attaches one. But a bonded central's subscription outlives its
//! connection, in BlueZ and in the C6 alike, so after a link that dropped without an unsubscribe
//! the next connection is subscribed the moment it is encrypted: the bridge attached it and sent
//! its `Ready` about a second before this transport had found the service and opened its
//! notifications, and the transport then waited for a `Ready` that had gone by (D0:1C,
//! 2026-11-13). So once the notifications are open it asks, every [`IDENTIFY_EVERY`] until a
//! `Ready` answers, as the USB transport does. The P4 answers a central that holds the queue and
//! drops the frame from one that does not. The asking resumes after a USB takeover one period
//! later, never as a burst of the ticks missed while the stream was announced ([`asker`]). The
//! Bluetooth gate never closes on silence, so there is nothing to hold open. A USB host that holds
//! the queue keeps a subscribed central waiting until 15 s after its last frame, so a subscription
//! with no `Ready` after [`READY_WARN_AFTER`] logs one warning and keeps waiting.
//!
//! **A USB takeover is a link going down, not a fault.** The P4 sends the central one
//! `BridgeToHost::Error` with [`USB_TOOK_BRIDGE`] ahead of releasing it. The transport reports
//! [`LinkEvent::Disconnected`] with [`DisconnectCause::UsbTookBridge`], stays subscribed, and
//! reports `Connected` on the `Ready` that hands the queue back, though it is the same life, since
//! the stream starts afresh there.
//!
//! **Writes.** Each host frame is encoded as on USB (`serial::encode_for_wire`). Down is a byte
//! stream: the C6 forwards each write's bytes, and the P4 reassembles frames at each `0x00`. So
//! the frames queued together go as one run, a write group ([`DownWriter`]), cut into pieces of the
//! ATT MTU less its 3-byte header, and at most `ble::DOWN_VALUE_MAX`: 179 bytes at the C6's MTU of
//! 182. A longer write can reach the C6's host cut short and stop it. The C6's MTU keeps every
//! central to that size, and the transport caps its pieces too, for a bridge whose C6 serves more
//! (`wartui_proto::ble` `//!`). A write is a stream fragment, so the first frame after every
//! connect goes behind a `0x00`, which ends whatever partial frame the P4's accumulator held from
//! an earlier attachment. The byte shares that frame's first write (`serial::encode_for_wire`).
//!
//! **Each group ends with a write request**, and every piece before it is a write without
//! response. TrouBLE 0.8 queues 8 ATT PDUs per connection on the C6 and silently drops the rest,
//! and a write without response gives the central nothing to wait on: a burst of full writes lost
//! some whenever the C6's forwarding to the P4 slowed. The C6 answers the request only once it has
//! forwarded that piece, behind every piece queued ahead of it, so the central never has more than
//! one group outstanding. A group holds at most one write fewer than the C6's queue, 7 pieces,
//! which carry the largest frame and the leading `0x00`. A frame that would take a group past that
//! waits for the next group. One larger than a whole group goes alone, in more writes than a group
//! holds: that takes an ATT MTU under 168, and BlueZ asks for 517, so the link runs at the C6's
//! 182. Every write as a request held the link too, at about 3.4 KB/s on a 30 ms interval. A
//! request the C6 answers with an ATT error fails as any write does, and ends the connection. The
//! transport is done with a group's commands once its request is answered or a write fails
//! (`crate` docs), which is what paces an update's sender to the link (`crate::pace`).
//!
//! **Commands wait for the connection's `Ready`.** The commands queue is read only while the
//! stream is announced, and what it holds is dropped as the transport reports that `Ready` as
//! `Connected`, as the gate drops what decodes ahead of it. A command queued while the link was
//! down, a USB host held the bridge or the bridge had not yet answered is stale by then: written
//! at the next connection, an update's re-sends from a gap arrive as a backlog. Every Bluetooth
//! caller waits for `Connected` before its first command. USB writes what was queued, since
//! `wartui reset` writes before any `Ready`.
//!
//! **Not bonded, and maybe not bonded, are two causes.** BlueZ never starts a first pairing on its
//! own (`docs/c6-ble-pairing-findings.md`), so the transport says when the bridge will not serve
//! this host for want of a bond, and how sure it is:
//!
//! - **[`DisconnectCause::NotBonded`]** is the C6's own answer: an ATT authentication or encryption
//!   error on the subscription to Up or on a write request to Down ([`refused_for_bonding`]).
//! - **[`DisconnectCause::MaybeNotBonded`]** is timing: a link that drops before any `Ready`, 8 to
//!   35 s after connecting, which is when the C6 disconnects a central that has not bonded
//!   ([`dropped_as_unbonded`]). A bonded host waiting behind a USB host and then losing the link
//!   to a P4 reset or to range drops the same way, so `wartui bluetooth pair` acts only on the
//!   first.
//!
//! **Reconnecting.** Like `serial::supervise`: a failure that repeats is one warning, and a
//! connection that announced a bridge settles the run on that peripheral. A P4 reboot resets its
//! C6 and drops the link, which is an ordinary reconnect.
//!
//! **Closing.** Once the handle is gone the connection unsubscribes from Up and disconnects, and a
//! scan, a connect, a service discovery, the subscription or a write under way stops, so the
//! bridge never serves a process that has exited. The transport counts as running until then,
//! which `crate::close_all` waits for. A write the C6 does not answer within [`WRITE_WITHIN`], or
//! a subscription within [`CONNECT_WITHIN`], ends the connection, so neither waits out BlueZ's
//! 30 s ATT timeout.
//!
//! **Pairing is not the transport's.** [`find_advertiser`] is its scan alone, for
//! `wartui bluetooth pair`, which pairs through the OS (`crate::bluez` on Linux).

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::{Duration, Instant};

use btleplug::api::{
    BDAddr, Central as _, CentralEvent, Characteristic, Manager as _, Peripheral as _, ScanFilter,
    WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::StreamExt;
use tokio::time::{Interval, MissedTickBehavior};
use wartui_proto::ble::{ATT_MTU, DOWN_UUID, DOWN_VALUE_MAX, NAME_PREFIX, UP_UUID};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LinkError, MAX_FRAME, MAX_HOST_FRAME,
    USB_TOOK_BRIDGE, decode_frame,
};
use wartui_proto::mac::{self, Mac};

use crate::gate::{Admitted, Gate};
use crate::remember::BridgeMemory;
use crate::serial::{BridgeSpec, encode_for_wire};
use crate::{
    CommandRx, DisconnectCause, LinkEvent, LinkHandle, LinkPlumbing, TransportError,
    link_pair_awaited,
};

/// How long a scan looks for the bridge's name before giving up.
pub const SCAN_FOR: Duration = Duration::from_secs(10);

/// How long an unnamed scan listens before taking the bridges it has heard, so a second one
/// advertising nearby is seen rather than passed over.
const UNNAMED_LISTEN: Duration = Duration::from_secs(2);

/// How long a connect, a service discovery or the subscription may take. The subscription can
/// wait on the link's encryption, which the C6's refusal of an unencrypted one prompts.
const CONNECT_WITHIN: Duration = Duration::from_secs(10);

/// How long the disconnect that cancels a timed-out connect may take.
const CANCEL_WITHIN: Duration = Duration::from_secs(2);

/// How long one write to Down may take. A write request is answered within a few connection
/// intervals. The C6 holds its answer only while its queue to the P4 is full, and refuses each
/// write that waits there past a second.
const WRITE_WITHIN: Duration = Duration::from_secs(5);

/// How many times a scan reads a device that has no name before it reads it no more.
const NAME_READS: u8 = 3;

/// How long a subscription waits for its `Ready` before warning that a USB host may hold the
/// bridge. Past the P4's handback ([`HOST_SILENCE_SECS`]), so a USB host that just let go is not
/// reported.
///
/// [`HOST_SILENCE_SECS`]: wartui_proto::link::HOST_SILENCE_SECS
pub const READY_WARN_AFTER: Duration = Duration::from_secs(20);

/// How often a subscription without a `Ready` asks for one with `Identify`.
const IDENTIFY_EVERY: Duration = Duration::from_secs(1);

/// How often a connection asks the OS whether it is still connected, behind the disconnect
/// event that usually says so first.
const ALIVE_EVERY: Duration = Duration::from_secs(2);

/// The longest wait between reconnects after failures.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(10);

/// When a disconnect before any `Ready` reads as the C6 dropping an unbonded central: 10 s after
/// connecting outside its pairing window, 30 s inside it.
const UNBONDED_DROP: std::ops::RangeInclusive<Duration> =
    Duration::from_secs(8)..=Duration::from_secs(35);

/// The ATT header a write carries ahead of its value.
const ATT_HEADER: u16 = 3;

/// The smallest write value, at the default ATT MTU of 23.
const MIN_WRITE: usize = 20;

/// The ATT PDUs TrouBLE 0.8 queues per connection on the C6 (`L2CAP_RX_QUEUE_SIZE`). It drops the
/// rest silently.
const C6_WRITE_QUEUE: usize = 8;

/// The most writes one group takes: one fewer than the C6 queues.
const GROUP_WRITES: usize = C6_WRITE_QUEUE - 1;

/// The most bytes one write carries at ATT MTU `mtu`: the MTU less its header, capped at
/// [`DOWN_VALUE_MAX`] (`wartui_proto::ble`), and at least [`MIN_WRITE`].
const fn piece_for(mtu: u16) -> usize {
    let room = mtu.saturating_sub(ATT_HEADER) as usize;
    let capped = if room < DOWN_VALUE_MAX { room } else { DOWN_VALUE_MAX };
    if capped > MIN_WRITE { capped } else { MIN_WRITE }
}

// The largest frame and the first group's leading `0x00` fit one group of capped pieces at the
// MTU the C6 serves. Under an MTU of 168 a frame can take more writes than a group, up to 58 at
// the default MTU of 23, and goes alone; a central stuck there is slow anyway.
const _: () = assert!(
    (MAX_HOST_FRAME + 1).div_ceil(piece_for(ATT_MTU)) <= GROUP_WRITES,
    "the largest frame overflows a write group"
);

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
        let (handle, plumbing) = link_pair_awaited();
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

/// A bridge a scan heard: the name it advertises, and the address the OS knows it by, or on
/// macOS the identifier CoreBluetooth gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertiser {
    /// Its advertised `wartui XX:YY`.
    pub name: String,
    /// Its address, as [`crate::remember::BridgeMemory::on_bluetooth`] takes it.
    pub address: String,
}

/// Scan for up to [`SCAN_FOR`] for the bridge `spec` names, preferring one `preferred` names,
/// as a connection's scan does, and connect to nothing.
///
/// # Errors
/// [`TransportError::NoAdapter`], [`TransportError::NotFound`],
/// [`TransportError::SeveralAdvertising`], or the stack's own failure.
pub async fn find_advertiser(
    spec: Option<&BridgeSpec>,
    preferred: Option<&BridgeSpec>,
) -> Result<Advertiser, TransportError> {
    let adapter = open_adapter().await?;
    let (name, peripheral) = scan(&adapter, spec, preferred).await?;
    Ok(Advertiser { name, address: address_of(&peripheral) })
}

/// Whether `msg` is the P4's notice that a USB host took the queue from this central.
#[must_use]
pub fn is_takeover(msg: &BridgeToHost) -> bool {
    matches!(msg, BridgeToHost::Error { message } if message.as_str() == USB_TOOK_BRIDGE)
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

/// Whether a link that dropped `after` connecting, with no `Ready` heard, reads as the C6
/// dropping a central that holds no bond. `None` when the link was up before this connection
/// tried, so nothing says when the C6's deadline started, and the timing proves nothing.
#[must_use]
pub fn dropped_as_unbonded(after: Option<Duration>, announced: bool) -> bool {
    !announced && after.is_some_and(|after| UNBONDED_DROP.contains(&after))
}

/// Where a connection looks for its bridge ([`look`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    /// The remembered Bluetooth address to connect to without a scan, when the OS lists it.
    pub direct: Option<String>,
    /// The bridge a scan prefers among those it hears.
    pub preferred: Option<BridgeSpec>,
}

/// Where a connection looks for the bridge `spec` names, any without one, given the bridge
/// `remembered` with its Bluetooth `address`, and whether a connection straight to that address
/// has `failed`.
///
/// The address is tried first while it stands for a bridge `spec` names and no connection
/// straight to it has failed. After that a scan looks, preferring the bridge `spec` names, or
/// unnamed the remembered one, and taking another only in its absence.
#[must_use]
pub fn look(
    spec: Option<&BridgeSpec>,
    remembered: Option<Mac>,
    address: Option<String>,
    failed: bool,
) -> Look {
    // The remembered address stands for the remembered bridge, so only when that is the one
    // wanted.
    let wanted = spec.is_none_or(|spec| remembered.is_some_and(|mac| spec.matches_address(&mac)));
    let direct = address.filter(|_| wanted && !failed);
    let preferred = match spec {
        Some(spec) => Some(spec.clone()),
        None => remembered.map(BridgeSpec::Mac),
    };
    Look { direct, preferred }
}

/// The one bridge among those `heard`, which holds at least one, each with its advertised name
/// and whether it is advertising now: those advertising when any is, since BlueZ also lists the
/// bridges it has bonded, heard or not; then those `preferred` names when any is.
///
/// # Errors
/// [`TransportError::SeveralAdvertising`] when that leaves more than one.
pub fn choose<P>(
    mut heard: Vec<(String, bool, P)>,
    preferred: Option<&BridgeSpec>,
) -> Result<P, TransportError> {
    if heard.iter().any(|(_, live, _)| *live) {
        heard.retain(|(_, live, _)| *live);
    }
    if heard.iter().any(|(name, _, _)| names_bridge(preferred, name)) {
        heard.retain(|(name, _, _)| names_bridge(preferred, name));
    }
    if let [_] = heard.as_slice() {
        return Ok(heard.remove(0).2);
    }
    let mut names: Vec<String> = heard.into_iter().map(|(name, _, _)| name).collect();
    names.sort();
    Err(TransportError::SeveralAdvertising { names: names.join(", ") })
}

/// Whether a peripheral at `address`, with the OS identifier `id`, is the `remembered` one.
#[must_use]
pub fn is_remembered(remembered: &str, address: &str, id: &str) -> bool {
    remembered.eq_ignore_ascii_case(address) || remembered.eq_ignore_ascii_case(id)
}

/// How a write to Down goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    /// A write without response: the central waits on nothing.
    Command,
    /// A write request, which the C6 answers once it has forwarded it.
    Request,
}

/// One write to Down.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownWrite {
    /// The write's value.
    pub bytes: Vec<u8>,
    /// How it goes.
    pub kind: WriteKind,
}

/// The writes to Down that carry the host's frames, a group at a time: [`Self::offer`] frames, then
/// [`Self::take`] their writes.
#[derive(Debug)]
pub struct DownWriter {
    /// The most bytes one write carries.
    piece: usize,
    /// Whether the next frame offered is this connection's first, which goes behind a `0x00`.
    lead: bool,
    /// The frames offered since the last take, encoded end to end.
    run: Vec<u8>,
}

impl DownWriter {
    /// A writer for a connection just made, at ATT MTU `mtu`: its pieces are the MTU less its
    /// header, at most [`DOWN_VALUE_MAX`], and its first frame is encoded behind a `0x00`.
    #[must_use]
    pub fn new(mtu: u16) -> Self {
        Self { piece: piece_for(mtu), lead: true, run: Vec::new() }
    }

    /// Add `cmd`'s frame to the group, encoded as on USB and, as the connection's first, behind a
    /// `0x00`: `false` when the group is full, and the frame is not taken. A frame too large for
    /// any group is taken into an empty one.
    ///
    /// # Errors
    /// [`LinkError`] if `cmd` does not fit a frame.
    pub fn offer(&mut self, cmd: &HostToBridge) -> Result<bool, LinkError> {
        let mut buf = [0u8; MAX_HOST_FRAME + 1];
        let len = encode_for_wire(cmd, self.lead, &mut buf)?;
        let writes = (self.run.len() + len).div_ceil(self.piece);
        if !self.run.is_empty() && writes > GROUP_WRITES {
            return Ok(false);
        }
        self.run.extend_from_slice(&buf[..len]);
        self.lead = false;
        Ok(true)
    }

    /// The group's writes, and an empty group after it: the offered frames cut into writes
    /// without response, the last a request. None when nothing was offered.
    pub fn take(&mut self) -> Vec<DownWrite> {
        let run = std::mem::take(&mut self.run);
        let mut writes: Vec<DownWrite> = run
            .chunks(self.piece)
            .map(|bytes| DownWrite { bytes: bytes.to_vec(), kind: WriteKind::Command })
            .collect();
        if let Some(last) = writes.last_mut() {
            last.kind = WriteKind::Request;
        }
        writes
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
                        cause: DisconnectCause::UsbTookBridge,
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
    /// It ended, for this reason and cause, after announcing a bridge or before.
    Over { reason: String, cause: DisconnectCause, announced: bool },
}

impl End {
    fn failed(reason: impl Into<String>) -> Self {
        Self::Over { reason: reason.into(), cause: DisconnectCause::Other, announced: false }
    }
}

/// Why a connection's write or subscription failed.
struct Failure {
    reason: String,
    cause: DisconnectCause,
}

impl Failure {
    fn other(reason: String) -> Self {
        Self { reason, cause: DisconnectCause::Other }
    }
}

/// Keep a link up: find the bridge, serve the connection, report the failure, try again.
async fn supervise(transport: BleTransport, mut plumbing: LinkPlumbing) {
    let mut previous: Option<String> = None;
    let mut delay = transport.reconnect_delay;
    let mut adapter: Option<Adapter> = None;
    // The peripheral this run settled on, once it announced a bridge; never another after.
    let mut settled: Option<Peripheral> = None;
    // A connection straight to the remembered address failed, so every later try scans.
    let mut direct_failed = false;
    loop {
        let unreported = previous.is_none();
        let end = attempt(
            &transport,
            &mut plumbing,
            &mut adapter,
            (&mut settled, &mut direct_failed),
            unreported,
        );
        let (reason, cause) = match end.await {
            End::HandleDropped => return,
            End::Over { reason, cause, announced: true } => {
                tracing::warn!(reason = %reason, "Bluetooth link down");
                previous = None;
                delay = transport.reconnect_delay;
                let down = LinkEvent::Disconnected { reason, cause };
                if plumbing.events.send(down).await.is_err() {
                    return;
                }
                if !wait_unless_closed(&plumbing, delay).await {
                    return;
                }
                continue;
            }
            End::Over { reason, cause, announced: false } => (reason, cause),
        };
        if previous.as_deref() == Some(reason.as_str()) {
            tracing::debug!(reason = %reason, "Bluetooth link still down");
        } else {
            tracing::warn!(reason = %reason, retry_in = ?delay, "Bluetooth link down");
            previous = Some(reason.clone());
        }
        if plumbing.events.send(LinkEvent::Disconnected { reason, cause }).await.is_err() {
            return;
        }
        if !wait_unless_closed(&plumbing, delay).await {
            return;
        }
        delay = (delay * 2).min(MAX_RETRY_DELAY);
    }
}

/// Sleep `delay`, unless the handle goes first: `false` then.
async fn wait_unless_closed(plumbing: &LinkPlumbing, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        () = plumbing.events.closed() => false,
    }
}

/// One try: the adapter, the peripheral, then the connection. A try that connects straight to
/// the remembered address sets `direct_failed`, which only a connection that announces a bridge
/// makes moot, since it settles the run.
async fn attempt(
    transport: &BleTransport,
    plumbing: &mut LinkPlumbing,
    adapter: &mut Option<Adapter>,
    (settled, direct_failed): (&mut Option<Peripheral>, &mut bool),
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
        None => {
            let found = tokio::select! {
                found = find(&central, transport, *direct_failed) => found,
                () = plumbing.events.closed() => {
                    let _ = central.stop_scan().await;
                    return End::HandleDropped;
                }
            };
            match found {
                Ok((peripheral, direct)) => {
                    *direct_failed |= direct;
                    peripheral
                }
                Err(e) => return End::failed(e.to_string()),
            }
        }
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

/// The peripheral to connect to, by [`look`]: the remembered one when the OS lists it and no
/// connection straight to it has failed, else the one a scan hears advertising the name. `true`
/// beside it when it is the remembered one, reached without a scan.
async fn find(
    adapter: &Adapter,
    transport: &BleTransport,
    direct_failed: bool,
) -> Result<(Peripheral, bool), TransportError> {
    let spec = transport.spec.as_ref();
    let Look { direct, preferred } =
        look(spec, transport.memory.recall(), transport.memory.recall_bluetooth(), direct_failed);
    if let Some(remembered) = direct {
        let listed = adapter.peripherals().await.map_err(bluetooth)?;
        if let Some(known) = listed
            .into_iter()
            .find(|p| is_remembered(&remembered, &p.address().to_string(), &p.id().to_string()))
        {
            return Ok((known, true));
        }
    }
    Ok((scan(adapter, spec, preferred.as_ref()).await?.1, false))
}

/// The bridges one scan has heard, by the OS's identifier `I` for each peripheral `P`, with their
/// names and whether each has been heard advertising.
struct Hearing<'a, I, P> {
    spec: Option<&'a BridgeSpec>,
    bridges: Vec<(I, String, bool, P)>,
    /// Peripherals not read again: named and not a bridge, or unnamed after [`NAME_READS`].
    done: HashSet<I>,
    /// How many times each peripheral still unnamed has been read.
    unnamed: HashMap<I, u8>,
}

impl<'a, I: Clone + Eq + Hash, P> Hearing<'a, I, P> {
    fn new(spec: Option<&'a BridgeSpec>) -> Self {
        Self { spec, bridges: Vec::new(), done: HashSet::new(), unnamed: HashMap::new() }
    }

    /// Whether the scan can end before [`UNNAMED_LISTEN`]: the spec gives a whole advertised name,
    /// so it names one bridge, and that bridge has been heard advertising.
    fn settled(&self) -> bool {
        self.spec.and_then(advertised_name).is_some()
            && self.bridges.iter().any(|(_, _, live, _)| *live)
    }

    /// Whether a hearing of `id` is worth reading its properties for: any device not done with,
    /// except a bridge already heard advertising.
    fn wants(&self, id: &I) -> bool {
        match self.bridges.iter().find(|(heard, ..)| heard == id) {
            Some((_, _, live, _)) => !live,
            None => !self.done.contains(id),
        }
    }

    /// Take what a read of `peripheral` found: its name and whether it is advertising, if it has
    /// a name. A bridge `spec` names is kept, and one already kept is marked advertising once a
    /// read says so.
    fn noted(&mut self, id: I, peripheral: P, read: Option<(String, bool)>) {
        if let Some((_, _, live, _)) = self.bridges.iter_mut().find(|(heard, ..)| *heard == id) {
            *live |= read.is_some_and(|(_, now)| now);
            return;
        }
        let Some((name, live)) = read else {
            let reads = self.unnamed.entry(id.clone()).or_default();
            *reads += 1;
            if *reads >= NAME_READS {
                self.unnamed.remove(&id);
                self.done.insert(id);
            }
            return;
        };
        self.unnamed.remove(&id);
        if names_bridge(self.spec, &name) {
            self.bridges.push((id, name, live, peripheral));
        } else {
            self.done.insert(id);
        }
    }

    /// The bridges heard, as [`choose`] takes them.
    fn into_bridges(self) -> Vec<(String, bool, P)> {
        self.bridges
            .into_iter()
            .map(|(_, name, live, peripheral)| (name, live, peripheral))
            .collect()
    }
}

/// Scan for up to [`SCAN_FOR`] for a bridge `spec` names, preferring one `preferred` names, and
/// return its advertised name beside it.
async fn scan(
    adapter: &Adapter,
    spec: Option<&BridgeSpec>,
    preferred: Option<&BridgeSpec>,
) -> Result<(String, Peripheral), TransportError> {
    // Opened before the scan starts, so nothing it hears goes by unreported.
    let mut events = adapter.events().await.map_err(bluetooth)?;
    adapter.start_scan(ScanFilter::default()).await.map_err(bluetooth)?;
    let heard = listen(adapter, &mut events, spec).await;
    let _ = adapter.stop_scan().await;
    let heard = heard?.into_iter().map(|(name, live, p)| (name.clone(), live, (name, p)));
    choose(heard.collect(), preferred)
}

/// Read `peripheral`'s name, when `hearing` wants it, and note what the read found.
async fn note(hearing: &mut Hearing<'_, PeripheralId, Peripheral>, peripheral: Peripheral) {
    let id = peripheral.id();
    if hearing.wants(&id) {
        let read = name_of(&peripheral).await;
        hearing.noted(id, peripheral, read);
    }
}

/// Read the adapter's events until a bridge `spec` names is heard: at once when the spec gives its
/// whole advertised name and it is heard advertising ([`Hearing::settled`]), else once
/// [`UNNAMED_LISTEN`] has passed. At that point the OS's listing is read once, for a bridge that
/// raised no event.
async fn listen(
    adapter: &Adapter,
    events: &mut Events,
    spec: Option<&BridgeSpec>,
) -> Result<Vec<(String, bool, Peripheral)>, TransportError> {
    let started = tokio::time::Instant::now();
    let (listened, give_up) = (started + UNNAMED_LISTEN, started + SCAN_FOR);
    let mut hearing = Hearing::new(spec);
    let mut listed = false;
    loop {
        if hearing.settled() {
            break;
        }
        if !listed && tokio::time::Instant::now() >= listened {
            listed = true;
            for peripheral in adapter.peripherals().await.map_err(bluetooth)? {
                note(&mut hearing, peripheral).await;
            }
        }
        if listed && !hearing.bridges.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= give_up {
            return Err(TransportError::NotFound {
                name: spec
                    .and_then(advertised_name)
                    .unwrap_or_else(|| format!("{NAME_PREFIX}XX:YY")),
                secs: SCAN_FOR.as_secs(),
            });
        }
        let wake = if listed { give_up } else { listened };
        tokio::select! {
            event = events.next() => match event {
                // BlueZ reports a device it already knew, heard again, as an RSSI change.
                Some(
                    CentralEvent::DeviceDiscovered(id)
                    | CentralEvent::DeviceUpdated(id)
                    | CentralEvent::RssiUpdate { id, .. },
                ) => {
                    if hearing.wants(&id)
                        && let Ok(peripheral) = adapter.peripheral(&id).await
                    {
                        note(&mut hearing, peripheral).await;
                    }
                }
                Some(_) => {}
                None => {
                    return Err(TransportError::Bluetooth(
                        "the Bluetooth adapter stopped reporting".to_owned(),
                    ));
                }
            },
            () = tokio::time::sleep_until(wake) => {}
        }
    }
    Ok(hearing.into_bridges())
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
    progress!(bridge = %label, "connecting to the bridge over Bluetooth");

    // When this try made the link, since the C6's unbonded deadline runs from the link coming up.
    let mut connected_at = None;
    if !peripheral.is_connected().await.unwrap_or(false) {
        let connecting = tokio::select! {
            connecting = tokio::time::timeout(CONNECT_WITHIN, peripheral.connect()) => connecting,
            () = plumbing.events.closed() => {
                let _ = peripheral.disconnect().await;
                return End::HandleDropped;
            }
        };
        match connecting {
            Ok(Ok(())) => connected_at = Some(Instant::now()),
            Ok(Err(e)) => return End::failed(format!("could not connect to {label}: {e}")),
            Err(_) => {
                // Unlike a link that dropped, this one never came up: BlueZ's `Connect` is still
                // pending, and could complete after this process has gone. `Disconnect` cancels
                // it, and BlueZ arms its 2 s disconnect timer only for a connected device, so no
                // later connection inherits it.
                let _ = tokio::time::timeout(CANCEL_WITHIN, peripheral.disconnect()).await;
                return End::failed(format!(
                    "could not connect to {label} in {}s; check the bridge is powered and in range",
                    CONNECT_WITHIN.as_secs()
                ));
            }
        }
    }
    let end = serve(adapter, peripheral, transport, plumbing, settled, &label).await;
    let end = match end {
        // The C6 drops an unbonded central about 10 s after it connects, and the timing is all
        // that says so.
        End::Over { announced: false, cause: DisconnectCause::Other, .. }
            if dropped_as_unbonded(connected_at.map(|at| at.elapsed()), false)
                && !peripheral.is_connected().await.unwrap_or(false) =>
        {
            End::Over {
                reason: TransportError::MaybeNotBonded { bridge: label.clone() }.to_string(),
                cause: DisconnectCause::MaybeNotBonded,
                announced: false,
            }
        }
        end => end,
    };
    // A link that already dropped is not disconnected again: BlueZ runs the request on its 2 s
    // timer, where it lands on the connection the next try makes, and ends it.
    if peripheral.is_connected().await.unwrap_or(false) {
        let _ = peripheral.disconnect().await;
    }
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
    let discovered = discover(peripheral.discover_services(), label);
    if let Some(end) = unless_closed(discovered, plumbing, false).await {
        return end;
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
    let subscribed = subscribe(peripheral.subscribe(&up), label);
    if let Some(end) = unless_closed(subscribed, plumbing, false).await {
        return end;
    }
    let mtu = peripheral.mtu();
    tracing::info!(bridge = %label, mtu, "subscribed to the bridge over Bluetooth");

    let end = pump(
        peripheral,
        (transport, label),
        plumbing,
        settled,
        (&up, &down),
        (&mut notes, &mut events),
    );
    let end = end.await;
    if matches!(end, End::HandleDropped) {
        let _ = peripheral.unsubscribe(&up).await;
    }
    end
}

/// Wait for `discovering`, the service discovery, for up to [`CONNECT_WITHIN`].
async fn discover(
    discovering: impl Future<Output = btleplug::Result<()>>,
    label: &str,
) -> Result<(), Failure> {
    match tokio::time::timeout(CONNECT_WITHIN, discovering).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(Failure::other(format!("could not read {label}'s services: {e}"))),
        Err(_) => Err(Failure::other(format!("{label} did not list its services"))),
    }
}

/// Wait for `subscribing`, the subscription to Up, for up to [`CONNECT_WITHIN`]. The C6's refusal
/// for want of a bond is [`refusal`]'s.
async fn subscribe(
    subscribing: impl Future<Output = btleplug::Result<()>>,
    label: &str,
) -> Result<(), Failure> {
    match tokio::time::timeout(CONNECT_WITHIN, subscribing).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(refusal(label, &e)
            .unwrap_or_else(|| Failure::other(format!("could not subscribe to {label}: {e}")))),
        Err(_) => Err(Failure::other(format!("{label} did not answer the subscription"))),
    }
}

/// The notifications stream, and the adapter's events.
type Notes =
    std::pin::Pin<Box<dyn futures_util::Stream<Item = btleplug::api::ValueNotification> + Send>>;
type Events = std::pin::Pin<Box<dyn futures_util::Stream<Item = CentralEvent> + Send>>;

/// The timer that asks for a `Ready` with `Identify`. Its first tick is at once.
///
/// Its arm is off while the stream is announced, which can be hours, and a takeover turns it back
/// on. Tokio's default `Burst` would then fire every tick missed meanwhile back to back, each a
/// write request ahead of the engine's commands, so a late tick fires once and the next is a
/// period after it. `pump` also resets it when the gate restarts, so the first `Identify` after
/// a takeover goes out a period later.
fn asker() -> Interval {
    let mut ask = tokio::time::interval(IDENTIFY_EVERY);
    ask.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ask
}

/// Pump a subscribed connection: notifications up, commands down, until either side ends.
async fn pump(
    peripheral: &Peripheral,
    (transport, label): (&BleTransport, &str),
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
    // Its first tick is immediate: the notifications are open, so the `Ready` it brings is heard.
    let mut ask = asker();
    let ready_warn = tokio::time::sleep(READY_WARN_AFTER);
    tokio::pin!(ready_warn);
    let mut warned = false;
    let mut ever_announced = false;
    let failed = |Failure { reason, cause }, announced| End::Over { reason, cause, announced };
    let over = |reason: &str, announced| failed(Failure::other(reason.to_owned()), announced);

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
                            if matches!(event, LinkEvent::Connected(_)) {
                                // Queued before this `Ready`, so stale (§ "Commands wait").
                                let dropped = plumbing.commands.discard();
                                if dropped > 0 {
                                    tracing::debug!(dropped, "dropped commands queued before Ready");
                                }
                            }
                            if let LinkEvent::Disconnected { reason, .. } = &event {
                                tracing::warn!(reason = %reason, "the bridge let this host go");
                                // The handback is expected, and is no reason to warn again.
                                warned = true;
                                // The gate restarted: ask again a period from now.
                                ask.reset();
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
            _ = ask.tick(), if !inbound.announced() => {
                if writer.offer(&HostToBridge::Identify).is_err() {
                    tracing::error!("Identify did not fit in a frame");
                }
                let sent = send(peripheral, down, writer.take(), label);
                if let Some(end) = unless_closed(sent, plumbing, ever_announced).await {
                    return end;
                }
            }
            cmd = plumbing.commands.recv(), if inbound.announced() => {
                let Some(cmd) = cmd else { return End::HandleDropped };
                let taken = group(&mut writer, cmd, &mut plumbing.commands);
                let sent = send(peripheral, down, writer.take(), label);
                let end = unless_closed(sent, plumbing, ever_announced).await;
                // Written, or lost with the connection: done either way.
                plumbing.commands.done(taken);
                if let Some(end) = end {
                    return end;
                }
            }
            // The commands arm is off until the `Ready`, so it cannot say the handle is gone.
            () = plumbing.events.closed() => return End::HandleDropped,
        }
    }
}

/// Wait for `sent` unless the handle goes first: the connection's end when either ends it, after
/// announcing a bridge or before.
async fn unless_closed(
    sent: impl Future<Output = Result<(), Failure>>,
    plumbing: &LinkPlumbing,
    announced: bool,
) -> Option<End> {
    let sent = tokio::select! {
        sent = sent => sent,
        () = plumbing.events.closed() => return Some(End::HandleDropped),
    };
    sent.err().map(|Failure { reason, cause }| End::Over { reason, cause, announced })
}

/// Offer `first`, then every command already queued, to `writer`'s group until it refuses one,
/// which is held for the next group. Returns how many commands the group took, counting one
/// dropped for not fitting a frame.
fn group(writer: &mut DownWriter, first: HostToBridge, commands: &mut CommandRx) -> u64 {
    let mut taken = 0;
    let mut next = Some(first);
    while let Some(cmd) = next {
        match writer.offer(&cmd) {
            Ok(true) => {}
            Ok(false) => {
                commands.hold(cmd);
                break;
            }
            // Only reachable if a command outgrew MAX_HOST_FRAME.
            Err(_) => tracing::error!("command did not fit in a frame; dropping it"),
        }
        taken += 1;
        next = commands.try_recv();
    }
    taken
}

/// Write one group to Down, a piece at a time, each within [`WRITE_WITHIN`].
async fn send(
    peripheral: &Peripheral,
    down: &Characteristic,
    writes: Vec<DownWrite>,
    label: &str,
) -> Result<(), Failure> {
    for write in writes {
        let kind = match write.kind {
            WriteKind::Command => WriteType::WithoutResponse,
            WriteKind::Request => WriteType::WithResponse,
        };
        let written =
            tokio::time::timeout(WRITE_WITHIN, peripheral.write(down, &write.bytes, kind));
        match written.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                // Only a request carries the C6's answer; a write without response fails locally.
                let refused =
                    (write.kind == WriteKind::Request).then(|| refusal(label, &e)).flatten();
                return Err(refused.unwrap_or_else(|| Failure::other(format!("write failed: {e}"))));
            }
            Err(_) => {
                return Err(Failure::other(format!(
                    "a write to the bridge was not answered in {}s",
                    WRITE_WITHIN.as_secs()
                )));
            }
        }
    }
    Ok(())
}

/// The C6's refusal of this host for want of a bond, when `error` is one ([`refused_for_bonding`]).
fn refusal(label: &str, error: &btleplug::Error) -> Option<Failure> {
    refused_for_bonding(&error.to_string()).then(|| Failure {
        reason: TransportError::NotBonded { bridge: label.to_owned() }.to_string(),
        cause: DisconnectCause::NotBonded,
    })
}

#[cfg(test)]
mod tests {
    use tokio::time::Instant;

    use super::{
        BridgeSpec, CONNECT_WITHIN, End, Hearing, IDENTIFY_EVERY, NAME_READS, asker, choose,
        subscribe, unless_closed,
    };
    use crate::link_pair;

    fn spec(text: &str) -> BridgeSpec {
        text.parse().unwrap_or_else(|never| match never {})
    }

    const BRIDGE: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

    /// A read that found `name`, advertising now or not.
    fn read(name: &str, live: bool) -> Option<(String, bool)> {
        Some((name.to_owned(), live))
    }

    #[test]
    fn ble_hearing_marks_bridge_live_when_heard_again_after_a_listed_read() {
        // BlueZ replays a bonded bridge as discovered before the scan has heard it.
        let mut hearing = Hearing::new(None);
        hearing.noted(1, "remembered", read("wartui 44:C0", false));
        hearing.noted(2, "other", read("wartui A1:B0", true));
        assert!(hearing.wants(&1), "a bridge not heard advertising is read again");
        hearing.noted(1, "remembered", read("wartui 44:C0", true));
        assert!(!hearing.wants(&1), "nor once it has been");
        let chosen = choose(hearing.into_bridges(), Some(&BridgeSpec::Mac(BRIDGE)));
        assert_eq!(chosen.expect("one bridge"), "remembered");
    }

    #[test]
    fn ble_hearing_reads_device_no_more_when_named_or_unnamed_past_the_cap() {
        let mut hearing: Hearing<'_, u8, ()> = Hearing::new(None);
        hearing.noted(1, (), read("Pixel 8", true));
        assert!(!hearing.wants(&1), "named and not a bridge");
        for _ in 1..NAME_READS {
            hearing.noted(2, (), None);
            assert!(hearing.wants(&2));
        }
        hearing.noted(2, (), None);
        assert!(!hearing.wants(&2), "unnamed after {NAME_READS} reads");
    }

    #[test]
    fn ble_hearing_keeps_bridge_when_it_is_named_on_a_later_read() {
        let mut hearing = Hearing::new(None);
        hearing.noted(1, "late", None);
        hearing.noted(1, "late", read("wartui 44:C0", true));
        assert_eq!(hearing.into_bridges(), [("wartui 44:C0".to_owned(), true, "late")]);
    }

    #[test]
    fn ble_hearing_settles_when_bridge_named_in_full_is_heard_advertising() {
        let named = spec("44:C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read("wartui 44:C0", true));
        assert!(hearing.settled());
    }

    #[test]
    fn ble_hearing_waits_for_listing_when_bridge_named_in_full_is_only_listed() {
        // BlueZ replays a bonded bridge that is off as discovered, with no RSSI.
        let named = spec("02:00:5E:10:44:C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read("wartui 44:C0", false));
        assert!(!hearing.settled());
    }

    #[test]
    fn ble_hearing_waits_for_listing_when_partial_name_matches_one_live_bridge() {
        // `C0` names every bridge ending in it, so `choose` has to see them all.
        let named = spec("C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read("wartui 44:C0", true));
        assert!(!hearing.settled());
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_fails_when_bridge_does_not_answer_in_time() {
        let start = Instant::now();
        let failed = subscribe(std::future::pending(), "wartui 44:C0").await.err();
        let failure = failed.expect("an unanswered subscription fails");
        assert_eq!(failure.reason, "wartui 44:C0 did not answer the subscription");
        assert_eq!(start.elapsed(), CONNECT_WITHIN);
    }

    #[tokio::test(start_paused = true)]
    async fn ble_unless_closed_ends_connection_when_handle_goes_during_subscription() {
        // A quit while the C6 has yet to answer: the connection lets the bridge go at once.
        let (handle, plumbing) = link_pair();
        let start = Instant::now();
        let subscribing = subscribe(std::future::pending(), "wartui 44:C0");
        drop(handle);
        let end = unless_closed(subscribing, &plumbing, false).await;
        assert!(matches!(end, Some(End::HandleDropped)));
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
    }

    /// An hour with the arm off, as a stream announced that long leaves it.
    const ANNOUNCED_FOR: std::time::Duration = std::time::Duration::from_secs(3600);

    #[tokio::test(start_paused = true)]
    async fn ble_asker_fires_once_then_a_period_later_when_ticks_were_missed() {
        let mut ask = asker();
        ask.tick().await;
        tokio::time::advance(ANNOUNCED_FOR).await;
        let start = Instant::now();
        ask.tick().await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO, "the one late tick");
        ask.tick().await;
        assert_eq!(start.elapsed(), IDENTIFY_EVERY, "not a burst of the missed ones");
    }

    #[tokio::test(start_paused = true)]
    async fn ble_asker_waits_a_period_when_reset_after_ticks_were_missed() {
        // What `pump` does when a takeover restarts the gate.
        let mut ask = asker();
        ask.tick().await;
        tokio::time::advance(ANNOUNCED_FOR).await;
        ask.reset();
        let start = Instant::now();
        ask.tick().await;
        assert_eq!(start.elapsed(), IDENTIFY_EVERY);
    }
}
