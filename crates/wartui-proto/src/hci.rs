//! Enough of the Bluetooth host-controller interface to run a scan.
//!
//! All a scan wants from Bluetooth is an address, a signal strength and — if
//! the advertiser volunteered it — a manufacturer identifier, and a full
//! host stack such as NimBLE is a lot of code to be wrong in. `esp-radio` hands
//! out the controller as a raw HCI packet pipe, so a few commands and one event
//! per scan family are the whole of it, and this module is the byte layouts with
//! nothing that talks to hardware. [`BlePending`], the buffer one scan's reports
//! wait in, lives here too, so its rules run under `cargo test`.
//!
//! Layouts are Bluetooth Core Specification v5.3, Vol 4 Part E — the H4
//! transport in §2, `HCI_Reset` in §7.3.2, `HCI_Set_Event_Mask` in §7.3.1,
//! `HCI_LE_Set_Scan_Parameters` in §7.8.10, `HCI_LE_Set_Scan_Enable` in §7.8.11
//! and the LE Advertising Report in §7.7.65.2.
//!
//! # Extended scanning
//!
//! The legacy scan hears only legacy advertising PDUs on the 1M PHY. An advertiser
//! using extended advertising alone — an `AUX_ADV_IND` chain, or the Coded PHY — is
//! reported only through `HCI_LE_Set_Extended_Scan_Parameters` (§7.8.64),
//! `HCI_LE_Set_Extended_Scan_Enable` (§7.8.65) and the LE Extended Advertising
//! Report (§7.7.65.13), which also carries legacy PDUs with a flag saying so. The
//! pieces for that are here and measured on a bench before the node uses them.
//!
//! A controller runs one command family per reset: after the first legacy or
//! extended advertising or scanning command, a command from the other family is
//! refused with Command Disallowed until the next [`RESET`] (§3.1.1). Switching
//! families is therefore a reset, then both event masks again.
//!
//! What the controller supports is read from `HCI_Read_Local_Supported_Commands`
//! (§7.4.2, bit table in §6.27) and `HCI_LE_Read_Local_Supported_Features`
//! (§7.8.3, bits in Vol 6 Part B §4.6), both answered by a Command Complete
//! (§7.7.14).

use crate::air::{RecordKind, Security, SightingMsg};
use crate::dedup::Refused;

/// Largest HCI packet, so a read buffer can never be short.
///
/// One H4 type byte, a two-byte event header and up to 255 bytes of parameters.
/// `esp-radio`'s `BleConnector::next` copies a whole queued packet into the
/// buffer it is given without checking that it fits, so this is a floor rather
/// than a suggestion.
pub const PACKET_MAX: usize = 258;

const _: () = assert!(
    PACKET_MAX >= 1 + 2 + 255,
    "an HCI read buffer shorter than the largest event is a panic, not a truncation"
);

/// H4 packet types.
const CMD: u8 = 0x01;
const EVT: u8 = 0x04;

/// `HCI_LE_Meta` event, and the advertising-report subevents inside it.
const LE_META: u8 = 0x3E;
const ADV_REPORT: u8 = 0x02;
const EXT_ADV_REPORT: u8 = 0x0D;

/// `HCI_Command_Complete` event.
const COMMAND_COMPLETE: u8 = 0x0E;

/// Scan interval and window are counted in units of 625 microseconds.
pub const SCAN_UNIT_US: u32 = 625;

/// `HCI_Reset`. Sent once, because the controller comes up in whatever state
/// the last run left it and a scan enabled twice is an error.
pub const RESET: [u8; 4] = [CMD, 0x03, 0x0C, 0x00];

/// `HCI_Set_Event_Mask`, with the LE Meta Event bit set.
///
/// Without this a scan is enabled, acknowledged, and reports nothing: an advertising
/// report is an LE Meta Event, which is bit 61, and [`RESET`] restores the
/// specification's default mask with that bit clear. The value is that default plus
/// bit 61 rather than all ones, because events nothing here drains cost queue space
/// in the controller.
pub const SET_EVENT_MASK: [u8; 12] =
    [CMD, 0x01, 0x0C, 0x08, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x1F, 0x00, 0x20];

