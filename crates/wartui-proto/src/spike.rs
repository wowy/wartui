//! The Bluetooth host spike's load generator and counters: what the P4 sends when a host asks
//! for [`HostToBridge::SpikeLoad`](crate::link::HostToBridge::SpikeLoad), and how a host checks it.
//!
//! Each [`BridgeToHost::SpikeLoad`](crate::link::BridgeToHost::SpikeLoad) carries a `seq`, +1
//! per record from 0 when a host attaches, and a body whose last four bytes are a [`crc32`] over
//! the rest, little-endian. A host reads loss from the seq and corruption from the CRC.
//!
//! Over Bluetooth the C6 offers one GATT service, [`SERVICE_UUID`]. [`UP_UUID`] notifies the
//! host stream a USB host reads, split across notifications anywhere, and [`DOWN_UUID`] takes
//! host frames written without response, framed as on USB. The UUIDs live here so the C6 and the
//! host read the same ones.
//!
//! The C6 holds the stream it has yet to notify in a [`ByteRing`], and counts link-layer
//! delivery with an [`AclLedger`] over its HCI traffic.

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::link::{MAX_NODE_PAYLOAD, RadioLinkStats};
use crate::radio_link::{ADV_PHASES, BleStats, EVENT_KINDS, EVENTS_PHASES, NOTIFY_PHASES};

/// A sighting batch's typical body, the size the rate profiles send.
pub const SMALL_BODY: usize = 45;
/// A full sighting batch's body, the size [`SpikeProfile::Saturate`] and a burst send.
pub const FULL_BODY: usize = MAX_NODE_PAYLOAD;
/// How often the P4 sends [`SpikeStats`].
pub const STATS_EVERY_MS: u64 = 2_000;
/// How old [`SpikeStats::ble_stats_age_ms`] gets before the text calls the C6's stats stale.
pub const BLE_STATS_STALE_MS: u32 = 5_000;

/// The "wartui link" GATT service.
pub const SERVICE_UUID: u128 = 0xf241_0001_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Up characteristic, notify: the bridge's frame stream.
pub const UP_UUID: u128 = 0xf241_0002_d870_4eb6_a5a0_ed64_5732_a877;
/// The service's Down characteristic, write without response: the host's frames.
pub const DOWN_UUID: u128 = 0xf241_0003_d870_4eb6_a5a0_ed64_5732_a877;
/// What the C6 advertises ahead of its MAC's last two octets: `wartui 96:C4`.
pub const NAME_PREFIX: &str = "wartui ";

/// What the generator sends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SpikeProfile {
    /// Nothing.
    #[default]
    Off,
    /// A drive's sightings: 7.5 records a second of [`SMALL_BODY`] bytes.
    Drive,
    /// A twenty-node fleet's: 25 records a second of [`SMALL_BODY`] bytes.
    Fleet20,
    /// [`FULL_BODY`] records as fast as the host queue takes them without evicting.
    Saturate,
    /// `count` [`FULL_BODY`] records at 700 a second, then [`SpikeProfile::Fleet20`].
    Burst,
}

impl SpikeProfile {
    /// Records per 10 s, for the profiles that send at a rate.
    #[must_use]
    pub const fn per_10s(self) -> Option<u64> {
        match self {
            Self::Drive => Some(75),
            Self::Fleet20 => Some(250),
            Self::Burst => Some(7_000),
            Self::Off | Self::Saturate => None,
        }
    }

    /// The body length each record carries.
    #[must_use]
    pub const fn body_len(self) -> usize {
        match self {
            Self::Drive | Self::Fleet20 | Self::Off => SMALL_BODY,
            Self::Saturate | Self::Burst => FULL_BODY,
        }
    }
}

