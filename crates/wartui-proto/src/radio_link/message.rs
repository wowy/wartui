//! The messages a [`Kind::Messages`](super::Kind::Messages) packet carries, and how
//! several share one packet.
//!
//! Not frozen. Both firmwares are built from one tree, so these are postcard, like the host
//! link, and change whenever the work needs. [`LINK_REVISION`] names the encoding, and the
//! hello carries it so a P4 never bridges with a C6 that encodes them differently.
//!
//! They carry the P4's poll for the C6's counters, the counters, the C6's lines for the host,
//! and the host link over Bluetooth: the host stream each way ([`Down::HostBytes`],
//! [`Up::HostBytes`]), the C6's credit for what it took, its Bluetooth state and counters, and
//! its bonds ([`Down::Bonds`], [`Up::BondAdded`], [`Up::BondRemoved`]), which the P4 keeps
//! (`crate::bonds`).
//!
//! **The host stream is credited.** The P4 sends `Down::HostBytes` only while the bytes it has
//! sent, less [`Up::BleCredit`]'s `drained`, stay under a window, so a slow central backs the
//! stream up in the P4's host queue rather than holding the whole link behind RTS/CTS.
//! `drained` is cumulative since the C6 booted and wraps: the P4 reads a credit as
//! `drained.wrapping_sub(last)`, so a lost credit costs nothing once the next arrives.
//!
//! A payload is records of `[len: u16 LE][postcard bytes]`, ending at a zero `len` or the
//! payload's end. A record never spans packets: the largest message fits one, so the
//! packer refuses rather than splits. A queued record that does not fit is [`Held`] and
//! leads the next packet.

use heapless::Vec;
use serde::{Deserialize, Serialize, Serializer};

use crate::bonds::{BondAddr, BondRecord, MAX_BONDS};
use crate::link::{LogStr, QueueStat};

/// The revision of the [`Up`] and [`Down`] encoding, which [`Hello`](super::Hello)
/// carries.
///
/// Not frozen. Held at 1 until 1.0, as [`crate::link::LINK_PROTO_VERSION`] is: both firmwares
/// are flashed from one tree. `radio_link_revision_golden_bytes` in `tests/radio_link.rs` pins
/// the encoding, and is re-pinned when these types change shape.
pub const LINK_REVISION: u16 = 1;

/// P4 to C6.
// Unboxed for the reason `crate::link::HostToBridge` gives.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Down<'a> {
    /// Ask for an [`Up::RadioStatus`].
    GetRadioStatus,
    /// The next bytes of the host stream: COBS-framed `BridgeToHost` frames, as UART0 carries
    /// them, cut wherever a packet ended. At most [`HOST_BYTES_MAX`].
    HostBytes(#[serde(serialize_with = "byte_string", borrow)] &'a [u8]),
    /// Every bond the P4 holds, oldest first, and the pairing window's time left. Sent after each
    /// hello answer, and whenever the bonds or the window change.
    Bonds {
        /// The bonds.
        bonds: Vec<BondRecord, MAX_BONDS>,
        /// Milliseconds the pairing window stays open from now: 0 while it is closed,
        /// [`WINDOW_UNTIL_BONDED`] while no bond is stored.
        window_ms: u32,
    },
}

/// [`Down::Bonds`]'s `window_ms` while no bond is stored: open until one is.
pub const WINDOW_UNTIL_BONDED: u32 = u32::MAX;

/// C6 to P4.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Up<'a> {
    /// Reply to [`Down::GetRadioStatus`].
    RadioStatus(RadioStatus),
    /// A line for the host's log.
    Log {
        /// The text.
        message: LogStr,
    },
    /// A fault for the host.
    Error {
        /// The text.
        message: LogStr,
    },
    /// Bytes of the `HostToBridge` stream a central wrote, cut wherever a write ended. At most
    /// [`HOST_BYTES_MAX`].
    HostBytes(#[serde(serialize_with = "byte_string", borrow)] &'a [u8]),
    /// The C6 took more of the host stream off the link.
    BleCredit {
        /// `Down::HostBytes` bytes taken since the C6 booted, wrapping: notified, or discarded
        /// with no subscriber.
        drained: u32,
    },
    /// The Bluetooth link changed. Sent on every change.
    BleState(BleState),
    /// The C6's Bluetooth counters, every 2 s.
    BleStats(BleStats),
    /// A central paired and bonded: store it as the newest bond.
    BondAdded(BondRecord),
    /// A bonded central paired again: drop its old bond. A [`Up::BondAdded`] follows.
    BondRemoved(BondAddr),
}