/// `HCI_LE_Set_Scan_Parameters`, passive.
///
/// Passive rather than active: an active scan transmits to pull back a scan response
/// whose every field is thrown away before the wire, so it buys nothing and not
/// transmitting is the point of the firmware.
///
/// `interval` and `window` are in [`SCAN_UNIT_US`] units; equal values mean the
/// radio listens continuously while scanning is enabled.
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
/// Duplicate filtering is left off: the controller's filter is a small fixed table
/// that would silently compete with [`crate::dedup::MacRing`] for the same job.
#[must_use]
pub const fn set_scan_enable(enable: bool) -> [u8; 6] {
    [CMD, 0x0C, 0x20, 0x02, enable as u8, 0x00]
}

/// `HCI_LE_Set_Event_Mask`, with the Extended Advertising Report bit set.
///
/// The extended counterpart of [`SET_EVENT_MASK`]'s trap: [`RESET`] restores an LE
/// mask of `0x1F`, which reports subevents 0x01 to 0x05 and hides 0x0D, so an
/// extended scan is enabled, acknowledged and silent. The value is that default
/// plus bit 12, the bit for subevent 0x0D. Sent after [`SET_EVENT_MASK`], which
/// is what lets any LE Meta Event through at all.
pub const SET_LE_EVENT_MASK: [u8; 12] =
    [CMD, 0x01, 0x20, 0x08, 0x1F, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];

/// `HCI_Read_Local_Supported_Commands`. [`ext_scan_commands`] reads the answer.
pub const READ_LOCAL_SUPPORTED_COMMANDS: [u8; 4] = [CMD, 0x02, 0x10, 0x00];

/// `HCI_LE_Read_Local_Supported_Features`. [`le_features`] reads the answer.
pub const LE_READ_LOCAL_SUPPORTED_FEATURES: [u8; 4] = [CMD, 0x03, 0x20, 0x00];

/// The opcode and status of a Command Complete event, or `None` for any other
/// packet.
///
/// A Command Complete with no return parameters — the controller announcing
/// free command slots under opcode 0 — has no status, and is `None` too.
#[must_use]
pub fn command_complete(packet: &[u8]) -> Option<(u16, u8)> {
    // H4 type, event code, parameter length, command slots, opcode, status.
    let &[EVT, COMMAND_COMPLETE, _plen, _slots, lo, hi, status] = packet.get(..7)? else {
        return None;
    };
    Some((u16::from_le_bytes([lo, hi]), status))
}

/// The return parameters of a successful Command Complete for `opcode`.
fn returned(packet: &[u8], opcode: u16) -> Option<&[u8]> {
    match command_complete(packet)? {
        (op, 0x00) if op == opcode => packet.get(7..),
        _ => None,
    }
}

/// Whether the controller lists the two extended scan commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtScanCommands {
    /// `HCI_LE_Set_Extended_Scan_Parameters`.
    pub parameters: bool,
    /// `HCI_LE_Set_Extended_Scan_Enable`.
    pub enable: bool,
}

/// The extended scan commands out of the Command Complete for
/// [`READ_LOCAL_SUPPORTED_COMMANDS`], or `None` for any other packet, a failed
/// read or an answer too short to hold them.
///
/// Both sit in octet 37 of the supported-commands table, at bits 5 and 6.
#[must_use]
pub fn ext_scan_commands(packet: &[u8]) -> Option<ExtScanCommands> {
    let octet = *returned(packet, 0x1002)?.get(37)?;
    Some(ExtScanCommands { parameters: octet & (1 << 5) != 0, enable: octet & (1 << 6) != 0 })
}

/// The controller's LE feature bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeFeatures(pub u64);

impl LeFeatures {
    /// LE Coded PHY, bit 11: whether a scan can listen on the Coded PHY at all.
    #[must_use]
    pub const fn coded_phy(self) -> bool {
        self.0 & (1 << 11) != 0
    }

    /// LE Extended Advertising, bit 12: whether the extended scan commands mean
    /// anything on this controller.
    #[must_use]
    pub const fn extended_advertising(self) -> bool {
        self.0 & (1 << 12) != 0
    }
}

/// The feature bits out of the Command Complete for
/// [`LE_READ_LOCAL_SUPPORTED_FEATURES`], or `None` for any other packet, a
/// failed read or a short answer.
#[must_use]
pub fn le_features(packet: &[u8]) -> Option<LeFeatures> {
    let bits = *returned(packet, 0x2003)?.first_chunk::<8>()?;
    Some(LeFeatures(u64::from_le_bytes(bits)))
}

