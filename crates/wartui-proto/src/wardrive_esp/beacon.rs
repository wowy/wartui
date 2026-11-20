//! The WardriveESP status beacon and advert extras: what a phone in range reads from the bridge's
//! advertisements without linking.
//!
//! The P4 holds what they say, in a [`Beacon`] it sends the C6 on each change and every 5 s; the
//! C6 encodes it into its advertisement and scan response while it admits phones.
//!
//! **The status** is manufacturer data under [`COMPANY`] ([`status`]). The free board's scan
//! response carries its fixed 9 bytes between the name and an empty
//! [`ID_UUID`](crate::ble::ID_UUID) service data, which tells a wartui host to read the bridge's
//! octets from the status ([`bridge_octets`]). While a central is connected the scan response
//! carries the status with the app's nickname, then the empty id when the nickname leaves room
//! (14 bytes at most). **The extras** are 6 bytes of service data under [`EXTRAS_UUID`] in the
//! advertisement, in two frames swapped every 5 s ([`extras`]).
//!
//! **The fields the bridge has no figure for are fixed**: the temperature is unknown, the board is
//! generic, there is no battery gauge, screen or co-processor, and the USB, GPS and updating bits
//! are clear, since the phone never updates the bridge. The mode bits say Wi-Fi or Bluetooth, the
//! session's mode; the bridge offers no other. The level bytes are the protocol level `WD:CAPS`
//! names ([`FW_LEVEL`]).
//!
//! **The channel bits index the document's table** ([`EXTRAS_CHANNELS`]), not the scan table, so a
//! change to the scan table cannot move them.

use serde::{Deserialize, Serialize};

use super::command::NICKNAME_MAX;
use super::line::FW_LEVEL;

/// The status's company id.
pub const COMPANY: u16 = 0xFFFF;

/// The extras' 16-bit service UUID.
pub const EXTRAS_UUID: u16 = 0x5744;

/// The longest status: the fixed bytes and a whole nickname.
pub const STATUS_MAX: usize = 9 + NICKNAME_MAX;

/// Bytes in one extras frame.
pub const EXTRAS_LEN: usize = 6;

/// The channels the extras' bits name, bit `i` for entry `i`.
pub const EXTRAS_CHANNELS: [u8; 42] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, //
    36, 40, 44, 48, 52, 56, 60, 64, //
    100, 104, 108, 112, 116, 120, 124, 128, 132, 136, 140, 144, //
    149, 153, 157, 161, 165, 169, 173, 177,
];

/// The status's first byte.
const MARK: u8 = b'W';

/// Where the bridge's last two octets sit in the status.
const BRIDGE_AT: usize = 7;

/// The status's temperature byte: unknown.
const TEMPERATURE_UNKNOWN: u8 = 0x80;

/// The extras' board byte: generic.
const BOARD_GENERIC: u8 = 0;

/// The extras' battery byte: no gauge.
const NO_BATTERY: u8 = 0xFF;

/// What the P4 knows that the beacon says. Rides `Down::Beacon`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Beacon {
    /// The phone's session scans.
    pub scanning: bool,
    /// The phone's session is in Bluetooth mode, else Wi-Fi.
    pub bluetooth: bool,
    /// `WD:AP` or `WD:BT` lines in the last finished period.
    pub count: u16,
    /// The app said it is an iPhone.
    pub iphone: bool,
    /// The app's nickname; empty when it gave none.
    pub nickname: heapless::Vec<u8, NICKNAME_MAX>,
    /// How the last phone link ended.
    pub link_end: LinkEnd,
    /// The channel bits, in the extras' order.
    pub channels: u64,
}

/// How the last phone link ended, as the extras say it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LinkEnd {
    /// No phone has linked since the P4 booted.
    #[default]
    None = 0,
    /// The app sent `bye` before it let go.
    Released = 1,
    /// Any other end.
    Lost = 2,
}

/// The status bytes (after the company id) into `out`, returning their length: 9, or 9 and the
/// nickname while `connected`. `bridge` is the bridge's last two address octets.
pub fn status(
    out: &mut [u8; STATUS_MAX],
    beacon: &Beacon,
    bridge: [u8; 2],
    connected: bool,
) -> usize {
    // Bits 2-3 are the mode: 0 Wi-Fi, 1 Bluetooth.
    let flags = u8::from(connected)
        | u8::from(beacon.scanning) << 1
        | u8::from(beacon.bluetooth) << 2
        | u8::from(connected && beacon.iphone) << 7;
    let [count_lo, count_hi] = beacon.count.to_le_bytes();
    let (major, minor, patch) = FW_LEVEL;
    let fixed = [
        MARK,
        flags,
        TEMPERATURE_UNKNOWN,
        count_lo,
        count_hi,
        major << 4 | minor,
        patch,
        bridge[0],
        bridge[1],
    ];
    out[..fixed.len()].copy_from_slice(&fixed);
    if !connected {
        return fixed.len();
    }
    let nickname = &beacon.nickname;
    out[fixed.len()..fixed.len() + nickname.len()].copy_from_slice(nickname);
    fixed.len() + nickname.len()
}

/// The bridge's last two octets from status bytes (after the company id), or `None` for bytes
/// that are not a status: shorter than its fixed bytes, or not led by its mark.
#[must_use]
pub fn bridge_octets(status: &[u8]) -> Option<[u8; 2]> {
    if status.first() != Some(&MARK) {
        return None;
    }
    let octets = status.get(BRIDGE_AT..BRIDGE_AT + 2)?;
    Some([octets[0], octets[1]])
}

/// One extras frame (after the UUID): frame B when `second`.
#[must_use]
pub fn extras(beacon: &Beacon, second: bool) -> [u8; EXTRAS_LEN] {
    let head = u8::from(second) | (beacon.link_end as u8) << 1;
    let bits = beacon.channels.to_le_bytes();
    if second {
        [head, bits[3], bits[4], bits[5] & 0b11, 0, 0]
    } else {
        [head, BOARD_GENERIC, NO_BATTERY, bits[0], bits[1], bits[2]]
    }
}

/// The channel bits for `channels`. A channel outside the extras' table sets none.
#[must_use]
pub fn channel_bits(channels: &[u8]) -> u64 {
    channels.iter().fold(0, |bits, &channel| {
        EXTRAS_CHANNELS.iter().position(|&listed| listed == channel).map_or(bits, |i| bits | 1 << i)
    })
}
