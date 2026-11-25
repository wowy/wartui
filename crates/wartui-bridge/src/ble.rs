//! Talking to a real bridge over Bluetooth, through its C6's GATT service
//! (`wartui_proto::ble`), with `btleplug` on the tokio runtime the crate already runs.
//!
//! **Finding the bridge.** The C6 advertises the bridge's id: service data under
//! `wartui_proto::ble::ID_UUID` that carries its last two octets
//! (`wartui_proto::ble::advertised_octets`). A scan matches the id, for at most [`SCAN_FOR`]. The
//! advertised name is a label for a person, which the OS shows and the C6 may word as it likes,
//! so nothing matches it. The id carries two octets, so the first `Ready` is checked against the
//! whole spec, and a bridge that only shares the octets is refused. The rule is the serial
//! transport's ([`look`]):
//!
//! - **A bridge named with `--bridge` is the only one taken**, remembered or not.
//! - **A remembered bridge is tried first**, as the serial sweep opens its CH343 first: its
//!   Bluetooth address is connected to without a scan when it stands for a bridge the spec names
//!   and the OS lists it. BlueZ lists every bonded device, and CoreBluetooth only what it has
//!   seen, so macOS scans instead.
//! - **Once a connection straight to it has failed, every later try scans**, as the sweep moves
//!   on to the other CH343s. The scan still prefers the remembered bridge while it advertises, so
//!   one that is off or out of range blocks no other bridge, and one that answers is taken back.
//! - **A connection that announced a bridge settles the run on that bridge's address**, and no
//!   other is looked for. Every later try reconnects to its peripheral, and the first `Ready` is
//!   checked against the settled address, which already matched the spec ([`settled_spec`]).
//!
//! **Each try takes a fresh handle on macOS.** btleplug's CoreBluetooth backend drops a
//! peripheral from its own table on every disconnect, and keeps it in the adapter's. So
//! `peripheral(&id)`, `peripherals()` and a rescan all hand back a handle whose connect fails or
//! whose notifications go nowhere (`docs/ble-host-link-findings.md` § "macOS"). Asking for the
//! peripheral by its identifier (`retrieve_peripherals`) builds a live handle for one a disconnect
//! dropped, as long as no rescan has re-added it to the backend's table, so a settled try asks
//! that way first ([`fresh`]), and a try that scans clears both of btleplug's tables before it
//! does. BlueZ keeps its device object across a disconnect, so Linux needs neither.
//!
//! **A scan reads what it hears.** Its listing is unfiltered, and in dense RF the OS hears
//! hundreds of devices, so it reads the properties of a device only when the adapter reports it
//! discovered or heard again, at most [`ID_READS`] times while they carry no id, since each read
//! is a serial D-Bus round trip. A device left alone that way is read again whenever the adapter
//! reports service data under the id from it, since that is the bridge being heard: BlueZ keeps
//! no service data across its own restart, so the id can come late. A bridge is read again until it
//! is heard advertising, because BlueZ first replays every device it knows as discovered, so a
//! bonded bridge is read before the scan has heard it. A scan ends at once only on a bridge heard
//! advertising whose two octets the spec gives. Any other listens [`LISTEN_FOR`] and lets
//! [`choose`] decide, so a second bridge advertising nearby is seen rather than passed over, and a
//! bonded bridge that is off is not taken while one that answers advertises. At the end of that
//! listen it reads the OS's listing once, for a bridge that raised no event: one BlueZ lists as
//! bonded and has not heard, or one that was advertising before the scan began. A bridge listed
//! and never heard is still tried when it is the only one, since one BlueZ still holds a
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
//! **Writes.** Each host frame is encoded as on USB (`wire::encode_for_wire`). Down is a byte
//! stream: the C6 forwards each write's bytes, and the P4 reassembles frames at each `0x00`. So
//! the frames queued together go as one run, a write group ([`DownWriter`]), cut into pieces of the
//! ATT MTU less its 3-byte header, and at most `ble::DOWN_VALUE_MAX`: 179 bytes at the C6's MTU of
//! 182. A longer write can reach the C6's host cut short and stop it. The C6's MTU keeps every
//! central to that size, and the transport caps its pieces too, for a bridge whose C6 serves more
//! (`wartui_proto::ble` `//!`). A write is a stream fragment, so the first frame after every
//! connect goes behind a `0x00`, which ends whatever partial frame the P4's accumulator held from
//! an earlier attachment. The byte shares that frame's first write (`wire::encode_for_wire`).
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
//! **A bond only this host holds is a timeout on macOS.** CoreBluetooth's connect never
//! completes and reports no failure, so the transport's timeout at [`CONNECT_WITHIN`] is the only
//! sign, though the C6 sees links of about half a second meanwhile (D0:1C, 2026-11-20,
//! `docs/ble-host-link-findings.md` § "A bond only the Mac holds"). A try whose scan heard the
//! bridge advertising has shown it powered and near, so on macOS that timeout's text gives range
//! first and then the stale bond's fix, short enough for the view's fault box at 80 columns
//! ([`connect_timeout_text`]). The bond is an "if" there: a weak signal times out the same way.
//! A bridge another central holds advertises no wartui set, so it reads as not advertising. A Mac
//! that forgets a bridge still holding its bond cannot pair again until the window opens, which
//! is why range goes first. The cause stays [`DisconnectCause::Other`], so a caller keeps
//! retrying. A try that makes no scan, a settled one or one straight to the remembered address,
//! keeps the power-and-range wording. On Linux BlueZ connects in this state and the bridge drops
//! it, which `wartui`'s silence notice names, so Linux keeps the power-and-range wording too.
//!
//! **Waiting on the OS's pairing.** Off Linux the OS pairs on the first connection inside the
//! bridge's pairing window, behind a dialog of its own (Connect, on macOS), and the subscription
//! waits on the operator's answer. So it is given [`PAIR_WITHIN`], the C6's deadline for a
//! central that connects inside the window, and one still unanswered [`PAIRING_NOTICE_AFTER`] in
//! is announced once as [`LinkEvent::Disconnected`] with [`DisconnectCause::AwaitingPairing`].
//! The event is what reaches the operator: transport logs reach only `--log-file`. A caller keeps
//! that cause until another event, and a USB host that holds the queue keeps the `Ready` back
//! until 15 s after its last frame. So a subscription that announced the wait and then succeeds
//! is announced once more, as `Disconnected` with [`DisconnectCause::AwaitingReady`]. The host's
//! deadline starts after the C6's, so a dialog left unanswered ends with the C6's drop.
//! CoreBluetooth then fails the subscription with an ATT authentication or encryption error,
//! which reads as [`DisconnectCause::NotBonded`] (D0:1C, 2026-11-20). A run that has settled
//! heard a `Ready` from this bridge, so this host is bonded, and its subscription keeps
//! [`CONNECT_WITHIN`] and sends no notice ([`awaits_pairing`]).
//!
//! **Reconnecting.** Like `serial::supervise`: a failure that repeats is one warning, and a
//! connection that announced a bridge settles the run on that bridge. A P4 reboot resets its C6
//! and drops the link, which is an ordinary reconnect.
//!
//! **Closing.** Once the handle is gone the connection unsubscribes from Up and disconnects, and a
//! scan, a connect, a service discovery, the subscription or a write under way stops, so the
//! bridge never serves a process that has exited. The transport counts as running until then,
//! which `crate::close_all` waits for. A write the C6 does not answer within [`WRITE_WITHIN`], or
//! a subscription within [`CONNECT_WITHIN`], ends the connection, so neither waits out BlueZ's
//! 30 s ATT timeout. Where the OS pairs on the connection the subscription has [`PAIR_WITHIN`]
//! (§ "Waiting on the OS's pairing").
//!
//! **Pairing is not the transport's.** [`find_advertiser`] is its scan alone, for
//! `wartui bluetooth pair`, which pairs through the OS (`crate::bluez` on Linux).