/// Which PHYs an extended scan listens on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanPhys {
    /// LE 1M only, which is where every legacy advertiser is.
    OneM,
    /// LE Coded only.
    Coded,
    /// Both. The controller divides its listening time between them.
    OneMAndCoded,
}

impl ScanPhys {
    /// The `Scanning_PHYs` bitmask: bit 0 is 1M, bit 2 is Coded.
    #[must_use]
    pub const fn mask(self) -> u8 {
        match self {
            Self::OneM => 0b001,
            Self::Coded => 0b100,
            Self::OneMAndCoded => 0b101,
        }
    }

    /// How many PHYs, and so how many parameter blocks the command carries.
    #[must_use]
    pub const fn count(self) -> usize {
        self.mask().count_ones() as usize
    }
}

/// Longest `HCI_LE_Set_Extended_Scan_Parameters`: header, three fixed
/// parameters and two five-byte PHY blocks.
const EXT_SCAN_PARAMETERS_MAX: usize = 4 + 3 + 2 * 5;

/// An `HCI_LE_Set_Extended_Scan_Parameters` command, whose length depends on how
/// many PHYs it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtScanParameters {
    bytes: [u8; EXT_SCAN_PARAMETERS_MAX],
    len: usize,
}

impl ExtScanParameters {
    /// The command as it goes to the controller.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8] {
        self.bytes.split_at(self.len).0
    }
}

/// `HCI_LE_Set_Extended_Scan_Parameters`, passive, public own address, accepting
/// everything: the settings of [`set_scan_parameters`], for the reasons it gives.
///
/// Every PHY in `phys` gets the same `interval` and `window`, in [`SCAN_UNIT_US`]
/// units, in the order the bitmask names them: 1M, then Coded.
#[must_use]
pub const fn set_ext_scan_parameters(
    phys: ScanPhys,
    interval: u16,
    window: u16,
) -> ExtScanParameters {
    let [ilo, ihi] = interval.to_le_bytes();
    let [wlo, whi] = window.to_le_bytes();
    let params = 3 + 5 * phys.count();
    let mut bytes = [0u8; EXT_SCAN_PARAMETERS_MAX];
    bytes[0] = CMD;
    bytes[1] = 0x41;
    bytes[2] = 0x20;
    bytes[3] = params as u8;
    bytes[4] = 0x00; // own address type: public
    bytes[5] = 0x00; // filter policy: accept everything
    bytes[6] = phys.mask();
    let mut at = 7;
    while at < 4 + params {
        bytes[at] = 0x00; // scan type: passive
        bytes[at + 1] = ilo;
        bytes[at + 2] = ihi;
        bytes[at + 3] = wlo;
        bytes[at + 4] = whi;
        at += 5;
    }
    ExtScanParameters { bytes, len: 4 + params }
}

/// `HCI_LE_Set_Extended_Scan_Enable`.
///
/// Duplicate filtering off, for the reason [`set_scan_enable`] gives. Duration
/// and period zero: the scan runs until disabled, which is how the caller bounds it.
#[must_use]
pub const fn set_ext_scan_enable(enable: bool) -> [u8; 10] {
    [CMD, 0x42, 0x20, 0x06, enable as u8, 0x00, 0x00, 0x00, 0x00, 0x00]
}

/// One advertiser, heard once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdvReport {
    /// The advertiser's address, in the order it is written and displayed.
    /// HCI carries it least-significant byte first; this is reversed already.
    pub address: [u8; 6],
    /// Signal strength in dBm. `127` means the controller had no reading, which
    /// the Core Specification defines and which is not a plausible dBm value.
    pub rssi: i8,
    /// The Bluetooth SIG company identifier from the advertiser's
    /// manufacturer-specific data, if it carried any. `None` when it did not —
    /// which is common, and not a fault.
    pub mfgr: Option<u16>,
}

impl AdvReport {
    /// The observation in the shape the wire carries, with `ext` as the
    /// trailer — for a BLE sighting, the company identifier's two
    /// little-endian bytes or nothing.
    ///
    /// BLE records have no SSID and no channel, and the exporter depends on
    /// both being empty and zero rather than absent.
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
    /// The company identifier is the trailer, so the rule that turns one into
    /// the other lives here — once, where the identifier was read — rather
    /// than at each caller. A callback rather than a returned [`SightingMsg`]
    /// because the identifier's two bytes need somewhere to live for the
    /// borrow, and that somewhere cannot outlive this call.
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
/// Advertising data is a run of structures — a length that counts the type
/// byte and the payload, then that type byte, then the payload — and the
/// manufacturer-specific type is `0xFF`, whose payload begins with the two
/// little-endian bytes WiGLE's `MfgrId` column wants. First such structure
/// wins; a run that stops making sense ends the walk with nothing, which is
/// the same deal the beacon parser gives a malformed element.
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
/// controller says — command completions, unknown events, the lot.
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