/// The P4's counters for the spike, every [`STATS_EVERY_MS`] while any host owns the host queue,
/// UART0's or a central's. Byte counts are since the P4 booted, and wrap.
///
/// `sent_bytes` grows only while Bluetooth owns the queue, so `sent_bytes / ble_owned_ms` is the
/// rate the C6 link carried, read later from a UART0 host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpikeStats {
    /// The generator's next seq.
    pub seq: u32,
    /// Milliseconds since the P4 booted.
    pub uptime_ms: u32,
    /// Milliseconds Bluetooth has owned the host queue since the P4 booted, the current stretch
    /// included.
    pub ble_owned_ms: u32,
    /// Encoded frame bytes queued for the host.
    pub queued_bytes: u32,
    /// Host stream bytes sent to the C6 as `Down::HostBytes`.
    pub sent_bytes: u32,
    /// Host stream bytes the C6 credited.
    pub credited_bytes: u32,
    /// The C6's latest `BleStats`.
    pub ble: BleStats,
    /// Milliseconds since that `BleStats` arrived, or `u32::MAX` before the first.
    pub ble_stats_age_ms: u32,
    /// The bulk ring's high-water mark, in bytes.
    pub bulk_high_water: u32,
    /// The bulk ring's size, in bytes, which [`SpikeStats::bulk_high_water`] is out of.
    pub bulk_capacity: u32,
    /// Frames the host rings refused.
    pub refused: u32,
    /// Frames the host rings evicted to make room.
    pub evicted: u32,
    /// The ATT MTU, from the C6's latest `BleState`.
    pub mtu: u16,
    /// The PHY, from the C6's latest `BleState`.
    pub phy: u8,
    /// The connection interval in microseconds, from the C6's latest `BleState`.
    pub interval_us: u32,
    /// The P4–C6 link's counters.
    pub radio_link: RadioLinkStats,
}

impl SpikeStats {
    /// The counters as five `#` lines, with the rate the C6 link carried while Bluetooth
    /// owned the queue: the bytes sent to the C6 over the time it owned it.
    #[must_use]
    pub const fn text(&self) -> StatsText<'_> {
        StatsText(self)
    }
}

/// [`SpikeStats::text`].
#[derive(Debug, Clone, Copy)]
pub struct StatsText<'a>(&'a SpikeStats);

impl fmt::Display for StatsText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let stats = self.0;
        write!(
            f,
            "# spike stats: up {}, Bluetooth owned {}, seq {}; ",
            Seconds(stats.uptime_ms),
            Seconds(stats.ble_owned_ms),
            stats.seq,
        )?;
        if stats.ble_owned_ms == 0 {
            f.write_str("no Bluetooth time yet")?;
        } else {
            let per_s = u64::from(stats.sent_bytes) * 1000 / u64::from(stats.ble_owned_ms);
            write!(f, "{per_s} B/s sent while Bluetooth owned the queue")?;
        }
        let link = &stats.radio_link;
        let ble = &stats.ble;
        write!(
            f,
            "\n#   queued {} B, sent {} B, credited {} B, delivered {} B; bulk high-water {} of \
             {} B, {} refused, {} evicted\n\
             #   mtu {}, phy {}, interval {} us; C6 link {} packets in, crc {}/{}, magic {}/{}, \
             gap {}/{}, uart {}/{} (P4/C6)\n\
             #   C6: {} notifications, {} B notified, ACL {} B sent, {} B delivered; errors {} \
             link, {} HCI; {} B forwarded; dropped {} B unsubscribed, {} B overflow, {} B \
             written, {} ACL untracked; heap {} B, high-water {} B",
            stats.queued_bytes,
            stats.sent_bytes,
            stats.credited_bytes,
            ble.delivered_bytes,
            stats.bulk_high_water,
            stats.bulk_capacity,
            stats.refused,
            stats.evicted,
            stats.mtu,
            stats.phy,
            stats.interval_us,
            link.packets,
            link.crc_fail,
            link.radio_crc_fail,
            link.bad_magic,
            link.radio_bad_magic,
            link.seq_gap,
            link.radio_seq_gap,
            link.uart_errors,
            link.radio_uart_errors,
            ble.notifications,
            ble.notified_bytes,
            ble.acl_bytes,
            ble.delivered_bytes,
            ble.link_errors,
            ble.hci_errors,
            ble.forwarded_bytes,
            ble.dropped_unsubscribed,
            ble.dropped_overflow,
            ble.dropped_writes,
            ble.untracked_acl,
            ble.heap_used,
            ble.heap_high_water,
        )?;
        f.write_str("\n#   C6 diag: ")?;
        match stats.ble_stats_age_ms {
            u32::MAX => f.write_str("no C6 stats yet")?,
            age if age > BLE_STATS_STALE_MS => write!(f, "C6 stats stale {}", Seconds(age))?,
            age => write!(f, "stats {} old", Seconds(age))?,
        }
        write!(
            f,
            "; notify {} {}, {} calls, {} returns, {} OOM retries; events {} {}, {} seen, last {} \
             {}; adv {} {}, {} connections, {} disconnects; HCI {} reads, {} writes, in flight \
             {} read {} write, {} NOCP, {} ACL outstanding, {} disconnections; runner {}, {} \
             restarts",
            ble.notify_phase,
            Named(&NOTIFY_PHASES, ble.notify_phase),
            ble.notify_raw_calls,
            ble.notify_raw_returns,
            ble.notify_oom_retries,
            ble.events_phase,
            Named(&EVENTS_PHASES, ble.events_phase),
            ble.events_seen,
            ble.last_event,
            Named(&EVENT_KINDS, ble.last_event),
            ble.adv_phase,
            Named(&ADV_PHASES, ble.adv_phase),
            ble.connections,
            ble.disconnects,
            ble.hci_reads,
            ble.hci_writes,
            ble.reads_in_flight,
            ble.writes_in_flight,
            ble.nocp_events,
            ble.acl_outstanding,
            ble.disconnection_events,
            ble.runner_phase,
            ble.runner_restarts,
        )
    }
}

