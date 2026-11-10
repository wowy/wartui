//! The Bluetooth host spike's load generator and counters: what the P4 sends when a host asks
//! for [`HostToBridge::SpikeLoad`](crate::link::HostToBridge::SpikeLoad), and how a host checks it.
//!
//! Each [`BridgeToHost::SpikeLoad`](crate::link::BridgeToHost::SpikeLoad) carries a `seq`, +1
//! per record from 0 when a host attaches, and a body whose last four bytes are a [`crc32`] over
//! the rest, little-endian. A host reads loss from the seq and corruption from the CRC.

use serde::{Deserialize, Serialize};

use crate::link::{MAX_NODE_PAYLOAD, RadioLinkStats};

/// A sighting batch's typical body, the size the rate profiles send.
pub const SMALL_BODY: usize = 45;
/// A full sighting batch's body, the size [`SpikeProfile::Saturate`] and a burst send.
pub const FULL_BODY: usize = MAX_NODE_PAYLOAD;
/// How often the P4 sends [`SpikeStats`].
pub const STATS_EVERY_MS: u64 = 2_000;

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
