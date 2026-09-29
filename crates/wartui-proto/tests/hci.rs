//! The commands and events a BLE scan is made of, legacy and extended.

use wartui_proto::air::{RecordKind, SIGHTING_RECORD_MAX, Security, SightingMsg};
use wartui_proto::hci::{
    ANONYMOUS, AdvReport, BlePending, DataStatus, ExtScanCommands,
    LE_READ_LOCAL_SUPPORTED_FEATURES, PHY_1M, PHY_2M, PHY_CODED, READ_LOCAL_SUPPORTED_COMMANDS,
    RESET, SET_EVENT_MASK, SET_LE_EVENT_MASK, ScanPhys, adv_reports, command_complete,
    ext_adv_reports, ext_scan_commands, le_features, set_ext_scan_enable, set_ext_scan_parameters,
    set_scan_enable, set_scan_parameters,
};

/// An LE Advertising Report event carrying `reports` of `(address, data, rssi)`.
///
/// The address goes in least-significant byte first, which is the detail most
/// worth having a test for: a reversed MAC is a plausible-looking address that
/// is simply the wrong device.
fn event(reports: &[([u8; 6], &[u8], i8)]) -> Vec<u8> {
    let mut params = vec![0x02, u8::try_from(reports.len()).expect("few reports")];
    for (address, data, rssi) in reports {
        params.push(0x00); // event type: connectable undirected
        params.push(0x01); // address type: random
        params.extend(address.iter().rev());
        params.push(u8::try_from(data.len()).expect("short data"));
        params.extend_from_slice(data);
        params.push(*rssi as u8);
    }
    let mut packet = vec![0x04, 0x3E, u8::try_from(params.len()).expect("fits")];
    packet.extend_from_slice(&params);
    packet
}

const ADDR: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

#[test]
fn hci_command_encodes_reset_command_when_constant_is_evaluated() {
    // H4 command, opcode 0x0C03 little-endian, no parameters.
    assert_eq!(RESET, [0x01, 0x03, 0x0C, 0x00]);
}

#[test]
fn hci_command_sets_le_meta_event_bit_when_constructing_event_mask() {
    // Measured on hardware: without this the controller accepts every command
    // with `status 0` and delivers not one advertising report, because a reset
    // restores the specification's default mask and an advertising report is an
    // LE Meta Event -- bit 61, which that default leaves clear.
    assert_eq!(SET_EVENT_MASK[..4], [0x01, 0x01, 0x0C, 0x08], "opcode 0x0C01, eight bytes");

    let mask = u64::from_le_bytes(SET_EVENT_MASK[4..].try_into().expect("eight bytes"));
    assert_ne!(mask & (1 << 61), 0, "LE Meta Event is what the whole command is for");
    // The specification's own default, kept rather than replaced with all ones:
    // events nothing here reads would sit in the controller's queue unread.
    assert_eq!(mask & 0x0000_1FFF_FFFF_FFFF, 0x0000_1FFF_FFFF_FFFF);
}

#[test]
fn hci_command_formats_passive_continuous_scan_when_parameters_configured() {
    let cmd = set_scan_parameters(0x0060, 0x0060);
    assert_eq!(&cmd[..4], &[0x01, 0x0B, 0x20, 0x07], "opcode 0x200B, seven parameters");
    assert_eq!(cmd[4], 0x00, "passive: the node must not transmit a scan request");
    assert_eq!(&cmd[5..9], &[0x60, 0x00, 0x60, 0x00], "interval and window, little-endian");
    assert_eq!(&cmd[9..], &[0x00, 0x00], "public address, accept everything");
    assert_eq!(usize::from(cmd[3]), cmd.len() - 4, "the declared length matches the payload");
}

#[test]
fn hci_command_formats_scan_enable_without_filter_duplicates_when_toggled() {
    assert_eq!(set_scan_enable(true), [0x01, 0x0C, 0x20, 0x02, 0x01, 0x00]);
    assert_eq!(set_scan_enable(false), [0x01, 0x0C, 0x20, 0x02, 0x00, 0x00]);
}

#[test]
fn hci_parser_extracts_correct_mac_address_when_parsing_advertising_report() {
    let packet = event(&[(ADDR, &[0x02, 0x01, 0x06], -70)]);
    let reports: Vec<_> = adv_reports(&packet).collect();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0].address, ADDR);
    assert_eq!(reports[0].rssi, -70);
    assert!(reports[0].has_rssi());
}

