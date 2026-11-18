//! Enough of the Bluetooth host-controller interface to run a scan.
//!
//! A scan wants an advertiser's address, whether that address is public or random, the
//! signal strength, whether the advertisement is connectable, and the name and
//! manufacturer identifier when the advertiser sends them. A full host stack such as
//! NimBLE is a lot of code to be wrong in. `esp-radio` hands out the controller as a raw
//! HCI packet pipe, and four commands and two events cover the scan: the Command
//! Complete that says whether a command was taken, and the advertising report. This
//! module holds their byte layouts and nothing that talks to hardware; the crate docs
//! say why it lives here.
//!
//! Layouts are Bluetooth Core Specification v5.3, Vol 4 Part E — the H4
//! transport in §2, `HCI_Reset` in §7.3.2, `HCI_Set_Event_Mask` in §7.3.1,
//! `HCI_LE_Set_Scan_Parameters` in §7.8.10, `HCI_LE_Set_Scan_Enable` in §7.8.11,
//! Command Complete in §7.7.14 and the LE Advertising Report in §7.7.65.2.

use crate::bus::{RecordKind, SSID_MAX, Security, SightingMsg};

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

/// Command Complete event.
const COMMAND_COMPLETE: u8 = 0x0E;

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

/// The opcode of an H4 command packet, `None` for anything else.
///
/// The opcode is the two little-endian bytes after the packet type; a packet too short
/// to carry its parameter length is not a command.
#[must_use]
pub const fn opcode(command: &[u8]) -> Option<u16> {
    match command {
        [CMD, lo, hi, _len, ..] => Some(u16::from_le_bytes([*lo, *hi])),
        _ => None,
    }
}

/// The opcode and status a Command Complete event carries, `None` for any other
/// packet or one too short to hold a status.
///
/// A status of 0 is success; anything else is the controller refusing the command.
/// The controller announces itself at start-up with a Command Complete for opcode
/// 0x0000 and no status byte, which answers no command and reads as `None`.
#[must_use]
pub fn command_complete(packet: &[u8]) -> Option<(u16, u8)> {
    // H4 type, event code, parameter length, commands allowed, opcode, status.
    match packet {
        [EVT, COMMAND_COMPLETE, _plen, _ncmd, lo, hi, status, ..] => {
            Some((u16::from_le_bytes([*lo, *hi]), *status))
        }
        _ => None,
    }
}

/// Longest name a report carries: a 31-byte advertisement less the length and type bytes
/// of the one structure holding it.
pub const BLE_NAME_MAX: usize = 29;

const _: () = assert!(BLE_NAME_MAX <= SSID_MAX, "a BLE record carries the name as its SSID");

/// Advertising data types this module reads (Core Specification Supplement, Part A, §1).
const AD_SHORT_NAME: u8 = 0x08;
const AD_COMPLETE_NAME: u8 = 0x09;
const AD_MANUFACTURER: u8 = 0xFF;

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
    name: [u8; BLE_NAME_MAX],
    name_len: u8,
    /// The address is random, else public. From the report's address type: `0x00` and
    /// `0x02` (a resolved public identity) are public, anything else random.
    pub random: bool,
    /// The advertisement is connectable: an `ADV_IND` or `ADV_DIRECT_IND`.
    pub connectable: bool,
}

impl AdvReport {
    /// An all-zero report, for filling a buffer before anything is heard.
    pub(crate) const BLANK: Self = Self {
        address: [0; 6],
        rssi: 0,
        mfgr: None,
        name: [0; BLE_NAME_MAX],
        name_len: 0,
        random: false,
        connectable: false,
    };

    /// The advertised local name's bytes as they came: the complete name, else the
    /// shortened one. Empty when the advertisement carried neither. Not necessarily
    /// UTF-8.
    #[must_use]
    pub fn name(&self) -> &[u8] {
        &self.name[..usize::from(self.name_len)]
    }

    /// Take `other`'s name. The buffer's merge rule fills an empty name this way.
    pub(crate) fn take_name(&mut self, other: &Self) {
        self.name = other.name;
        self.name_len = other.name_len;
    }

    /// What the record's trailer says about this report.
    #[must_use]
    pub const fn trailer(&self) -> BleTrailer {
        BleTrailer { random: self.random, connectable: self.connectable, mfgr: self.mfgr }
    }

