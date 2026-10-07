//! Enough of the Bluetooth host-controller interface to run a scan.
//!
//! A scan wants three things from Bluetooth: an address, a signal strength, and a
//! manufacturer identifier when the advertiser sends one. A full host stack such as
//! NimBLE is a lot of code to be wrong in. `esp-radio` hands out the controller as a raw
//! HCI packet pipe, and four commands and one event cover the scan. This module holds
//! their byte layouts and nothing that talks to hardware; the crate docs say why it
//! lives here.
//!
//! Layouts are Bluetooth Core Specification v5.3, Vol 4 Part E — the H4
//! transport in §2, `HCI_Reset` in §7.3.2, `HCI_Set_Event_Mask` in §7.3.1,
//! `HCI_LE_Set_Scan_Parameters` in §7.8.10, `HCI_LE_Set_Scan_Enable` in §7.8.11
//! and the LE Advertising Report in §7.7.65.2.

use crate::air::{RecordKind, Security, SightingMsg};

/// Largest HCI packet, so a read buffer can never be short.
///
/// One H4 type byte, a two-byte event header and up to 255 bytes of parameters.
/// `esp-radio`'s `BleConnector::next` copies a whole queued packet into the buffer it
/// is given without checking that it fits, so this is a floor, not a suggestion.
pub const PACKET_MAX: usize = 258;

const _: () = assert!(
    PACKET_MAX >= 1 + 2 + 255,
    "an HCI read buffer shorter than the largest event is a panic, not a truncation"
);

/// H4 packet types.
const CMD: u8 = 0x01;
const EVT: u8 = 0x04;

/// `HCI_LE_Meta` event, and the advertising-report subevent inside it.
const LE_META: u8 = 0x3E;
const ADV_REPORT: u8 = 0x02;

/// Scan interval and window are counted in units of 625 microseconds.
pub const SCAN_UNIT_US: u32 = 625;

/// `HCI_Reset`. Sent once, because the controller comes up in whatever state the last
/// run left it, and enabling a scan twice is an error.
pub const RESET: [u8; 4] = [CMD, 0x03, 0x0C, 0x00];

/// `HCI_Set_Event_Mask`, with the LE Meta Event bit set.
///
/// Without this a scan is enabled, acknowledged, and reports nothing. An advertising
/// report is an LE Meta Event, bit 61, and [`RESET`] restores the specification's
/// default mask with that bit clear. The value is that default plus bit 61, not all
/// ones: events nothing here drains cost queue space in the controller.
pub const SET_EVENT_MASK: [u8; 12] =
    [CMD, 0x01, 0x0C, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x1F, 0x00, 0x20];

/// `HCI_LE_Set_Scan_Parameters`, passive.
///
/// Passive, because not transmitting is the point of the firmware. An active scan
/// transmits to pull back a scan response, and every field of that response is thrown
/// away before the wire.
///
/// `interval` and `window` are in [`SCAN_UNIT_US`] units. Equal values mean the radio
/// listens continuously while scanning is enabled.
#[must_use]
pub const fn set_scan_parameters(interval: u16, window: u16) -> [u8; 11] {
    let [ilo, ihi] = interval.to_le_bytes();
    let [wlo, whi] = window.to_le_bytes();
    [
        CMD, 0x0B, 0x20, 0x07, // opcode 0x200B, seven parameter bytes
        0x00, // scan type: passive
        ilo, ihi, wlo, whi,  //
        0x00, // own address type: public
        0x00, // filter policy: accept everything
    ]
}

/// `HCI_LE_Set_Scan_Enable`.
///
/// Duplicate filtering is off. The controller's filter is a small fixed table that
/// would silently compete with [`crate::dedup::MacRing`] for the same job.
#[must_use]
pub const fn set_scan_enable(enable: bool) -> [u8; 6] {
    [CMD, 0x0C, 0x20, 0x02, enable as u8, 0x00]
}