        // Event type, address type, address, advertising data whose length is
        // declared inline, then RSSI. Reports are laid out one after another rather
        // than as parallel arrays per parameter: the specification's table reads
        // either way, and every controller and host stack agrees on this reading.
        let mut address = *self.rest.get(2..8)?.first_chunk::<6>()?;
        address.reverse();
        let data_len = usize::from(*self.rest.get(8)?);
        let rssi = *self.rest.get(9 + data_len)? as i8;
        let mfgr = self.rest.get(9..9 + data_len).and_then(manufacturer_id);
        self.rest = self.rest.get(10 + data_len..)?;
        Some(AdvReport { address, rssi, mfgr })
    }
}

/// `ExtAdvReport::primary_phy` and `secondary_phy` values.
pub const PHY_1M: u8 = 0x01;
/// LE 2M, which is only ever a secondary PHY.
pub const PHY_2M: u8 = 0x02;
/// LE Coded.
pub const PHY_CODED: u8 = 0x03;

/// The address type an extended report gives an advertiser that sent no address.
pub const ANONYMOUS: u8 = 0xFF;

/// Whether an extended report carries all of the advertiser's data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataStatus {
    /// The whole of it.
    Complete,
    /// A fragment; another report continues it.
    MoreToCome,
    /// Some of it, and the controller has stopped following the chain.
    Truncated,
    /// The value the specification reserves.
    Reserved,
}

/// One advertiser, heard once, through an extended scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtAdvReport {
    /// The advertiser's address, reversed into display order as
    /// [`AdvReport::address`] is. Meaningless when anonymous.
    pub address: [u8; 6],
    /// The address type; [`ANONYMOUS`] when the advertiser sent none.
    pub address_type: u8,
    /// Whether this came from a legacy advertising PDU, which a legacy scan also hears.
    pub legacy: bool,
    /// [`PHY_1M`] or [`PHY_CODED`].
    pub primary_phy: u8,
    /// The PHY of the auxiliary chain, or 0 when there is none.
    pub secondary_phy: u8,
    /// Whether the data is whole.
    pub data_status: DataStatus,
    /// Signal strength in dBm, with `127` for no reading as in [`AdvReport::rssi`].
    pub rssi: i8,
    /// The manufacturer company identifier in this report's data, if any. A
    /// fragment carries part of the data, so an identifier in a later fragment is
    /// found only in that fragment's report.
    pub mfgr: Option<u16>,
}

impl ExtAdvReport {
    /// Whether the advertiser sent no address.
    #[must_use]
    pub const fn is_anonymous(&self) -> bool {
        self.address_type == ANONYMOUS
    }

    /// Whether the controller declined to report a signal strength.
    #[must_use]
    pub const fn has_rssi(&self) -> bool {
        self.rssi != 127
    }
}

/// Walk the extended advertising reports in one HCI packet.
///
/// Yields nothing for any other packet, legacy advertising reports included, so a
/// caller can hand it everything the controller says.
#[must_use]
pub fn ext_adv_reports(packet: &[u8]) -> ExtAdvReports<'_> {
    let empty = ExtAdvReports { rest: &[], remaining: 0 };
    let Some(&[EVT, LE_META, _plen, EXT_ADV_REPORT, count]) = packet.get(..5) else {
        return empty;
    };
    ExtAdvReports { rest: &packet[5..], remaining: count }
}

/// The iterator [`ext_adv_reports`] returns.
#[derive(Debug, Clone)]
pub struct ExtAdvReports<'a> {
    rest: &'a [u8],
    remaining: u8,
}

/// Bytes of an extended report ahead of its data.
const EXT_REPORT_FIXED: usize = 24;