    /// Hand `f` the record this report becomes: the name as its SSID and the encoded
    /// [`BleTrailer`] as its trailer, on channel 0.
    ///
    /// The rule that turns a report into a record lives here, where the report was read,
    /// rather than at each caller. It takes a callback rather than returning a
    /// [`SightingMsg`], because the record borrows the trailer's bytes, and they live only
    /// for this call. The exporter depends on the channel being 0.
    pub fn with_msg<R>(&self, f: impl FnOnce(SightingMsg<'_>) -> R) -> R {
        let mut ext = [0; BleTrailer::MAX];
        let len = self.trailer().encode(&mut ext);
        f(SightingMsg {
            kind: RecordKind::Ble,
            bssid: self.address,
            channel: 0,
            rssi: self.rssi,
            security: Security::Ble,
            ssid: self.name(),
            ext: &ext[..len],
        })
    }

    /// Whether the controller declined to report a signal strength.
    #[must_use]
    pub const fn has_rssi(&self) -> bool {
        self.rssi != 127
    }
}

/// A BLE record's trailer: what the advertisement said beyond its address and name.
///
/// One flags byte, then the company identifier's two little-endian bytes when the
/// advertisement carried manufacturer data: 1 or 3 bytes, never 0 or 2. Bit 0 of the
/// flags is [`Self::random`] and bit 1 [`Self::connectable`]. The other bits are written
/// zero and ignored when read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BleTrailer {
    /// The address is random, else public.
    pub random: bool,
    /// The advertisement is connectable.
    pub connectable: bool,
    /// The Bluetooth SIG company identifier, when it carried manufacturer data.
    pub mfgr: Option<u16>,
}

impl BleTrailer {
    /// The longest trailer.
    pub const MAX: usize = 3;

    const RANDOM: u8 = 1 << 0;
    const CONNECTABLE: u8 = 1 << 1;

    /// Write the trailer into `out`, returning its length: 1, or 3 with an identifier.
    pub fn encode(&self, out: &mut [u8; Self::MAX]) -> usize {
        let mut flags = 0;
        if self.random {
            flags |= Self::RANDOM;
        }
        if self.connectable {
            flags |= Self::CONNECTABLE;
        }
        out[0] = flags;
        match self.mfgr {
            Some(id) => {
                out[1..].copy_from_slice(&id.to_le_bytes());
                Self::MAX
            }
            None => 1,
        }
    }

    /// Read a trailer of 1 or 3 bytes; `None` for any other length.
    #[must_use]
    pub fn decode(ext: &[u8]) -> Option<Self> {
        let (&flags, id) = ext.split_first()?;
        let mfgr = match id {
            [] => None,
            &[lo, hi] => Some(u16::from_le_bytes([lo, hi])),
            _ => return None,
        };
        Some(Self {
            random: flags & Self::RANDOM != 0,
            connectable: flags & Self::CONNECTABLE != 0,
            mfgr,
        })
    }
}

/// What a report's advertising data carries for the record.
struct AdFields<'a> {
    mfgr: Option<u16>,
    name: &'a [u8],
}

/// The company identifier and the name out of a report's advertising data.
///
/// Advertising data is a run of structures: a length that counts the type byte and the
/// payload, then the type byte, then the payload. The manufacturer-specific type is
/// `0xFF`, and its payload begins with the two little-endian bytes WiGLE's `MfgrId`
/// column wants. The name is the complete local name, else the shortened one. The first
/// structure of each type wins, and a name longer than [`BLE_NAME_MAX`] is no legacy
/// advertisement's and is not taken. A run that stops making sense ends the walk, as a
/// malformed element ends the beacon parser's, and keeps what was read before it.
fn ad_fields(data: &[u8]) -> AdFields<'_> {
    let mut mfgr = None;
    let mut complete = None;
    let mut short = None;
    let mut rest = data;
    while let Some((&len, tail)) = rest.split_first() {
        let len = usize::from(len);
        // A zero length is the padding some controllers append; anything
        // shorter than it claims is the end of the structures.
        if len == 0 || tail.len() < len {
            break;
        }
        let (kind, payload) = (tail[0], &tail[1..len]);
        match kind {
            AD_MANUFACTURER if mfgr.is_none() && payload.len() >= 2 => {
                mfgr = Some(u16::from_le_bytes([payload[0], payload[1]]));
            }
            AD_COMPLETE_NAME if complete.is_none() && payload.len() <= BLE_NAME_MAX => {
                complete = Some(payload);
            }
            AD_SHORT_NAME if short.is_none() && payload.len() <= BLE_NAME_MAX => {
                short = Some(payload);
            }
            _ => {}
        }
        rest = &tail[len..];
    }
    AdFields { mfgr, name: complete.or(short).unwrap_or(&[]) }
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
        let &[event_type, address_type, ..] = self.rest else { return None };
        let mut address = *self.rest.get(2..8)?.first_chunk::<6>()?;
        address.reverse();
        let data_len = usize::from(*self.rest.get(8)?);
        let rssi = *self.rest.get(9 + data_len)? as i8;
        let fields = ad_fields(self.rest.get(9..9 + data_len)?);
        let mut report = AdvReport {
            address,
            rssi,
            mfgr: fields.mfgr,
            // `ADV_IND` and `ADV_DIRECT_IND`; scannable and non-connectable ones are not.
            connectable: matches!(event_type, 0x00 | 0x01),
            // Public, and a public identity the controller resolved.
            random: !matches!(address_type, 0x00 | 0x02),
            ..AdvReport::BLANK
        };
        report.name[..fields.name.len()].copy_from_slice(fields.name);
        // Bounded by `BLE_NAME_MAX` where the walk took it.
        report.name_len = u8::try_from(fields.name.len()).unwrap_or(0);
        self.rest = self.rest.get(10 + data_len..)?;
        Some(report)
    }
}