/// A code's name from its table, in parentheses, or `(?)` for a code past the table.
struct Named<'a>(&'a [&'a str], u8);

impl fmt::Display for Named<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({})", self.0.get(usize::from(self.1)).unwrap_or(&"?"))
    }
}

/// Milliseconds as seconds to a tenth.
struct Seconds(u32);

impl fmt::Display for Seconds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{} s", self.0 / 1000, self.0 % 1000 / 100)
    }
}

/// Fill `body` as record `seq` carries it: a pattern from `seq`, then the [`crc32`] of the
/// pattern in the last four bytes.
///
/// # Panics
/// If `body` is shorter than the CRC.
pub fn fill_body(seq: u32, body: &mut [u8]) {
    let crc_at = body.len() - 4;
    let start = seq.wrapping_mul(31) as u8;
    for (i, byte) in body[..crc_at].iter_mut().enumerate() {
        *byte = start.wrapping_add(i as u8);
    }
    let crc = crc32(&body[..crc_at]);
    body[crc_at..].copy_from_slice(&crc.to_le_bytes());
}

/// Whether `body` ends in the [`crc32`] of the rest.
#[must_use]
pub fn body_checks(body: &[u8]) -> bool {
    let Some(crc_at) = body.len().checked_sub(4) else { return false };
    crc32(&body[..crc_at]).to_le_bytes() == body[crc_at..]
}

/// CRC-32/ISO-HDLC, the zlib and Ethernet CRC: reflected polynomial `0xEDB88320`, initial value
/// and final XOR `0xFFFFFFFF`.
#[must_use]
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in data {
        crc = (crc >> 8) ^ CRC32_TABLE[usize::from(crc as u8 ^ byte)];
    }
    !crc
}

/// [`crc32`]'s remainder for each value of the low byte, built at compile time.
static CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut crc = n as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            bit += 1;
        }
        table[n] = crc;
        n += 1;
    }
    table
};

/// The host stream bytes the C6 holds until it notifies them: a byte ring, oldest first.
#[derive(Debug)]
pub struct ByteRing<const N: usize> {
    buf: [u8; N],
    /// Where the oldest byte is.
    head: usize,
    len: usize,
}

impl<const N: usize> ByteRing<N> {
    /// Empty. Const, so firmware builds it in a static.
    #[must_use]
    pub const fn new() -> Self {
        Self { buf: [0; N], head: 0, len: 0 }
    }

    /// Appends as much of `bytes` as fits and returns how much did.
    pub fn push(&mut self, bytes: &[u8]) -> usize {
        let n = bytes.len().min(N - self.len);
        let tail = (self.head + self.len) % N;
        let first = n.min(N - tail);
        self.buf[tail..tail + first].copy_from_slice(&bytes[..first]);
        self.buf[..n - first].copy_from_slice(&bytes[first..n]);
        self.len += n;
        n
    }

    /// Moves the oldest bytes into `out`, as many as it holds, and returns how many.
    pub fn take(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        let first = n.min(N - self.head);
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        out[first..n].copy_from_slice(&self.buf[..n - first]);
        self.head = (self.head + n) % N;
        self.len -= n;
        n
    }

    /// Discards everything held.
    pub const fn clear(&mut self) {
        self.head = 0;
        self.len = 0;
    }