#[test]
fn hci_parser_extracts_all_reports_when_event_contains_multiple_records() {
    let other = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
    let packet = event(&[(ADDR, &[], -40), (other, &[0x02, 0x01, 0x06], -90)]);
    let reports: Vec<_> = adv_reports(&packet).collect();
    assert_eq!(reports.len(), 2);
    assert_eq!(reports[0].address, ADDR);
    assert_eq!(reports[0].rssi, -40);
    assert_eq!(reports[1].address, other);
    assert_eq!(reports[1].rssi, -90);
}

#[test]
fn hci_parser_identifies_missing_rssi_when_controller_reports_127() {
    // 127 is the specification's "not available", and is not a dBm a radio
    // could report. Passing it through would put an implausible row in the
    // export.
    let packet = event(&[(ADDR, &[], 127)]);
    let report = adv_reports(&packet).next().expect("one report");
    assert!(!report.has_rssi());
}

#[test]
fn hci_parser_ignores_non_advertising_events_when_parsing_packets() {
    // Command completions and unknown events arrive on the same pipe, so the
    // caller hands everything over and expects silence for most of it.
    assert_eq!(adv_reports(&[]).count(), 0);
    assert_eq!(adv_reports(&[0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00]).count(), 0, "cmd complete");
    assert_eq!(adv_reports(&[0x04, 0x3E, 0x02, 0x0D, 0x00]).count(), 0, "extended report");
    assert_eq!(adv_reports(&[0x02, 0x3E, 0x02, 0x02, 0x01]).count(), 0, "an ACL packet");
}

#[test]
fn hci_parser_handles_truncated_event_safely_when_payload_is_cut() {
    let packet = event(&[(ADDR, &[0x02, 0x01, 0x06], -70), (ADDR, &[], -70)]);
    for cut in 5..packet.len() {
        // Whatever survives must be a prefix of the whole reading; the point is
        // that nothing panics and no address is fabricated.
        for report in adv_reports(&packet[..cut]) {
            assert_eq!(report.address, ADDR);
        }
    }
}

#[test]
fn hci_parser_survives_inflated_report_count_when_payload_is_short() {
    let mut packet = event(&[(ADDR, &[], -70)]);
    packet[4] = 200;
    assert_eq!(adv_reports(&packet).count(), 1, "one report is all there is");
}

#[test]
fn hci_report_converts_to_ble_sighting_frame_when_encoded() {
    let packet = event(&[(ADDR, &[], -70)]);
    let report = adv_reports(&packet).next().expect("one report");
    let msg = report.as_msg(&[]);
    assert_eq!(msg.kind, RecordKind::Ble);
    assert_eq!(msg.security, Security::Ble);
    assert_eq!(msg.channel, 0, "BLE has no channel, and the exporter depends on the zero");
    assert!(msg.ssid.is_empty(), "and no SSID");
    assert!(msg.ext.is_empty(), "and no trailer: this advertiser sent none");

    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = report.with_msg(|msg| msg.encode_record_into(&mut buf).expect("fits"));
    let (back, consumed) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert_eq!(consumed, len);
    assert_eq!(back, msg);
    assert_eq!(back.bssid, ADDR);
}

#[test]
fn hci_parser_extracts_manufacturer_id_when_manufacturer_structure_present() {
    // `0xFF` is the manufacturer-specific structure, and its payload begins
    // with the two little-endian bytes WiGLE's `MfgrId` column wants. Here it
    // sits behind a flags structure, which is how real advertisers carry it.
    let data = &[0x02, 0x01, 0x06, 0x05, 0xFF, 0x4C, 0x00, 0x10, 0x02];
    let packet = event(&[(ADDR, data, -70)]);
    let report = adv_reports(&packet).next().expect("one report");
    assert_eq!(report.mfgr, Some(76));

    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = report.with_msg(|msg| msg.encode_record_into(&mut buf).expect("fits"));
    let (back, _) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert_eq!(back.ext, &76u16.to_le_bytes(), "the identifier is the trailer");
}

#[test]
fn hci_parser_returns_none_for_manufacturer_id_when_data_omits_structure() {
    // Most do not send any, and that is not a fault.
    let packet = event(&[(ADDR, &[0x02, 0x01, 0x06], -70)]);
    let report = adv_reports(&packet).next().expect("one report");
    assert_eq!(report.mfgr, None);
}