use std::collections::{HashMap, HashSet};
use std::hash::Hash;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use btleplug::api::RetrievePeripheralsOptions;
use btleplug::api::bleuuid::uuid_from_u16;
use btleplug::api::{
    BDAddr, Central as _, CentralEvent, Characteristic, Manager as _, Peripheral as _,
    PeripheralProperties, ScanFilter, WriteType,
};
use btleplug::platform::{Adapter, Manager, Peripheral, PeripheralId};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio::time::{Interval, MissedTickBehavior};
use wartui_proto::ble::{ATT_MTU, DOWN_UUID, DOWN_VALUE_MAX, ID_UUID, UP_UUID, advertised_octets};
use wartui_proto::link::{
    BridgeToHost, FrameAccumulator, HostToBridge, LinkError, MAX_FRAME, MAX_HOST_FRAME,
    USB_TOOK_BRIDGE, decode_frame,
};
use wartui_proto::mac::{self, Mac};

use crate::gate::{Admitted, Gate};
use crate::remember::BridgeMemory;
use crate::spec::BridgeSpec;
use crate::wire::encode_for_wire;
use crate::{
    CommandRx, DisconnectCause, LinkEvent, LinkHandle, LinkPlumbing, TransportError,
    link_pair_awaited,
};

/// How long a scan looks for the bridge's id before giving up.
///
/// BlueZ restarts its discovery, and with it the controller's duplicate filter, every 10.24 s, so
/// a bridge whose advertisement the first cycle missed is reported only in the second, and a scan
/// reaches into that second cycle. The wartui set is off the air while a central holds the
/// bridge's one connection, as a phone that knocks does for 3 s at a time. On D0:1C beside such a
/// phone, 12 scans of 12 heard the id within [`LISTEN_FOR`] (`docs/wardrive-esp-findings.md`).
pub const SCAN_FOR: Duration = Duration::from_secs(12);

/// How long a scan listens before taking the bridges it has heard, unless the spec gives both of
/// one bridge's octets, so a second one advertising nearby is seen rather than passed over.
const LISTEN_FOR: Duration = Duration::from_secs(2);

/// How long a connect, a service discovery or, where the OS pairs beforehand, the subscription
/// may take. The subscription can wait on the link's encryption, which the C6's refusal of an
/// unencrypted one prompts.
const CONNECT_WITHIN: Duration = Duration::from_secs(10);

/// How long the subscription may take where the OS pairs on the connection, behind its own
/// dialog. It mirrors the C6's `PAIR_DEADLINE` (`firmware/p4-radio/src/ble.rs`), how long the C6
/// gives a central that connects inside the pairing window to bond.
pub const PAIR_WITHIN: Duration = Duration::from_secs(30);

/// How long the subscription waits before it announces the wait on the OS's pairing dialog. A
/// bonded Mac subscribes about 1 s after connecting, so a resumed bond never reaches it.
const PAIRING_NOTICE_AFTER: Duration = Duration::from_secs(3);

/// Whether the OS pairs on the first connection, behind a dialog of its own. BlueZ pairs
/// beforehand, through `wartui bluetooth pair`.
const OS_PAIRS_ON_CONNECT: bool = !cfg!(target_os = "linux");

/// How long the disconnect that cancels a timed-out connect may take.
const CANCEL_WITHIN: Duration = Duration::from_secs(2);

/// How long one write to Down may take. A write request is answered within a few connection
/// intervals. The C6 holds its answer only while its queue to the P4 is full, and refuses each
/// write that waits there past a second.
const WRITE_WITHIN: Duration = Duration::from_secs(5);

/// How many times a scan reads a device that carries no id before it leaves it alone.
const ID_READS: u8 = 3;

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