impl Iterator for ExtAdvReports<'_> {
    type Item = ExtAdvReport;

    fn next(&mut self) -> Option<ExtAdvReport> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        // Event type (2), address type, address (6), primary PHY, secondary PHY,
        // SID, Tx power, RSSI, periodic interval (2), direct address type, direct
        // address (6), data length, data. Laid out one report after another, as
        // `AdvReports` reads the legacy event.
        let fixed = self.rest.first_chunk::<EXT_REPORT_FIXED>()?;
        let event_type = u16::from_le_bytes([fixed[0], fixed[1]]);
        let mut address = [fixed[3], fixed[4], fixed[5], fixed[6], fixed[7], fixed[8]];
        address.reverse();
        let data_len = usize::from(fixed[23]);
        let data = self.rest.get(EXT_REPORT_FIXED..EXT_REPORT_FIXED + data_len)?;
        let report = ExtAdvReport {
            address,
            address_type: fixed[2],
            legacy: event_type & (1 << 4) != 0,
            primary_phy: fixed[9],
            secondary_phy: fixed[10],
            data_status: match (event_type >> 5) & 0b11 {
                0 => DataStatus::Complete,
                1 => DataStatus::MoreToCome,
                2 => DataStatus::Truncated,
                _ => DataStatus::Reserved,
            },
            rssi: fixed[13] as i8,
            mfgr: manufacturer_id(data),
        };
        self.rest = &self.rest[EXT_REPORT_FIXED + data_len..];
        Some(report)
    }
}

/// The reports of one scan, one per address, waiting for the main loop to drain them.
///
/// It never wraps: a full buffer turns the newest address away and counts it in
/// [`Self::dropped`], once per scan, the same policy the Wi-Fi sightings follow.
///
/// Slots go only to addresses the caller says are due to be reported. An
/// advertiser the host already has is heard again on every scan; given a slot,
/// it is thrown away by the drain, and a crowded room fills the buffer with
/// such repeats and turns new advertisers away.
#[derive(Debug, Clone)]
pub struct BlePending<const N: usize> {
    items: [AdvReport; N],
    len: usize,
    taken: usize,
    dropped: Refused,
}

impl<const N: usize> Default for BlePending<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> BlePending<N> {
    /// An empty buffer, `const` so it can sit in a `static`.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            items: [AdvReport { address: [0; 6], rssi: 0, mfgr: None }; N],
            len: 0,
            taken: 0,
            dropped: Refused::new(),
        }
    }

    /// Keep `report` if its address is new and `due`, or merge it into the
    /// reading already held for that address.
    ///
    /// A held address merges unconditionally, without asking `due`: the
    /// identifier can arrive in a later packet than the first hearing, and an
    /// advertiser that led with its flags and followed with its manufacturer
    /// data is still the one advertiser. `due` is asked only when the report
    /// would take a new slot, and before the buffer is checked for room, so a
    /// report that is not due neither takes a slot nor counts as dropped.
    pub fn record(&mut self, report: AdvReport, due: impl FnOnce(&AdvReport) -> bool) {
        if !report.has_rssi() {
            return;
        }
        if let Some(held) = self.items[..self.len].iter_mut().find(|r| r.address == report.address)
        {
            held.rssi = held.rssi.max(report.rssi);
            if held.mfgr.is_none() {
                held.mfgr = report.mfgr;
            }
            return;
        }
        if !due(&report) {
            return;
        }
        if self.len == N {
            self.dropped.note(&report.address);
            return;
        }
        self.items[self.len] = report;
        self.len += 1;
    }

    /// Take the oldest report not yet taken, if there is one.
    ///
    /// The bound is `len` and not the array: the slots past `len` are an
    /// earlier scan's leavings or the zero fill — a `00:00:00:00:00:00`
    /// advertiser at 0 dBm that would go on the air as if heard.
    pub fn take(&mut self) -> Option<AdvReport> {
        if self.taken >= self.len {
            return None;
        }
        let report = self.items[self.taken];
        self.taken += 1;
        Some(report)
    }

    /// Empty the buffer for a new scan. [`Self::dropped`] carries on, and an
    /// advertiser turned away last scan counts again if it is turned away in this one.
    pub fn clear(&mut self) {
        self.len = 0;
        self.taken = 0;
        self.dropped.reset();
    }

    /// Distinct addresses held this scan, at most `N`.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether this scan holds nothing.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Advertisers turned away by a full buffer since construction, each counted
    /// once per scan however often it repeats. Wraps. See [`Refused`] for how it
    /// slightly undercounts.
    #[must_use]
    pub const fn dropped(&self) -> u16 {
        self.dropped.total()
    }
}