#[test]
fn hci_parser_suppresses_manufacturer_id_when_data_structure_is_malformed() {
    // The walk stops where the data stops making sense — same deal the beacon
    // parser gives a malformed element. A structure claiming five bytes while
    // carrying three ends the run before any identifier is guessed at.
    let packet = event(&[(ADDR, &[0x05, 0xFF, 0x4C, 0x00], -70)]);
    let report = adv_reports(&packet).next().expect("one report");
    assert_eq!(report.mfgr, None);
}

/// A report from the advertiser whose address ends in `n`.
fn report(n: u8, rssi: i8, mfgr: Option<u16>) -> AdvReport {
    AdvReport { address: [0x11, 0x22, 0x33, 0x44, 0x55, n], rssi, mfgr }
}

fn due(_: &AdvReport) -> bool {
    true
}

fn not_due(_: &AdvReport) -> bool {
    false
}

fn drain<const N: usize>(pending: &mut BlePending<N>) -> Vec<AdvReport> {
    std::iter::from_fn(|| pending.take()).collect()
}

#[test]
fn ble_pending_skips_report_when_not_due() {
    let mut pending = BlePending::<4>::new();
    pending.record(report(1, -60, None), not_due);
    assert!(pending.is_empty());
    assert_eq!(pending.dropped(), 0, "a report not due is not a report lost");
    assert_eq!(pending.take(), None);
}

#[test]
fn ble_pending_merges_mfgr_when_later_report_not_due() {
    // The identifier often follows the first hearing in a later packet whose
    // RSSI is no better; the address already holds a slot, so it merges
    // without asking whether it is due.
    let mut pending = BlePending::<4>::new();
    pending.record(report(1, -60, None), due);
    pending.record(report(1, -70, Some(0x004C)), |_| panic!("a held address is not re-checked"));
    assert_eq!(drain(&mut pending), [report(1, -60, Some(0x004C))]);
}

#[test]
fn ble_pending_keeps_strongest_rssi_when_address_repeats() {
    let mut pending = BlePending::<4>::new();
    pending.record(report(1, -70, Some(0x0006)), due);
    pending.record(report(1, -50, Some(0x004C)), due);
    pending.record(report(1, -80, None), due);
    // The first identifier stays: a later one is not a better reading of it.
    assert_eq!(drain(&mut pending), [report(1, -50, Some(0x0006))]);
}

#[test]
fn ble_pending_drops_newest_when_full() {
    let mut pending = BlePending::<2>::new();
    pending.record(report(1, -60, None), due);
    pending.record(report(2, -60, None), due);
    pending.record(report(3, -60, None), not_due);
    assert_eq!(pending.dropped(), 0, "only a due address counts against a full buffer");
    pending.record(report(4, -60, None), due);
    assert_eq!(pending.dropped(), 1);
    assert_eq!(drain(&mut pending), [report(1, -60, None), report(2, -60, None)]);
}

#[test]
fn ble_pending_counts_advertiser_once_when_repeated_while_full() {
    // The counter is advertisers, not packets: one address with no room
    // counts once however often it repeats within the scan, and again in the next.
    let mut pending = BlePending::<1>::new();
    pending.record(report(1, -60, None), due);
    for _ in 0..3 {
        pending.record(report(2, -60, None), due);
    }
    assert_eq!(pending.dropped(), 1);
    assert_eq!(pending.len(), 1);

    pending.clear();
    pending.record(report(1, -60, None), due);
    pending.record(report(2, -60, None), due);
    assert_eq!(pending.dropped(), 2, "a new scan counts it again");
}

#[test]
fn ble_pending_ignores_report_when_rssi_unavailable() {
    let mut pending = BlePending::<4>::new();
    pending.record(report(1, 127, Some(0x004C)), |_| panic!("no reading is never considered"));
    assert!(pending.is_empty());
    assert_eq!(pending.dropped(), 0);
}

#[test]
fn ble_pending_admits_new_address_when_full_of_not_due_traffic() {
    // A crowded room of advertisers the host already has, heard on every scan,
    // outnumbering the buffer: the new ones must still land.
    let mut pending = BlePending::<8>::new();
    let fresh = 100..105;
    let is_fresh = |r: &AdvReport| fresh.contains(&r.address[5]);
    for n in (0..20).chain(fresh.clone()) {
        pending.record(report(n, -60, None), is_fresh);
    }
    let landed: Vec<u8> = drain(&mut pending).iter().map(|r| r.address[5]).collect();
    assert_eq!(landed, fresh.collect::<Vec<_>>());
    assert_eq!(pending.dropped(), 0);
}