/// Whether a subscription waits on the OS's pairing dialog: where the OS pairs on the connection,
/// and only until the run has `settled`, since a bridge that answered a `Ready` holds this
/// host's bond.
const fn awaits_pairing(os_pairs: bool, settled: bool) -> bool {
    os_pairs && !settled
}

/// How much longer than [`SCAN_FOR`] and the connect a first `Ready` may take where the OS
/// `pairs` on the connection: the subscription's wait before its notice.
const fn notice_allowance(pairs: bool) -> Duration {
    if pairs { PAIRING_NOTICE_AFTER } else { Duration::ZERO }
}

/// How much a command's first wait over Bluetooth needs beyond the scan and the connect to hear
/// the pairing notice on this OS: [`PAIRING_NOTICE_AFTER`] where the OS pairs on the connection,
/// else nothing.
#[must_use]
pub const fn pairing_notice_allowance() -> Duration {
    notice_allowance(OS_PAIRS_ON_CONNECT)
}

/// How long the subscription may take: [`PAIR_WITHIN`] when the OS `pairs` on the connection,
/// else [`CONNECT_WITHIN`].
const fn subscribe_within(pairs: bool) -> Duration {
    if pairs { PAIR_WITHIN } else { CONNECT_WITHIN }
}

/// The operator's notice that the subscription waits on the OS's pairing dialog for `label`,
/// worded for macOS when `macos`.
fn pairing_notice(label: &str, macos: bool) -> String {
    if macos {
        format!("pairing: click Connect if macOS shows a dialog for {label}")
    } else {
        format!("pairing: accept the dialog if the OS shows one for {label}")
    }
}

/// Why a connect to `label` timed out, for the operator. A connect that never completes is how
/// macOS meets a bond only this host holds, and a scan that `heard` the bridge advertising has
/// shown it powered and near, so on `macos` that pair gives range first and then the stale
/// bond's fix, briefly enough for the view's fault box (§ "A bond only this host holds is a
/// timeout on macOS").
fn connect_timeout_text(label: &str, heard: bool, macos: bool) -> String {
    let secs = CONNECT_WITHIN.as_secs();
    if heard && macos {
        format!(
            "could not connect to {label} in {secs}s, though it advertises. Come closer, or \
             forget it in System Settings → Bluetooth if the bridge dropped this Mac's bond, then \
             pair again."
        )
    } else {
        format!("could not connect to {label} in {secs}s; check the bridge is powered and in range")
    }
}

/// The operator's notice that the subscription that waited on the OS's pairing for `label` is
/// through, and the bridge's `Ready` has not come.
fn ready_notice(label: &str) -> String {
    format!("waiting for {label} to answer")
}

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

/// The two octets the bridge `spec` names advertises, when the spec gives both.
#[must_use]
pub fn named_octets(spec: &BridgeSpec) -> Option<[u8; 2]> {
    let tail = match spec {
        BridgeSpec::Mac(mac) => &mac[4..],
        BridgeSpec::Tail(tail) if tail.len() >= 2 => &tail[tail.len() - 2..],
        BridgeSpec::Tail(_) | BridgeSpec::Path(_) => return None,
    };
    Some([tail[0], tail[1]])
}

/// Whether a bridge advertising `octets` is one `spec` could name: any bridge without a spec.
#[must_use]
pub fn matches_octets(spec: Option<&BridgeSpec>, octets: [u8; 2]) -> bool {
    match spec {
        None => true,
        Some(BridgeSpec::Path(_)) => false,
        Some(BridgeSpec::Mac(mac)) => mac[4..] == octets,
        Some(BridgeSpec::Tail(wanted)) => {
            let n = wanted.len().min(2);
            wanted[wanted.len() - n..] == octets[2 - n..]
        }
    }
}

/// A bridge a scan heard: the octets it advertises, its name, and the address the OS knows it
/// by, or on macOS the identifier CoreBluetooth gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advertiser {
    /// The last two octets its id carries.
    pub octets: [u8; 2],
    /// The name it advertises, when the OS heard one: a label, never matched.
    pub name: Option<String>,
    /// Its address, as [`crate::remember::BridgeMemory::on_bluetooth`] takes it.
    pub address: String,
}

