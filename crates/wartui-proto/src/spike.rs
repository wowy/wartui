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

use core::fmt;

use serde::{Deserialize, Serialize};

use crate::link::{MAX_NODE_PAYLOAD, RadioLinkStats};

/// A sighting batch's typical body, the size the rate profiles send.
pub const SMALL_BODY: usize = 45;
/// A full sighting batch's body, the size [`SpikeProfile::Saturate`] and a burst send.
pub const FULL_BODY: usize = MAX_NODE_PAYLOAD;
/// How often the P4 sends [`SpikeStats`].
pub const STATS_EVERY_MS: u64 = 2_000;

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

/// The P4's counters for the spike, every [`STATS_EVERY_MS`] while a profile runs or Bluetooth
/// owns the host queue, and always in a `sink-discard` build. Byte counts are since the P4
/// booted, and wrap.
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
    /// Bytes the C6's link layer reported delivered, from its latest `BleStats`.
    pub delivered_bytes: u32,
    /// The bulk ring's high-water mark, in bytes.
    pub bulk_high_water: u32,
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
    /// The counters as three `#` lines, with the rate the C6 link carried while Bluetooth
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
        write!(
            f,
            "\n#   queued {} B, sent {} B, credited {} B, delivered {} B; bulk high-water {} B, \
             {} refused, {} evicted\n\
             #   mtu {}, phy {}, interval {} us; C6 link {} packets in, crc {}/{}, magic {}/{}, \
             gap {}/{}, uart {}/{} (P4/C6)",
            stats.queued_bytes,
            stats.sent_bytes,
            stats.credited_bytes,
            stats.delivered_bytes,
            stats.bulk_high_water,
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
        )
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