#[test]
fn ble_pending_take_stops_at_len_when_reset() {
    let mut pending = BlePending::<4>::new();
    pending.record(report(1, -60, None), due);
    pending.record(report(2, -60, None), due);
    pending.record(report(3, -60, None), due);
    assert_eq!(drain(&mut pending).len(), 3);

    // A fresh scan hearing fewer advertisers than the last leaves stale slots
    // behind `len`, and the zero fill beyond those; neither is ever yielded.
    pending.clear();
    pending.record(report(9, -40, None), due);
    assert_eq!(drain(&mut pending), [report(9, -40, None)]);
    assert_eq!(pending.take(), None);

    let mut untouched = BlePending::<4>::new();
    assert_eq!(untouched.take(), None, "the zero fill is not an advertiser");
}

/// One report in an LE Extended Advertising Report event.
struct Ext<'a> {
    event_type: u16,
    address_type: u8,
    address: [u8; 6],
    primary_phy: u8,
    secondary_phy: u8,
    rssi: i8,
    data: &'a [u8],
}

/// An LE Extended Advertising Report event carrying `reports`.
fn ext_event(reports: &[Ext<'_>]) -> Vec<u8> {
    let mut params = vec![0x0D, u8::try_from(reports.len()).expect("few reports")];
    for r in reports {
        params.extend(r.event_type.to_le_bytes());
        params.push(r.address_type);
        params.extend(r.address.iter().rev());
        params.push(r.primary_phy);
        params.push(r.secondary_phy);
        params.push(0x02); // SID
        params.push(0x7F); // Tx power: not available
        params.push(r.rssi as u8);
        params.extend([0x00, 0x00]); // no periodic advertising
        params.push(0x00); // direct address type
        params.extend([0x00; 6]); // direct address
        params.push(u8::try_from(r.data.len()).expect("short data"));
        params.extend_from_slice(r.data);
    }
    let mut packet = vec![0x04, 0x3E, u8::try_from(params.len()).expect("fits")];
    packet.extend_from_slice(&params);
    packet
}

/// A legacy connectable undirected PDU as the extended report describes it, on 1M.
fn legacy_ext(address: [u8; 6], rssi: i8) -> Ext<'static> {
    // Legacy (bit 4), connectable (bit 0), scannable (bit 1), complete.
    Ext {
        event_type: 0x0013,
        address_type: 0x00,
        address,
        primary_phy: PHY_1M,
        secondary_phy: 0x00,
        rssi,
        data: &[0x02, 0x01, 0x06],
    }
}

/// A Command Complete for `opcode` with `status` and `ret` after it.
fn complete(opcode: u16, status: u8, ret: &[u8]) -> Vec<u8> {
    let [lo, hi] = opcode.to_le_bytes();
    let mut packet = vec![0x04, 0x0E, u8::try_from(4 + ret.len()).expect("fits"), 0x01, lo, hi];
    packet.push(status);
    packet.extend_from_slice(ret);
    packet
}

const EXT_ADDR: [u8; 6] = [0xC0, 0xDE, 0xBE, 0xAC, 0x00, 0x02];

#[test]
fn hci_command_sets_ext_report_bit_when_constructing_le_event_mask() {
    assert_eq!(SET_LE_EVENT_MASK[..4], [0x01, 0x01, 0x20, 0x08], "opcode 0x2001, eight bytes");
    let mask = u64::from_le_bytes(SET_LE_EVENT_MASK[4..].try_into().expect("eight bytes"));
    // Bit 12 is subevent 0x0D; the rest is the specification's post-reset default.
    assert_eq!(mask, 0x1F | (1 << 12));
}

#[test]
fn hci_command_encodes_capability_reads_when_constants_are_evaluated() {
    assert_eq!(READ_LOCAL_SUPPORTED_COMMANDS, [0x01, 0x02, 0x10, 0x00]);
    assert_eq!(LE_READ_LOCAL_SUPPORTED_FEATURES, [0x01, 0x03, 0x20, 0x00]);
}

#[test]
fn hci_command_formats_one_phy_block_when_ext_scan_on_1m() {
    let cmd = set_ext_scan_parameters(ScanPhys::OneM, 0x0030, 0x0030);
    assert_eq!(
        cmd.as_bytes(),
        [
            0x01, 0x41, 0x20, 0x08, // opcode 0x2041, eight parameters
            0x00, 0x00, 0x01, // public, accept everything, 1M
            0x00, 0x30, 0x00, 0x30, 0x00, // passive, interval, window
        ]
    );
}