impl Advertiser {
    /// The bridge, for messages: its advertised name, else `bridge XX:YY`.
    #[must_use]
    pub fn label(&self) -> String {
        self.name.clone().unwrap_or_else(|| format!("bridge {}", mac::octets(&self.octets)))
    }
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
    let (octets, name, peripheral, _) = scan(&adapter, spec, preferred).await?;
    Ok(Advertiser { octets, name, address: address_of(&peripheral) })
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

/// The bridge a connection's first `Ready` must name: the one the run `settled` on, once it has,
/// since that already matched `spec`, else the bridge `spec` names.
#[must_use]
pub fn settled_spec(spec: Option<&BridgeSpec>, settled: Option<Mac>) -> Option<BridgeSpec> {
    match settled {
        Some(mac) => Some(BridgeSpec::Mac(mac)),
        None => spec.cloned(),
    }
}

/// The one bridge among those `heard`, which holds at least one, each with its advertised octets
/// and whether it is advertising now: those advertising when any is, since BlueZ also lists the
/// bridges it has bonded, heard or not; then those `preferred` names when any is.
///
/// # Errors
/// [`TransportError::SeveralAdvertising`] when that leaves more than one.
pub fn choose<P>(
    mut heard: Vec<([u8; 2], bool, P)>,
    preferred: Option<&BridgeSpec>,
) -> Result<P, TransportError> {
    if heard.iter().any(|(_, live, _)| *live) {
        heard.retain(|(_, live, _)| *live);
    }
    if heard.iter().any(|(octets, _, _)| matches_octets(preferred, *octets)) {
        heard.retain(|(octets, _, _)| matches_octets(preferred, *octets));
    }
    if let [_] = heard.as_slice() {
        return Ok(heard.remove(0).2);
    }
    let mut octets: Vec<[u8; 2]> = heard.into_iter().map(|(octets, _, _)| octets).collect();
    octets.sort_unstable();
    let bridges: Vec<String> = octets.iter().map(|o| mac::octets(o).to_string()).collect();
    Err(TransportError::SeveralAdvertising { bridges: bridges.join(", ") })
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
    /// A `Ready` from a bridge the spec does not name, which shares only its advertised octets.
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

/// The bridge a run settled on, once a connection announced it, and its peripheral.
struct Settled {
    mac: Mac,
    peripheral: Peripheral,
}

/// Keep a link up: find the bridge, serve the connection, report the failure, try again.
async fn supervise(transport: BleTransport, mut plumbing: LinkPlumbing) {
    let mut previous: Option<String> = None;
    let mut delay = transport.reconnect_delay;
    let mut adapter: Option<Adapter> = None;
    // The bridge this run settled on, once a connection announced it; never another after.
    let mut settled: Option<Settled> = None;
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
    (settled, direct_failed): (&mut Option<Settled>, &mut bool),
    unreported: bool,
) -> End {
    let central = match adapter {
        Some(central) => central.clone(),
        None => match open_adapter().await {
            Ok(central) => adapter.insert(central).clone(),
            Err(e) => return End::failed(e.to_string()),
        },
    };
    let (peripheral, heard) = match settled {
        Some(Settled { peripheral, .. }) => (peripheral.clone(), false),
        None => {
            // A scan's discovery builds a live handle only for a peripheral btleplug holds none
            // for (§ "Each try takes a fresh handle").
            #[cfg(target_os = "macos")]
            tokio::select! {
                cleared = central.clear_peripherals() => {
                    if let Err(e) = cleared {
                        tracing::debug!(error = %e, "could not clear the Bluetooth peripherals");
                    }
                }
                () = plumbing.events.closed() => return End::HandleDropped,
            }
            let found = tokio::select! {
                found = find(&central, transport, *direct_failed) => found,
                () = plumbing.events.closed() => {
                    let _ = central.stop_scan().await;
                    return End::HandleDropped;
                }
            };
            match found {
                Ok(Found { peripheral, direct, heard }) => {
                    *direct_failed |= direct;
                    (peripheral, heard)
                }
                Err(e) => return End::failed(e.to_string()),
            }
        }
    };
    let peripheral = tokio::select! {
        peripheral = fresh(&central, peripheral) => peripheral,
        () = plumbing.events.closed() => return End::HandleDropped,
    };
    connect(&central, &peripheral, transport, plumbing, settled, unreported, heard).await
}

/// A live handle for `peripheral`. On macOS, CoreBluetooth is asked for it by its identifier:
/// that returns the same handle while it is live, and a new one once a disconnect has dropped it,
/// unless a rescan has re-added it (§ "Each try takes a fresh handle"). Elsewhere, and when
/// CoreBluetooth returns none, it is `peripheral` itself.
async fn fresh(adapter: &Adapter, peripheral: Peripheral) -> Peripheral {
    #[cfg(target_os = "macos")]
    {
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
    }
    #[cfg(not(target_os = "macos"))]
    let _ = adapter;
    peripheral
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

/// How a peripheral is remembered: its address, or the OS's identifier where the address is
/// hidden, as on macOS.
fn address_of(peripheral: &Peripheral) -> String {
    let address = peripheral.address();
    if address == BDAddr::default() { peripheral.id().to_string() } else { address.to_string() }
}

/// The peripheral a try connects to ([`find`]).
struct Found {
    peripheral: Peripheral,
    /// Whether it is the remembered one, reached without a scan.
    direct: bool,
    /// Whether the scan that found it heard it advertising.
    heard: bool,
}

/// The peripheral to connect to, by [`look`]: the remembered one when the OS lists it and no
/// connection straight to it has failed, else the one a scan hears advertising the id.
async fn find(
    adapter: &Adapter,
    transport: &BleTransport,
    direct_failed: bool,
) -> Result<Found, TransportError> {
    let spec = transport.spec.as_ref();
    let Look { direct, preferred } =
        look(spec, transport.memory.recall(), transport.memory.recall_bluetooth(), direct_failed);
    if let Some(remembered) = direct {
        let listed = adapter.peripherals().await.map_err(bluetooth)?;
        if let Some(known) = listed
            .into_iter()
            .find(|p| is_remembered(&remembered, &p.address().to_string(), &p.id().to_string()))
        {
            return Ok(Found { peripheral: known, direct: true, heard: false });
        }
    }
    let (_, _, peripheral, heard) = scan(adapter, spec, preferred.as_ref()).await?;
    Ok(Found { peripheral, direct: false, heard })
}

/// A bridge a scan heard, as [`choose`] takes it: its octets, whether it is advertising, and its
/// name beside the peripheral.
type Bridge<P> = ([u8; 2], bool, (Option<String>, P));

/// A bridge one scan has heard.
#[derive(Debug)]
struct Kept<I, P> {
    id: I,
    octets: [u8; 2],
    name: Option<String>,
    /// Whether it has been heard advertising.
    live: bool,
    peripheral: P,
}

/// The bridges one scan has heard, by the OS's identifier `I` for each peripheral `P`, with their
/// octets and whether each has been heard advertising.
struct Hearing<'a, I, P> {
    spec: Option<&'a BridgeSpec>,
    bridges: Vec<Kept<I, P>>,
    /// Bridges `spec` does not name, never read again.
    others: HashSet<I>,
    /// Peripherals read [`ID_READS`] times with no id, read again only when heard with one.
    left_alone: HashSet<I>,
    /// How many times each peripheral not yet identified has been read.
    unidentified: HashMap<I, u8>,
}

impl<'a, I: Clone + Eq + Hash, P> Hearing<'a, I, P> {
    fn new(spec: Option<&'a BridgeSpec>) -> Self {
        Self {
            spec,
            bridges: Vec::new(),
            others: HashSet::new(),
            left_alone: HashSet::new(),
            unidentified: HashMap::new(),
        }
    }

    /// Whether the scan can end before [`LISTEN_FOR`]: the spec gives both octets, so it names
    /// one bridge, and that bridge has been heard advertising.
    fn settled(&self) -> bool {
        self.spec.and_then(named_octets).is_some() && self.bridges.iter().any(|kept| kept.live)
    }

    /// Whether a hearing of `id` is worth reading its properties for: any device but a bridge
    /// already heard advertising and one `spec` does not name, and not one left alone unless it
    /// was `heard_with_id`.
    fn wants(&self, id: &I, heard_with_id: bool) -> bool {
        match self.bridges.iter().find(|kept| kept.id == *id) {
            Some(kept) => !kept.live,
            None => !self.others.contains(id) && (heard_with_id || !self.left_alone.contains(id)),
        }
    }

    /// Take what a read of `peripheral` found. A bridge `spec` names is kept, and one already
    /// kept is marked advertising once a read says so. A read with no id counts towards
    /// [`ID_READS`].
    fn noted(&mut self, id: I, peripheral: P, read: Option<Read>) {
        if let Some(kept) = self.bridges.iter_mut().find(|kept| kept.id == id) {
            if let Some(read) = read {
                kept.live |= read.live;
                kept.name = kept.name.take().or(read.name);
            }
            return;
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
            return;
        };
        self.unidentified.remove(&id);
        self.left_alone.remove(&id);
        if matches_octets(self.spec, octets) {
            let Read { name, live, .. } = read;
            self.bridges.push(Kept { id, octets, name, live, peripheral });
        } else {
            self.others.insert(id);
        }
    }

    /// The bridges heard, as [`choose`] takes them, each with its name.
    fn into_bridges(self) -> Vec<Bridge<P>> {
        self.bridges
            .into_iter()
            .map(|kept| (kept.octets, kept.live, (kept.name, kept.peripheral)))
            .collect()
    }
}

/// Scan for up to [`SCAN_FOR`] for a bridge `spec` names, preferring one `preferred` names, and
/// return its octets and name beside it, and whether it was heard advertising.
async fn scan(
    adapter: &Adapter,
    spec: Option<&BridgeSpec>,
    preferred: Option<&BridgeSpec>,
) -> Result<([u8; 2], Option<String>, Peripheral, bool), TransportError> {
    // Opened before the scan starts, so nothing it hears goes by unreported.
    let mut events = adapter.events().await.map_err(bluetooth)?;
    adapter.start_scan(ScanFilter::default()).await.map_err(bluetooth)?;
    let heard = listen(adapter, &mut events, spec).await;
    let _ = adapter.stop_scan().await;
    let heard =
        heard?.into_iter().map(|(octets, live, (name, p))| (octets, live, (octets, name, p, live)));
    choose(heard.collect(), preferred)
}

/// Read `peripheral`'s properties, when `hearing` wants them, and note what the read found. A
/// peripheral `heard_with_id` is read even once left alone.
async fn note(
    hearing: &mut Hearing<'_, PeripheralId, Peripheral>,
    peripheral: Peripheral,
    heard_with_id: bool,
) {
    let id = peripheral.id();
    if hearing.wants(&id, heard_with_id) {
        let read = read_of(&peripheral).await;
        hearing.noted(id, peripheral, read);
    }
}

/// The bridge `spec` names, for the message when none is found: its octets when the spec gives
/// both, else the spec as written.
fn sought(spec: &BridgeSpec) -> String {
    named_octets(spec).map_or_else(|| spec.to_string(), |octets| mac::octets(&octets).to_string())
}

/// Read the adapter's events until a bridge `spec` names is heard: at once when the spec gives
/// both its octets and it is heard advertising ([`Hearing::settled`]), else once [`LISTEN_FOR`]
/// has passed. At that point the OS's listing is read once, for a bridge that raised no event.
async fn listen(
    adapter: &Adapter,
    events: &mut Events,
    spec: Option<&BridgeSpec>,
) -> Result<Vec<Bridge<Peripheral>>, TransportError> {
    let started = tokio::time::Instant::now();
    let (listened, give_up) = (started + LISTEN_FOR, started + SCAN_FOR);
    let mut hearing = Hearing::new(spec);
    let mut listed = false;
    let id_key = uuid_from_u16(ID_UUID);
    loop {
        if hearing.settled() {
            break;
        }
        if !listed && tokio::time::Instant::now() >= listened {
            listed = true;
            for peripheral in adapter.peripherals().await.map_err(bluetooth)? {
                note(&mut hearing, peripheral, false).await;
            }
        }
        if listed && !hearing.bridges.is_empty() {
            break;
        }
        if tokio::time::Instant::now() >= give_up {
            return Err(TransportError::NotFound {
                bridge: spec.map(sought),
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
                    if hearing.wants(&id, false)
                        && let Ok(peripheral) = adapter.peripheral(&id).await
                    {
                        note(&mut hearing, peripheral, false).await;
                    }
                }
                // The bridge heard: BlueZ reports its service data on every advertisement.
                Some(CentralEvent::ServiceDataAdvertisement { id, service_data }) => {
                    let with_id = service_data.contains_key(&id_key);
                    if hearing.wants(&id, with_id)
                        && let Ok(peripheral) = adapter.peripheral(&id).await
                    {
                        note(&mut hearing, peripheral, with_id).await;
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
    if let Some(octets) = read_of(peripheral).await.and_then(|read| read.octets) {
        return mac::octets(&octets).to_string();
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
    settled: &mut Option<Settled>,
    unreported: bool,
    heard: bool,
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
                return End::failed(connect_timeout_text(&label, heard, cfg!(target_os = "macos")));
            }
        }
    }
    let end = serve(adapter, peripheral, transport, plumbing, settled, (&label, unreported)).await;
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
    settled: &mut Option<Settled>,
    (label, unreported): (&str, bool),
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
    let pairs = awaits_pairing(OS_PAIRS_ON_CONNECT, settled.is_some());
    let subscribed =
        subscribe(peripheral.subscribe(&up), (label, unreported), pairs, &plumbing.events);
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

/// Wait for `subscribing`, the subscription to Up, for [`subscribe_within`]. When the OS `pairs`
/// on the connection and the subscription is unanswered [`PAIRING_NOTICE_AFTER`] in, one
/// [`DisconnectCause::AwaitingPairing`] goes to `events` and the wait goes on. It is logged at
/// info only while the failure is `unreported`. A subscription that sent that notice and then
/// succeeds sends one [`DisconnectCause::AwaitingReady`]. The C6's refusal for want of a bond is
/// [`refusal`]'s.
async fn subscribe(
    subscribing: impl Future<Output = btleplug::Result<()>>,
    (label, unreported): (&str, bool),
    pairs: bool,
    events: &mpsc::Sender<LinkEvent>,
) -> Result<(), Failure> {
    let subscribing = tokio::time::timeout(subscribe_within(pairs), subscribing);
    tokio::pin!(subscribing);
    let notice = tokio::time::sleep(PAIRING_NOTICE_AFTER);
    tokio::pin!(notice);
    let mut noticed = !pairs;
    // `noticed` starts set where the OS pairs beforehand, so it cannot say a notice went out.
    let mut sent = false;
    let answered = loop {
        tokio::select! {
            // An answer on the poll the notice falls due is no wait to announce.
            biased;
            answered = &mut subscribing => break answered,
            () = &mut notice, if !noticed => {
                noticed = true;
                // Retrying a failure already reported is not news, as in `connect`.
                if unreported {
                    tracing::info!(bridge = %label, "the subscription waits on the OS's pairing");
                } else {
                    tracing::debug!(bridge = %label, "the subscription waits on the OS's pairing");
                }
                let waiting = LinkEvent::Disconnected {
                    reason: pairing_notice(label, cfg!(target_os = "macos")),
                    cause: DisconnectCause::AwaitingPairing,
                };
                // A dropped handle ends the wait in `unless_closed`.
                let _ = events.send(waiting).await;
                sent = true;
            }
        }
    };
    match answered {
        Ok(Ok(())) => {
            if sent {
                let through = LinkEvent::Disconnected {
                    reason: ready_notice(label),
                    cause: DisconnectCause::AwaitingReady,
                };
                // A dropped handle ends the connection in `unless_closed`.
                let _ = events.send(through).await;
            }
            Ok(())
        }
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
    settled: &mut Option<Settled>,
    (up, down): (&Characteristic, &Characteristic),
    (notes, events): (&mut Notes, &mut Events),
) -> End {
    let id = peripheral.id();
    // Once settled, only the settled bridge is taken (§ "Finding the bridge").
    let wanted = settled_spec(transport.spec.as_ref(), settled.as_ref().map(|s| s.mac));
    let mut inbound = Inbound::new(wanted.clone());
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
                            *settled = Some(Settled { mac, peripheral: peripheral.clone() });
                            transport.memory.remember_bluetooth(mac, address_of(peripheral));
                        }
                        Heard::WrongBridge(mac) => {
                            let named = wanted.as_ref().map(ToString::to_string);
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
    use std::time::Duration;

    use btleplug::api::PeripheralProperties;
    use btleplug::api::bleuuid::uuid_from_u16;
    use tokio::sync::mpsc;
    use tokio::time::Instant;
    use wartui_proto::ble::ID_UUID;
    use wartui_proto::wardrive_esp::beacon::{self, Beacon, COMPANY, STATUS_MAX};

    use super::{
        BridgeSpec, CONNECT_WITHIN, End, Hearing, ID_READS, IDENTIFY_EVERY, PAIR_WITHIN,
        PAIRING_NOTICE_AFTER, Read, asker, awaits_pairing, choose, connect_timeout_text,
        notice_allowance, pairing_notice, ready_notice, subscribe, subscribe_within, unless_closed,
    };
    use crate::{DisconnectCause, LinkEvent, link_pair};

    fn spec(text: &str) -> BridgeSpec {
        text.parse().unwrap_or_else(|never| match never {})
    }

    const BRIDGE: [u8; 6] = [0x02, 0x00, 0x5E, 0x10, 0x44, 0xC0];

    /// A read that found the bridge `octets`, advertising now or not.
    fn read(octets: [u8; 2], live: bool) -> Option<Read> {
        Some(Read { octets: Some(octets), name: None, live })
    }

    /// A read of a device that carries no id.
    fn unidentified(live: bool) -> Option<Read> {
        Some(Read { octets: None, name: Some("Pixel 8".to_owned()), live })
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
    fn ble_hearing_marks_bridge_live_when_heard_again_after_a_listed_read() {
        // BlueZ replays a bonded bridge as discovered before the scan has heard it.
        let mut hearing = Hearing::new(None);
        hearing.noted(1, "remembered", read([0x44, 0xC0], false));
        hearing.noted(2, "other", read([0xA1, 0xB0], true));
        assert!(hearing.wants(&1, false), "a bridge not heard advertising is read again");
        hearing.noted(1, "remembered", read([0x44, 0xC0], true));
        assert!(!hearing.wants(&1, false), "nor once it has been");
        assert!(!hearing.wants(&1, true), "even when heard with the id");
        let chosen = choose(hearing.into_bridges(), Some(&BridgeSpec::Mac(BRIDGE)));
        assert_eq!(chosen.expect("one bridge").1, "remembered");
    }

    #[test]
    fn ble_hearing_leaves_device_alone_when_reads_without_id_reach_the_cap() {
        let mut hearing: Hearing<'_, u8, ()> = Hearing::new(None);
        for _ in 1..ID_READS {
            hearing.noted(1, (), unidentified(true));
            assert!(hearing.wants(&1, false));
        }
        hearing.noted(1, (), unidentified(true));
        assert!(!hearing.wants(&1, false), "left alone after {ID_READS} reads");
        assert!(hearing.wants(&1, true), "but read when heard with the id");
    }

    #[test]
    fn ble_hearing_keeps_bridge_when_it_is_identified_on_a_later_read() {
        let mut hearing = Hearing::new(None);
        hearing.noted(1, "late", unidentified(true));
        hearing.noted(1, "late", read([0x44, 0xC0], true));
        assert_eq!(hearing.into_bridges(), [([0x44, 0xC0], true, (None, "late"))]);
    }

    #[test]
    fn ble_hearing_keeps_bridge_when_service_data_event_follows_reads_that_ran_out() {
        // BlueZ lists a bonded bridge before it has heard its service data, so the id can arrive
        // after the reads ran out.
        let mut hearing = Hearing::new(None);
        for _ in 0..ID_READS {
            hearing.noted(1, "late", unidentified(false));
        }
        assert!(!hearing.wants(&1, false));
        assert!(hearing.wants(&1, true), "the service-data event reads it");
        hearing.noted(1, "late", read([0x44, 0xC0], true));
        assert_eq!(hearing.into_bridges(), [([0x44, 0xC0], true, (None, "late"))]);
    }

    #[test]
    fn ble_hearing_drops_bridge_when_spec_names_another_bridges_octets() {
        let named = spec("44:C0");
        let mut hearing: Hearing<'_, u8, ()> = Hearing::new(Some(&named));
        hearing.noted(1, (), read([0xA1, 0xB0], true));
        assert!(!hearing.wants(&1, false), "another bridge is not read again");
        assert!(!hearing.wants(&1, true), "even when heard with the id");
        assert!(hearing.into_bridges().is_empty());
    }

    #[test]
    fn ble_hearing_keeps_name_when_a_later_read_gives_one() {
        let mut hearing = Hearing::new(None);
        hearing.noted(1, (), read([0x44, 0xC0], false));
        let named = Read { octets: None, name: Some("wartui 44:C0".to_owned()), live: true };
        hearing.noted(1, (), Some(named));
        let name = Some("wartui 44:C0".to_owned());
        assert_eq!(hearing.into_bridges(), [([0x44, 0xC0], true, (name, ()))]);
    }

    #[test]
    fn ble_hearing_settles_when_bridge_named_in_full_is_heard_advertising() {
        let named = spec("44:C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read([0x44, 0xC0], true));
        assert!(hearing.settled());
    }

    #[test]
    fn ble_hearing_waits_for_listing_when_bridge_named_in_full_is_only_listed() {
        // BlueZ replays a bonded bridge that is off as discovered, with no RSSI.
        let named = spec("02:00:5E:10:44:C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read([0x44, 0xC0], false));
        assert!(!hearing.settled());
    }

    #[test]
    fn ble_hearing_waits_for_listing_when_one_octet_matches_one_live_bridge() {
        // `C0` names every bridge ending in it, so `choose` has to see them all.
        let named = spec("C0");
        let mut hearing = Hearing::new(Some(&named));
        hearing.noted(1, (), read([0x44, 0xC0], true));
        assert!(!hearing.settled());
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_fails_when_bridge_does_not_answer_in_time() {
        let (events, mut heard) = mpsc::channel(4);
        let start = Instant::now();
        let failed = subscribe(std::future::pending(), ("44:C0", true), false, &events).await.err();
        let failure = failed.expect("an unanswered subscription fails");
        assert_eq!(failure.reason, "44:C0 did not answer the subscription");
        assert_eq!(start.elapsed(), CONNECT_WITHIN);
        assert!(heard.try_recv().is_err(), "no notice where the OS pairs beforehand");
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_waits_the_pairing_window_when_os_pairs_on_connection() {
        let (events, mut heard) = mpsc::channel(4);
        let start = Instant::now();
        let subscribing = subscribe(std::future::pending(), ("44:C0", true), true, &events);
        tokio::pin!(subscribing);
        let notice = tokio::select! {
            notice = heard.recv() => notice,
            _ = &mut subscribing => panic!("the subscription ended before its notice"),
        };
        assert_eq!(start.elapsed(), PAIRING_NOTICE_AFTER);
        let Some(LinkEvent::Disconnected { cause, .. }) = notice else {
            panic!("not a link event: {notice:?}");
        };
        assert_eq!(cause, DisconnectCause::AwaitingPairing);
        let failed = subscribing.await.err();
        let failure = failed.expect("an unanswered subscription fails");
        assert_eq!(failure.reason, "44:C0 did not answer the subscription");
        assert_eq!(start.elapsed(), PAIR_WITHIN);
        assert!(heard.try_recv().is_err(), "the notice is sent once");
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_sends_no_notice_when_bond_resumes_before_it_is_due() {
        // A bonded Mac subscribes about a second after connecting.
        let (events, mut heard) = mpsc::channel(4);
        let resumed = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            Ok(())
        };
        assert!(subscribe(resumed, ("44:C0", true), true, &events).await.is_ok());
        assert!(heard.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_sends_no_notice_when_answer_comes_as_notice_falls_due() {
        let (events, mut heard) = mpsc::channel(4);
        let answered = async {
            tokio::time::sleep(PAIRING_NOTICE_AFTER).await;
            Ok(())
        };
        assert!(subscribe(answered, ("44:C0", true), true, &events).await.is_ok());
        assert!(heard.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn ble_subscribe_reports_awaiting_ready_when_answer_comes_after_notice() {
        // The operator clicked Connect; the bridge's `Ready` is still to come.
        let (events, mut heard) = mpsc::channel(4);
        let answered = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok(())
        };
        assert!(subscribe(answered, ("44:C0", true), true, &events).await.is_ok());
        let Ok(LinkEvent::Disconnected { cause, .. }) = heard.try_recv() else {
            panic!("no pairing notice");
        };
        assert_eq!(cause, DisconnectCause::AwaitingPairing);
        let Ok(LinkEvent::Disconnected { reason, cause }) = heard.try_recv() else {
            panic!("no ready notice");
        };
        assert_eq!(reason, ready_notice("44:C0"));
        assert_eq!(cause, DisconnectCause::AwaitingReady);
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn ble_ready_notice_names_bridge_when_subscription_is_through() {
        assert_eq!(ready_notice("44:C0"), "waiting for 44:C0 to answer");
    }

    #[test]
    fn ble_awaits_pairing_when_os_pairs_on_connection_and_run_has_not_settled() {
        assert!(awaits_pairing(true, false));
    }

    #[test]
    fn ble_awaits_no_pairing_when_run_has_settled() {
        // A bridge that answered a `Ready` holds this host's bond.
        assert!(!awaits_pairing(true, true));
    }

    #[test]
    fn ble_awaits_no_pairing_when_os_pairs_beforehand() {
        assert!(!awaits_pairing(false, false));
    }

    #[test]
    fn ble_notice_allowance_gives_notice_delay_when_os_pairs_on_connection() {
        assert_eq!(notice_allowance(true), PAIRING_NOTICE_AFTER);
    }

    #[test]
    fn ble_notice_allowance_gives_nothing_when_os_pairs_beforehand() {
        assert_eq!(notice_allowance(false), Duration::ZERO);
    }

    #[test]
    fn ble_subscribe_within_gives_pairing_window_when_os_pairs_on_connection() {
        assert_eq!(subscribe_within(true), PAIR_WITHIN);
    }

    #[test]
    fn ble_subscribe_within_gives_connect_deadline_when_os_pairs_beforehand() {
        assert_eq!(subscribe_within(false), CONNECT_WITHIN);
    }

    #[test]
    fn ble_connect_timeout_names_stale_bond_when_bridge_was_heard_on_macos() {
        assert_eq!(
            connect_timeout_text("44:C0", true, true),
            "could not connect to 44:C0 in 10s, though it advertises. Come closer, or forget it \
             in System Settings → Bluetooth if the bridge dropped this Mac's bond, then pair \
             again."
        );
    }

    #[test]
    fn ble_connect_timeout_fits_fault_box_when_terminal_is_80_columns() {
        // At 80 columns the view's fault box takes 3 lines of the width less 2, wrapped on words
        // (`footer::fault_lines` in `wartui`). A greedy wrap wastes at most the longest word less
        // one character at each line end but the last, so this bound proves the fit.
        for label in ["44:C0", "02:00:5E:10:44:C0"] {
            let line = format!("link down: {}", connect_timeout_text(label, true, true));
            let longest_word =
                line.split_whitespace().map(|word| word.chars().count()).max().unwrap_or(0);
            let chars = line.chars().count();
            assert!(chars + 2 * longest_word.saturating_sub(1) <= 3 * 78, "{line}");
        }
    }

    #[test]
    fn ble_connect_timeout_says_power_and_range_when_bridge_was_not_heard() {
        assert_eq!(
            connect_timeout_text("44:C0", false, true),
            "could not connect to 44:C0 in 10s; check the bridge is powered and in range"
        );
    }

    #[test]
    fn ble_connect_timeout_says_power_and_range_when_not_on_macos() {
        for heard in [true, false] {
            assert_eq!(
                connect_timeout_text("44:C0", heard, false),
                "could not connect to 44:C0 in 10s; check the bridge is powered and in range"
            );
        }
    }

    #[test]
    fn ble_pairing_notice_names_connect_when_on_macos() {
        assert_eq!(
            pairing_notice("44:C0", true),
            "pairing: click Connect if macOS shows a dialog for 44:C0"
        );
    }

    #[test]
    fn ble_pairing_notice_names_the_os_when_not_on_macos() {
        assert_eq!(
            pairing_notice("44:C0", false),
            "pairing: accept the dialog if the OS shows one for 44:C0"
        );
    }

    #[test]
    fn ble_pairing_notice_fits_footer_line_when_label_is_a_full_address() {
        // The view draws it as a line of its own, unwrapped.
        for macos in [true, false] {
            let line = format!("  {}", pairing_notice("02:00:5E:10:44:C0", macos));
            assert!(line.chars().count() <= 80, "{line}");
        }
    }

    #[test]
    fn ble_pairing_notice_keeps_instruction_when_terminal_is_58_columns() {
        // A cut at the edge of a small screen costs the label, never what to do.
        for (macos, instruction) in [(true, "click Connect"), (false, "accept the dialog")] {
            let line = format!("  {}", pairing_notice("44:C0", macos));
            let shown: String = line.chars().take(58).collect();
            assert!(shown.contains(instruction), "{shown}");
        }
    }

    #[test]
    fn ble_ready_notice_fits_footer_line_when_terminal_is_58_columns() {
        // The view draws it unwrapped, so the whole line fits the smallest screen.
        let line = format!("  {}", ready_notice("02:00:5E:10:44:C0"));
        assert!(line.chars().count() <= 58, "{line}");
    }

    #[tokio::test(start_paused = true)]
    async fn ble_unless_closed_ends_connection_when_handle_goes_during_subscription() {
        // A quit while the C6 has yet to answer: the connection lets the bridge go at once.
        let (handle, plumbing) = link_pair();
        let start = Instant::now();
        let subscribing =
            subscribe(std::future::pending(), ("44:C0", true), true, &plumbing.events);
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