    /// Bytes held.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is held.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> Default for ByteRing<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// The H4 packet type of ACL data.
pub const H4_ACL: u8 = 0x02;
/// The H4 packet type of an event.
pub const H4_EVENT: u8 = 0x04;

/// `HCI_Number_Of_Completed_Packets`.
pub const NUMBER_OF_COMPLETED_PACKETS: u8 = 0x13;
/// `HCI_Disconnection_Complete`.
pub const DISCONNECTION_COMPLETE: u8 = 0x05;

/// Link-layer delivery, from the HCI traffic between a host and its controller: each outgoing
/// ACL packet's length waits here, by connection handle, until a Number Of Completed Packets
/// event names it.
///
/// A completed packet is one the peer acknowledged at the link layer, so
/// [`AclLedger::delivered_bytes`] is what reached the central, whatever the host above believes.
/// A disconnection discards its handle's packets, which the controller never completes. Packets
/// past `N` are counted in [`AclLedger::untracked`] rather than kept; the controller holds no
/// more than its own buffer count, so `N` above that never fills.
///
/// Layouts are Bluetooth Core Specification v5.3, Vol 4 Part E: ACL data in §5.4.2, Number Of
/// Completed Packets in §7.7.19 (handle and count pairs, interleaved), Disconnection Complete in
/// §7.7.5. `crate::hci` holds only the scan's packets, so none of its parsing applies.
#[derive(Debug)]
pub struct AclLedger<const N: usize> {
    /// Handle and ACL payload length of each packet the controller holds, oldest first.
    sent: heapless::Deque<(u16, u16), N>,
    acl_bytes: u32,
    delivered_bytes: u32,
    untracked: u32,
}

impl<const N: usize> AclLedger<N> {
    /// Nothing sent yet.
    #[must_use]
    pub const fn new() -> Self {
        Self { sent: heapless::Deque::new(), acl_bytes: 0, delivered_bytes: 0, untracked: 0 }
    }

    /// The host wrote an H4 packet of type `kind` whose bytes, after the type, start with
    /// `head`. Only ACL data is recorded.
    pub fn on_host_packet(&mut self, kind: u8, head: &[u8]) {
        let (H4_ACL, [lo, hi, len_lo, len_hi, ..]) = (kind, head) else { return };
        let handle = u16::from_le_bytes([*lo, *hi]) & 0x0FFF;
        let len = u16::from_le_bytes([*len_lo, *len_hi]);
        self.acl_bytes = self.acl_bytes.wrapping_add(u32::from(len));
        if self.sent.push_back((handle, len)).is_err() {
            self.untracked = self.untracked.wrapping_add(1);
        }
    }

    /// The controller sent an H4 packet of type `kind`, `packet` being its bytes after the
    /// type. Completed packets count as delivered, and a disconnection forgets its handle's.
    pub fn on_controller_packet(&mut self, kind: u8, packet: &[u8]) {
        let (H4_EVENT, [code, plen, params @ ..]) = (kind, packet) else { return };
        let params = &params[..usize::from(*plen).min(params.len())];
        match (*code, params) {
            (NUMBER_OF_COMPLETED_PACKETS, [count, pairs @ ..]) => {
                for [h_lo, h_hi, c_lo, c_hi] in
                    pairs.as_chunks::<4>().0.iter().take(usize::from(*count))
                {
                    let handle = u16::from_le_bytes([*h_lo, *h_hi]) & 0x0FFF;
                    let completed = u16::from_le_bytes([*c_lo, *c_hi]);
                    self.complete(handle, usize::from(completed));
                }
            }
            (DISCONNECTION_COMPLETE, [0, lo, hi, ..]) => {
                self.forget(u16::from_le_bytes([*lo, *hi]) & 0x0FFF);
            }
            _ => {}
        }
    }

    /// Count the oldest `completed` packets on `handle` delivered, keeping every other
    /// handle's in order.
    fn complete(&mut self, handle: u16, mut completed: usize) {
        for _ in 0..self.sent.len() {
            let Some((on, len)) = self.sent.pop_front() else { break };
            if on == handle && completed > 0 {
                completed -= 1;
                self.delivered_bytes = self.delivered_bytes.wrapping_add(u32::from(len));
            } else {
                // Room was just made by the pop.
                let _ = self.sent.push_back((on, len));
            }
        }
    }

    /// Drop every packet on `handle`.
    fn forget(&mut self, handle: u16) {
        for _ in 0..self.sent.len() {
            let Some(entry) = self.sent.pop_front() else { break };
            if entry.0 != handle {
                let _ = self.sent.push_back(entry);
            }
        }
    }

    /// ACL payload bytes the host wrote, wrapping.
    #[must_use]
    pub const fn acl_bytes(&self) -> u32 {
        self.acl_bytes
    }

    /// ACL payload bytes the controller completed, wrapping.
    #[must_use]
    pub const fn delivered_bytes(&self) -> u32 {
        self.delivered_bytes
    }

    /// Packets written while the ledger was full, wrapping.
    #[must_use]
    pub const fn untracked(&self) -> u32 {
        self.untracked
    }

    /// Packets the controller holds that the ledger tracks.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.sent.len()
    }
}

impl<const N: usize> Default for AclLedger<N> {
    fn default() -> Self {
        Self::new()
    }
}