#[test]
fn hci_command_formats_two_phy_blocks_when_ext_scan_on_1m_and_coded() {
    let cmd = set_ext_scan_parameters(ScanPhys::OneMAndCoded, 0x0030, 0x0020);
    assert_eq!(
        cmd.as_bytes(),
        [
            0x01, 0x41, 0x20, 0x0D, // opcode 0x2041, thirteen parameters
            0x00, 0x00, 0x05, // public, accept everything, 1M and Coded
            0x00, 0x30, 0x00, 0x20, 0x00, // 1M: passive, interval, window
            0x00, 0x30, 0x00, 0x20, 0x00, // Coded: the same
        ]
    );
}

#[test]
fn hci_command_formats_one_phy_block_when_ext_scan_on_coded() {
    let cmd = set_ext_scan_parameters(ScanPhys::Coded, 0x0030, 0x0030);
    assert_eq!(
        cmd.as_bytes(),
        [0x01, 0x41, 0x20, 0x08, 0x00, 0x00, 0x04, 0x00, 0x30, 0x00, 0x30, 0x00]
    );
}

#[test]
fn hci_command_formats_ext_scan_enable_without_filter_duplicates_when_toggled() {
    // Duplicates off, duration 0 and period 0: runs until disabled.
    assert_eq!(set_ext_scan_enable(true), [0x01, 0x42, 0x20, 0x06, 0x01, 0, 0, 0, 0, 0]);
    assert_eq!(set_ext_scan_enable(false), [0x01, 0x42, 0x20, 0x06, 0x00, 0, 0, 0, 0, 0]);
}

#[test]
fn hci_command_complete_reads_opcode_and_status_when_packet_is_complete() {
    assert_eq!(command_complete(&complete(0x0C03, 0x00, &[])), Some((0x0C03, 0x00)));
    assert_eq!(command_complete(&complete(0x2041, 0x0C, &[])), Some((0x2041, 0x0C)));
}

#[test]
fn hci_command_complete_returns_none_when_packet_is_not_complete() {
    assert_eq!(command_complete(&[]), None);
    // A Command Status, which carries its status in a different place.
    assert_eq!(command_complete(&[0x04, 0x0F, 0x04, 0x00, 0x01, 0x41, 0x20]), None);
    // Opcode 0 announcing free slots, with no status at all.
    assert_eq!(command_complete(&[0x04, 0x0E, 0x03, 0x01, 0x00, 0x00]), None);
    assert_eq!(command_complete(&event(&[(ADDR, &[], -70)])), None);
}

#[test]
fn hci_ext_scan_commands_reads_octet_37_when_answer_lists_them() {
    let mut table = [0u8; 64];
    table[37] = 1 << 5;
    let packet = complete(0x1002, 0x00, &table);
    assert_eq!(
        ext_scan_commands(&packet),
        Some(ExtScanCommands { parameters: true, enable: false })
    );
    table[37] = (1 << 5) | (1 << 6);
    let packet = complete(0x1002, 0x00, &table);
    assert_eq!(
        ext_scan_commands(&packet),
        Some(ExtScanCommands { parameters: true, enable: true })
    );
}

#[test]
fn hci_ext_scan_commands_returns_none_when_answer_is_not_usable() {
    let table = [0xFFu8; 64];
    assert_eq!(ext_scan_commands(&complete(0x1002, 0x01, &table)), None, "failed read");
    assert_eq!(ext_scan_commands(&complete(0x2003, 0x00, &table)), None, "another command");
    assert_eq!(ext_scan_commands(&complete(0x1002, 0x00, &table[..37])), None, "short");
}

#[test]
fn hci_le_features_reads_coded_and_extended_bits_when_answer_lists_them() {
    let bits: u64 = (1 << 11) | (1 << 12);
    let features = le_features(&complete(0x2003, 0x00, &bits.to_le_bytes())).expect("read");
    assert!(features.coded_phy());
    assert!(features.extended_advertising());

    let features = le_features(&complete(0x2003, 0x00, &(1u64 << 12).to_le_bytes())).expect("read");
    assert!(!features.coded_phy());
    assert!(features.extended_advertising());
}

#[test]
fn hci_le_features_returns_none_when_answer_is_not_usable() {
    let bits = u64::MAX.to_le_bytes();
    assert_eq!(le_features(&complete(0x2003, 0x01, &bits)), None, "failed read");
    assert_eq!(le_features(&complete(0x1002, 0x00, &bits)), None, "another command");
    assert_eq!(le_features(&complete(0x2003, 0x00, &bits[..7])), None, "short");
}