/// The C6's Bluetooth link, as [`Up::BleState`] reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BleState {
    /// A central has subscribed to the host stream's notifications. Bluetooth owns the host
    /// queue while it holds.
    pub subscribed: bool,
    /// The ATT MTU, or 0 with no connection.
    pub mtu: u16,
    /// The PHY: 1 for 1M, 2 for 2M, 3 for Coded, 0 with no connection.
    pub phy: u8,
    /// The connection interval, in microseconds, or 0 with no connection.
    pub interval_us: u32,
    /// Centrals connected.
    pub conn: u8,
    /// The link is encrypted.
    pub encrypted: bool,
    /// The central holds a stored bond. The host stream flows only while this and `encrypted`
    /// both hold.
    pub bonded: bool,
}

/// The C6's Bluetooth counters since it booted, as [`Up::BleStats`] reports them. Each wraps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BleStats {
    /// Notifications sent.
    pub notifications: u32,
    /// Host stream bytes those notifications carried.
    pub notified_bytes: u32,
    /// ACL payload bytes handed to the controller: the notifications with their L2CAP and ATT
    /// headers, and whatever else the host sent.
    pub acl_bytes: u32,
    /// ACL payload bytes the controller reported completed: link-layer delivery.
    pub delivered_bytes: u32,
    /// Link errors: failed notifications and dropped connections.
    pub link_errors: u32,
    /// Host stack errors: transport failures and the host runner stopping.
    pub hci_errors: u32,
    /// Bytes a central wrote that went to the P4 as `Up::HostBytes`.
    pub forwarded_bytes: u32,
    /// `Down::HostBytes` bytes discarded, and credited, with no subscriber.
    pub dropped_unsubscribed: u32,
    /// `Down::HostBytes` bytes discarded, and credited, because the transmit buffer was full.
    pub dropped_overflow: u32,
    /// Bytes a central wrote that were dropped because the queue to the P4 stayed full.
    pub dropped_writes: u32,
    /// Outgoing ACL packets the delivery ledger had no room to track.
    pub untracked_acl: u32,
    /// Heap in use, in bytes.
    pub heap_used: u32,
    /// The most heap in use since boot, in bytes.
    pub heap_high_water: u32,
    /// Where the notifier is now: [`NOTIFY_PHASES`] names each.
    pub notify_phase: u8,
    /// Notifications the packet pool had no room for, each retried after a sleep.
    pub notify_oom_retries: u32,
    /// Calls into TrouBLE's `notify_raw`.
    pub notify_raw_calls: u32,
    /// Returns from `notify_raw`.
    pub notify_raw_returns: u32,
    /// Connection events the event loop has taken.
    pub events_seen: u32,
    /// The latest connection event's kind: [`EVENT_KINDS`] names each, 0 before the first.
    pub last_event: u8,
    /// Where the event loop is now: [`EVENTS_PHASES`] names each.
    pub events_phase: u8,
    /// Where the advertiser is now: [`ADV_PHASES`] names each.
    pub adv_phase: u8,
    /// Connections accepted.
    pub connections: u32,
    /// Connections served to their end.
    pub disconnects: u32,
    /// HCI packets read from the controller.
    pub hci_reads: u32,
    /// HCI packets written to the controller.
    pub hci_writes: u32,
    /// Reads on the HCI transport pending now.
    pub reads_in_flight: u8,
    /// Writes on the HCI transport pending now.
    pub writes_in_flight: u8,
    /// Number Of Completed Packets events read from the controller.
    pub nocp_events: u32,
    /// ACL packets written that the controller has not completed, as the ledger tracks them.
    pub acl_outstanding: u32,
    /// Disconnection Complete events read from the controller.
    pub disconnection_events: u32,
    /// 1 while TrouBLE's runner runs, 0 between runs.
    pub runner_phase: u8,
    /// Returns from TrouBLE's runner, each followed by a fresh run.
    pub runner_restarts: u32,
    /// The latest disconnection's HCI reason code, or 0 before the first.
    pub last_disconnect_reason: u8,
    /// The last four disconnections' HCI reason codes, newest first, 0 where none.
    pub disconnect_reasons: [u8; 4],
    /// GATT requests the event loop took, Down's writes aside.
    pub gatt_requests: u32,
    /// Replies to those requests that TrouBLE finished sending.
    pub gatt_replies_sent: u32,
    /// The latest of those requests' kind: [`GATT_KINDS`] names each, 0 before the first.
    pub last_gatt_kind: u8,
    /// The latest of those requests' attribute handle, or 0 for one without.
    pub last_gatt_handle: u16,
    /// Changes of ATT MTU the notifier saw.
    pub mtu_changes: u32,
    /// Pairings that ended in a bond.
    pub pairings: u32,
    /// Pairings that failed.
    pub pairing_failures: u32,
    /// Centrals disconnected for not holding a bond: encrypted without one while the pairing
    /// window was closed, or not bonded and encrypted 10 s after connecting.
    pub unbonded_kicks: u32,
    /// Milliseconds from the latest bonded pairing's connection to its `PairingComplete`, or 0
    /// before the first.
    pub last_pair_ms: u32,
    /// Milliseconds from the latest bonded resume's connection to its encryption, or 0 before the
    /// first.
    pub last_resume_ms: u32,
}