/// One advertiser, heard once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvReport {
    /// The advertiser's address, in the order it is written and displayed. HCI
    /// carries it least-significant byte first, and this is already reversed.
    pub address: [u8; 6],
    /// Signal strength in dBm. The Core Specification defines `127` as no reading,
    /// which is not a plausible dBm value.
    pub rssi: i8,
    /// The Bluetooth SIG company identifier from the advertiser's
    /// manufacturer-specific data. `None` when it carried none, which is common and
    /// not a fault.
    pub mfgr: Option<u16>,
}

impl AdvReport {
    /// The sighting as the record the wire carries, with `ext` as the trailer: the
    /// company identifier's two little-endian bytes, or nothing.
    ///
    /// A BLE record has an empty SSID and channel 0. The exporter depends on both.
    #[must_use]
    pub const fn as_msg<'a>(&self, ext: &'a [u8]) -> SightingMsg<'a> {
        SightingMsg {
            kind: RecordKind::Ble,
            bssid: self.address,
            channel: 0,
            rssi: self.rssi,
            security: Security::Ble,
            ssid: b"",
            ext,
        }
    }

    /// Hand `f` the record this report becomes, trailer and all.
    ///
    /// The company identifier is the trailer. The rule that turns one into the other
    /// lives here, where the identifier was read, rather than at each caller. It takes
    /// a callback rather than returning a [`SightingMsg`], because the record borrows
    /// the identifier's two bytes, and they live only for this call.
    pub fn with_msg<R>(&self, f: impl FnOnce(SightingMsg<'_>) -> R) -> R {
        match self.mfgr {
            Some(id) => f(self.as_msg(&id.to_le_bytes())),
            None => f(self.as_msg(&[])),
        }
    }

    /// Whether the controller declined to report a signal strength.
    #[must_use]
    pub const fn has_rssi(&self) -> bool {
        self.rssi != 127
    }
}

/// The company identifier out of a report's advertising data, if it holds
/// manufacturer-specific data.
///
/// Advertising data is a run of structures: a length that counts the type byte and the
/// payload, then the type byte, then the payload. The manufacturer-specific type is
/// `0xFF`, and its payload begins with the two little-endian bytes WiGLE's `MfgrId`
/// column wants. The first such structure wins. A run that stops making sense ends the
/// walk with nothing, as a malformed element ends the beacon parser's.
fn manufacturer_id(data: &[u8]) -> Option<u16> {
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let len = usize::from(len);
        // A zero length is the padding some controllers append; anything
        // shorter than it claims is the end of the structures.
        if len == 0 || tail.len() < len {
            return None;
        }
        if tail[0] == 0xFF && len >= 3 {
            return Some(u16::from_le_bytes([tail[1], tail[2]]));
        }
        rest = tail.get(len..)?;
    }
    None
}

/// Walk the advertising reports in one HCI packet.
///
/// Yields nothing for any other packet, so a caller can hand it everything the
/// controller says: command completions, unknown events and all.
#[must_use]
pub fn adv_reports(packet: &[u8]) -> AdvReports<'_> {
    let empty = AdvReports { rest: &[], remaining: 0 };
    // H4 type, event code, parameter length, subevent, report count.
    let Some(&[EVT, LE_META, _plen, ADV_REPORT, count]) = packet.get(..5) else { return empty };
    AdvReports { rest: &packet[5..], remaining: count }
}

/// The iterator [`adv_reports`] returns.
#[derive(Debug, Clone)]
pub struct AdvReports<'a> {
    rest: &'a [u8],
    remaining: u8,
}

impl Iterator for AdvReports<'_> {
    type Item = AdvReport;

    fn next(&mut self) -> Option<AdvReport> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        // Event type, address type, address, advertising data with its length
        // inline, then RSSI. Reports are laid out one after another, not as parallel
        // arrays per parameter. The specification's table reads either way, and every
        // controller and host stack agrees on this reading.
        let mut address = *self.rest.get(2..8)?.first_chunk::<6>()?;
        address.reverse();
        let data_len = usize::from(*self.rest.get(8)?);
        let rssi = *self.rest.get(9 + data_len)? as i8;
        let mfgr = self.rest.get(9..9 + data_len).and_then(manufacturer_id);
        self.rest = self.rest.get(10 + data_len..)?;
        Some(AdvReport { address, rssi, mfgr })
    }
}