#[test]
fn hci_ext_parser_reads_legacy_and_extended_reports_when_event_holds_both() {
    let mfgr = &[0x06, 0xFF, 0xFF, 0xFF, b'W', b'T', 0x01];
    let packet = ext_event(&[
        legacy_ext(ADDR, -60),
        Ext {
            // Non-connectable, non-scannable, not legacy, complete.
            event_type: 0x0000,
            address_type: 0x01,
            address: EXT_ADDR,
            primary_phy: PHY_CODED,
            secondary_phy: PHY_CODED,
            rssi: -85,
            data: mfgr,
        },
    ]);
    let reports: Vec<_> = ext_adv_reports(&packet).collect();
    assert_eq!(reports.len(), 2);

    assert_eq!(reports[0].address, ADDR, "reversed into display order");
    assert!(reports[0].legacy);
    assert_eq!(reports[0].primary_phy, PHY_1M);
    assert_eq!(reports[0].secondary_phy, 0x00);
    assert_eq!(reports[0].data_status, DataStatus::Complete);
    assert_eq!(reports[0].rssi, -60);
    assert_eq!(reports[0].mfgr, None);
    assert!(!reports[0].is_anonymous());

    assert_eq!(reports[1].address, EXT_ADDR);
    assert_eq!(reports[1].address_type, 0x01);
    assert!(!reports[1].legacy);
    assert_eq!(reports[1].primary_phy, PHY_CODED);
    assert_eq!(reports[1].secondary_phy, PHY_CODED);
    assert_eq!(reports[1].rssi, -85);
    assert_eq!(reports[1].mfgr, Some(0xFFFF));
    assert!(reports[1].has_rssi());
}

#[test]
fn hci_ext_parser_reads_data_status_when_event_type_carries_it() {
    let status = |bits: u16| {
        let mut ext = legacy_ext(ADDR, -60);
        ext.event_type = bits << 5;
        ext.secondary_phy = PHY_2M;
        let packet = ext_event(&[ext]);
        ext_adv_reports(&packet).next().expect("one report").data_status
    };
    assert_eq!(status(0), DataStatus::Complete);
    assert_eq!(status(1), DataStatus::MoreToCome);
    assert_eq!(status(2), DataStatus::Truncated);
    assert_eq!(status(3), DataStatus::Reserved);
}

#[test]
fn hci_ext_parser_flags_anonymous_when_address_type_is_ff() {
    let mut ext = legacy_ext([0; 6], -70);
    ext.event_type = 0x0000;
    ext.address_type = ANONYMOUS;
    let packet = ext_event(&[ext]);
    let report = ext_adv_reports(&packet).next().expect("one report");
    assert!(report.is_anonymous());
    assert!(!report.legacy);
}

#[test]
fn hci_ext_parser_handles_truncated_event_safely_when_payload_is_cut() {
    let packet = ext_event(&[legacy_ext(ADDR, -60), legacy_ext(ADDR, -70)]);
    for cut in 0..packet.len() {
        // A prefix of the whole reading, never a fabricated address.
        let reports: Vec<_> = ext_adv_reports(&packet[..cut]).collect();
        assert!(reports.len() < 2, "cut at {cut} still yields both");
        for report in reports {
            assert_eq!(report.address, ADDR);
        }
    }
    assert_eq!(ext_adv_reports(&packet).count(), 2);
}

#[test]
fn hci_ext_parser_survives_inflated_report_count_when_payload_is_short() {
    let mut packet = ext_event(&[legacy_ext(ADDR, -60)]);
    packet[4] = 200;
    assert_eq!(ext_adv_reports(&packet).count(), 1, "one report is all there is");
}

#[test]
fn hci_parsers_ignore_each_others_subevent_when_handed_the_wrong_report() {
    // One scan family per reset, but the caller hands every packet to whichever
    // parser it holds; a report of the other kind must read as nothing.
    let legacy = event(&[(ADDR, &[0x02, 0x01, 0x06], -70)]);
    assert_eq!(ext_adv_reports(&legacy).count(), 0);
    let extended = ext_event(&[legacy_ext(ADDR, -60)]);
    assert_eq!(adv_reports(&extended).count(), 0);
    assert_eq!(ext_adv_reports(&complete(0x2042, 0x00, &[])).count(), 0, "cmd complete");
}