/// [`BleStats::notify_phase`]'s values, by index.
pub const NOTIFY_PHASES: [&str; 4] = ["ready wait", "notify_raw", "OOM sleep", "credit"];

/// [`BleStats::events_phase`]'s values, by index.
pub const EVENTS_PHASES: [&str; 4] = ["next", "forward", "reply send", "other"];

/// [`BleStats::adv_phase`]'s values, by index.
pub const ADV_PHASES: [&str; 3] = ["advertise", "serve", "backoff"];

/// [`BleStats::last_event`]'s values, by index.
pub const EVENT_KINDS: [&str; 16] = [
    "none",
    "Disconnected",
    "PhyUpdated",
    "ConnectionParamsUpdated",
    "SubratingParamsUpdated",
    "RequestConnectionParams",
    "DataLengthUpdated",
    "FrameSpaceUpdated",
    "ConnectionRateChanged",
    "Gatt Down write",
    "Gatt other",
    "PairingComplete",
    "PairingFailed",
    "BondLost",
    "Encrypted",
    "pass key or OOB",
];

/// [`BleStats::last_gatt_kind`]'s values, by index.
pub const GATT_KINDS: [&str; 4] = ["none", "read", "write", "other"];

/// The C6's counters, which the P4 reports beside its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RadioStatus {
    /// The C6's bounded queues.
    pub queues: Vec<QueueStat, 4>,
    /// Packets from the P4 that failed their CRC.
    pub crc_fail: u32,
    /// Packets from the P4 without [`super::MAGIC`].
    pub bad_magic: u32,
    /// Breaks in the P4's seq.
    pub seq_gap: u32,
    /// Line errors on this end's UART: framing, parity, glitch or FIFO overflow.
    pub uart_errors: u32,
}

/// Packs encoded records into a packet's payload.
#[derive(Debug)]
pub struct Packer<'a> {
    payload: &'a mut [u8],
    len: usize,
}

impl<'a> Packer<'a> {
    /// A packer writing from the start of `payload`.
    pub fn new(payload: &'a mut [u8]) -> Self {
        Self { payload, len: 0 }
    }

    /// Appends one record, or returns `false` and changes nothing when it does not fit.
    pub fn push_encoded(&mut self, record: &[u8]) -> bool {
        let end = self.len + PREFIX + record.len();
        let Ok(record_len) = u16::try_from(record.len()) else { return false };
        if end > self.payload.len() {
            return false;
        }
        self.payload[self.len..self.len + PREFIX].copy_from_slice(&record_len.to_le_bytes());
        self.payload[self.len + PREFIX..end].copy_from_slice(record);
        self.len = end;
        true
    }

    /// Appends one record that `write` encodes into the room it is given, returning its
    /// length or `None`. Returns `false` and changes nothing when `write` refuses.
    pub fn push_with(&mut self, write: impl FnOnce(&mut [u8]) -> Option<usize>) -> bool {
        let start = self.len + PREFIX;
        let room = self.room();
        if start > self.payload.len() {
            return false;
        }
        let Some(len) = write(&mut self.payload[start..start + room]) else { return false };
        if len == 0 || len > room {
            return false;
        }
        self.payload[self.len..start].copy_from_slice(&(len as u16).to_le_bytes());
        self.len = start + len;
        true
    }

    /// The longest record that still fits.
    #[must_use]
    pub fn room(&self) -> usize {
        self.payload.len().saturating_sub(self.len + PREFIX).min(usize::from(u16::MAX))
    }

    /// Bytes packed so far, the `len` to seal the packet with.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is packed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// The longest record: a payload less its record's length prefix. A queue that holds none
/// longer always fits its record into an empty packet.
pub const RECORD_MAX: usize = super::PAYLOAD_MAX - PREFIX;

/// Bytes of length prefix ahead of each record.
const PREFIX: usize = 2;

/// The most host stream bytes one [`Down::HostBytes`] or [`Up::HostBytes`] carries: what fits
/// a [`RECORD_MAX`] record.
pub const HOST_BYTES_MAX: usize = host_bytes_fit(RECORD_MAX);

/// The most host stream bytes a [`Down::HostBytes`] or [`Up::HostBytes`] record of at most
/// `room` bytes carries: postcard's one-byte variant index, the length as a varint, then the
/// bytes.
#[must_use]
pub const fn host_bytes_fit(room: usize) -> usize {
    let one = room.saturating_sub(2);
    if one < 0x80 {
        return one;
    }
    let two = room - 3;
    if two < 0x80 {
        0x7F
    } else if two < 0x4000 {
        two
    } else {
        0x3FFF
    }
}

/// Write a borrowed slice as postcard's byte string in one call, rather than a byte at a time
/// as a sequence; the bytes on the wire are the same.
fn byte_string<S: Serializer>(bytes: &&[u8], serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_bytes(bytes)
}

/// A record taken from a queue that did not fit the last packet, sent first in the next.
#[derive(Debug)]
pub struct Held {
    buf: [u8; RECORD_MAX],
    len: usize,
}

impl Held {
    /// Nothing held. Const, so firmware builds it in a static.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; RECORD_MAX], len: 0 }
    }

    /// Packs the held record, then records from `next`, until one does not fit — it is then
    /// held for the next packet — or `next` has none.
    ///
    /// `next` copies one whole record into the buffer it is given and returns its length,
    /// or 0 when nothing is queued. `next` must read from a queue holding no record longer
    /// than [`RECORD_MAX`], so that one read is a whole record.
    pub fn pack(&mut self, packer: &mut Packer<'_>, mut next: impl FnMut(&mut [u8]) -> usize) {
        loop {
            if self.len == 0 {
                self.len = next(&mut self.buf);
                if self.len == 0 {
                    break;
                }
            }
            if !packer.push_encoded(&self.buf[..self.len]) {
                break;
            }
            self.len = 0;
        }
    }
}

impl Default for Held {
    fn default() -> Self {
        Self::new()
    }
}

/// The records in a payload, in order. A record whose `len` runs past the payload ends
/// the iteration.
pub fn records(payload: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = payload;
    core::iter::from_fn(move || {
        let (head, tail) = rest.split_first_chunk::<PREFIX>()?;
        let len = usize::from(u16::from_le_bytes(*head));
        if len == 0 || len > tail.len() {
            return None;
        }
        let (record, next) = tail.split_at(len);
        rest = next;
        Some(record)
    })
}

/// Encodes `msg` into `out` and returns its length.
///
/// # Errors
/// When `out` is too short.
pub fn encode_up(msg: &Up<'_>, out: &mut [u8]) -> Result<usize, postcard::Error> {
    postcard::to_slice(msg, out).map(|used| used.len())
}

/// Encodes `msg` into `out` and returns its length.
///
/// # Errors
/// When `out` is too short.
pub fn encode_down(msg: &Down<'_>, out: &mut [u8]) -> Result<usize, postcard::Error> {
    postcard::to_slice(msg, out).map(|used| used.len())
}

/// Decodes one record.
///
/// # Errors
/// When the record is not an [`Up`] this build can read.
pub fn decode_up(record: &[u8]) -> Result<Up<'_>, postcard::Error> {
    postcard::from_bytes(record)
}

/// Decodes one record.
///
/// # Errors
/// When the record is not a [`Down`] this build can read.
pub fn decode_down(record: &[u8]) -> Result<Down<'_>, postcard::Error> {
    postcard::from_bytes(record)
}
