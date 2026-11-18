//! The four commands and two events a BLE scan is made of, and the record a report becomes.

use wartui_proto::bus::{RecordKind, SIGHTING_RECORD_MAX, Security, SightingMsg};
use wartui_proto::hci::{
    AdvReport, BLE_NAME_MAX, BleTrailer, RESET, SET_EVENT_MASK, adv_reports, command_complete,
    opcode, set_scan_enable, set_scan_parameters,
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

/// One report of `event_type` and `address_type` carrying `data`, the fields the other
/// helper fixes.
fn one(event_type: u8, address_type: u8, data: &[u8]) -> AdvReport {
    let mut packet = event(&[(ADDR, data, -70)]);
    packet[5] = event_type;
    packet[6] = address_type;
    adv_reports(&packet).next().expect("one report")
}

/// The one report of an event carrying `data`.
fn report(data: &[u8]) -> AdvReport {
    adv_reports(&event(&[(ADDR, data, -70)])).next().expect("one report")
}

/// An advertising data structure: its length, its type, its payload.
fn ad(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![u8::try_from(payload.len() + 1).expect("short"), kind];
    out.extend_from_slice(payload);
    out
}

const ADDR: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

/// Flags: LE General Discoverable, BR/EDR not supported. What most advertisers lead with.
const FLAGS: [u8; 3] = [0x02, 0x01, 0x06];

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
    let data = [&FLAGS[..], &ad(0x09, b"Tag"), &ad(0xFF, &[0x4C, 0x00, 0x10])].concat();
    let report = report(&data);
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = report.with_msg(|msg| {
        assert_eq!(msg.kind, RecordKind::Ble);
        assert_eq!(msg.security, Security::Ble);
        assert_eq!(msg.channel, 0, "BLE has no channel, and the exporter depends on the zero");
        msg.encode_record_into(&mut buf).expect("fits")
    });
    assert_eq!(
        &buf[..len],
        &[
            0x01, // kind: BLE
            0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, // address
            0x00, // channel 0
            0xBA, // rssi -70
            0x0A, // security: [BLE]
            0x03, b'T', b'a', b'g', // the name, as the SSID
            0x03, // ext_len: flags and identifier
            0x03, // random, connectable
            0x4C, 0x00, // company identifier 76, little-endian
        ]
    );
    let (back, consumed) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert_eq!(consumed, len);
    assert_eq!(back.bssid, ADDR);
    assert_eq!(back.ssid, b"Tag");
}

#[test]
fn hci_report_hands_over_flags_byte_alone_when_advertiser_sent_nothing() {
    let report = one(0x03, 0x00, &[]);
    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = report.with_msg(|msg| msg.encode_record_into(&mut buf).expect("fits"));
    let (back, _) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert!(back.ssid.is_empty(), "no name");
    assert_eq!(back.ext, [0x00], "public, not connectable, no identifier: still the flags byte");
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
    assert_eq!(BleTrailer::decode(back.ext).and_then(|t| t.mfgr), Some(76));
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

#[test]
fn hci_parser_reads_complete_name_when_advertised() {
    let report = report(&[&FLAGS[..], &ad(0x09, b"Kitchen")].concat());
    assert_eq!(report.name(), b"Kitchen");
}

#[test]
fn hci_parser_reads_shortened_name_when_no_complete_name_advertised() {
    let report = report(&[&FLAGS[..], &ad(0x08, b"Kit")].concat());
    assert_eq!(report.name(), b"Kit");
}

#[test]
fn hci_parser_prefers_complete_name_when_both_advertised() {
    // Shortened first, so the order is not what decides it.
    let report = report(&[ad(0x08, b"Kit"), ad(0x09, b"Kitchen")].concat());
    assert_eq!(report.name(), b"Kitchen");
}

#[test]
fn hci_parser_keeps_first_name_when_two_complete_names_advertised() {
    let report = report(&[ad(0x09, b"one"), ad(0x09, b"two")].concat());
    assert_eq!(report.name(), b"one");
}

#[test]
fn hci_parser_returns_empty_name_when_none_advertised() {
    let report = report(&[&FLAGS[..], &ad(0xFF, &[0x4C, 0x00])].concat());
    assert!(report.name().is_empty());
    assert_eq!(report.mfgr, Some(76), "and the identifier still reads");
}

#[test]
fn hci_parser_reads_name_and_manufacturer_id_when_in_either_order() {
    let name = ad(0x09, b"Tag");
    let mfgr = ad(0xFF, &[0x4C, 0x00, 0x10]);
    for data in [[&name[..], &mfgr].concat(), [&mfgr[..], &name].concat()] {
        let report = report(&data);
        assert_eq!(report.name(), b"Tag", "{data:02X?}");
        assert_eq!(report.mfgr, Some(76), "{data:02X?}");
    }
}

#[test]
fn hci_parser_reads_longest_name_when_it_fills_the_advertisement() {
    // 31 bytes of advertising data: one structure's length and type, and 29 of name.
    let name = [b'n'; BLE_NAME_MAX];
    let data = ad(0x09, &name);
    assert_eq!(data.len(), 31);
    let report = report(&data);
    assert_eq!(report.name(), name);

    let mut buf = [0u8; SIGHTING_RECORD_MAX];
    let len = report.with_msg(|msg| msg.encode_record_into(&mut buf).expect("fits"));
    let (back, _) = SightingMsg::decode_record(&buf[..len]).expect("valid");
    assert_eq!(back.ssid, name);
}

#[test]
fn hci_parser_keeps_name_when_later_structure_is_malformed() {
    // The walk stops where the data stops making sense, and keeps what it read before.
    let data = [&ad(0x09, b"Tag")[..], &[0x05, 0xFF, 0x4C, 0x00]].concat();
    let report = report(&data);
    assert_eq!(report.name(), b"Tag");
    assert_eq!(report.mfgr, None, "the truncated identifier is not guessed at");
}

#[test]
fn hci_parser_keeps_manufacturer_id_when_later_structure_is_malformed() {
    let data = [&ad(0xFF, &[0x4C, 0x00])[..], &[0x05, 0x09, b'T']].concat();
    let report = report(&data);
    assert_eq!(report.mfgr, Some(76));
    assert!(report.name().is_empty());
}

#[test]
fn hci_parser_reads_address_type_when_report_is_public_or_random() {
    // Public, random, public identity and random identity, both resolved by the controller.
    let random: Vec<bool> = (0..4).map(|kind| one(0x00, kind, &[]).random).collect();
    assert_eq!(random, [false, true, false, true]);
}

#[test]
fn hci_parser_reads_connectable_when_event_type_allows_connection() {
    // `ADV_IND`, `ADV_DIRECT_IND`, `ADV_SCAN_IND`, `ADV_NONCONN_IND`.
    let connectable: Vec<bool> = (0..4).map(|kind| one(kind, 0x00, &[]).connectable).collect();
    assert_eq!(connectable, [true, true, false, false]);
}

#[test]
fn ble_trailer_encodes_golden_bytes_when_identifier_present_or_absent() {
    let mut out = [0u8; BleTrailer::MAX];
    let full = BleTrailer { random: true, connectable: true, mfgr: Some(0x004C) };
    let len = full.encode(&mut out);
    assert_eq!(&out[..len], [0x03, 0x4C, 0x00]);

    let mut out = [0u8; BleTrailer::MAX];
    let bare = BleTrailer { random: false, connectable: false, mfgr: None };
    let len = bare.encode(&mut out);
    assert_eq!(&out[..len], [0x00]);
}

#[test]
fn ble_trailer_round_trips_when_identifier_present_or_absent() {
    for random in [false, true] {
        for connectable in [false, true] {
            for mfgr in [None, Some(0x004C), Some(0xFFFF)] {
                let trailer = BleTrailer { random, connectable, mfgr };
                let mut out = [0u8; BleTrailer::MAX];
                let len = trailer.encode(&mut out);
                assert_eq!(len, if mfgr.is_some() { 3 } else { 1 });
                assert_eq!(BleTrailer::decode(&out[..len]), Some(trailer));
            }
        }
    }
}

#[test]
fn ble_trailer_returns_none_when_length_is_not_one_or_three() {
    assert_eq!(BleTrailer::decode(&[]), None);
    assert_eq!(BleTrailer::decode(&[0x4C, 0x00]), None, "a bare identifier is not a trailer");
    assert_eq!(BleTrailer::decode(&[0x03, 0x4C, 0x00, 0x00]), None);
}

#[test]
fn ble_trailer_ignores_unknown_flag_bits_when_decoded() {
    let trailer = BleTrailer::decode(&[0xFC]).expect("one byte");
    assert_eq!(trailer, BleTrailer { random: false, connectable: false, mfgr: None });
    let trailer = BleTrailer::decode(&[0xFD, 0x4C, 0x00]).expect("three bytes");
    assert_eq!(trailer, BleTrailer { random: true, connectable: false, mfgr: Some(76) });
}

#[test]
fn hci_opcode_reads_each_command_when_given_its_packet() {
    assert_eq!(opcode(&RESET), Some(0x0C03));
    assert_eq!(opcode(&SET_EVENT_MASK), Some(0x0C01));
    assert_eq!(opcode(&set_scan_parameters(0x0060, 0x0060)), Some(0x200B));
    assert_eq!(opcode(&set_scan_enable(true)), Some(0x200C));
}

#[test]
fn hci_opcode_returns_none_when_packet_is_not_a_command() {
    assert_eq!(opcode(&[]), None);
    assert_eq!(opcode(&[0x01, 0x03, 0x0C]), None, "no length byte");
    assert_eq!(opcode(&[0x04, 0x0E, 0x04, 0x01]), None, "an event");
}

#[test]
fn hci_command_complete_reads_success_when_status_is_zero() {
    // Command Complete for `HCI_Reset`: one command allowed, opcode 0x0C03, status 0.
    assert_eq!(command_complete(&[0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00]), Some((0x0C03, 0)));
}

#[test]
fn hci_command_complete_reads_refusal_when_status_is_nonzero() {
    // 0x12 is Invalid HCI Command Parameters.
    let packet = [0x04, 0x0E, 0x04, 0x01, 0x0B, 0x20, 0x12];
    assert_eq!(command_complete(&packet), Some((0x200B, 0x12)));
}

#[test]
fn hci_command_complete_returns_none_when_packet_is_an_advertising_report() {
    let packet = event(&[(ADDR, &[0x02, 0x01, 0x06], -70)]);
    assert_eq!(command_complete(&packet), None);
}

#[test]
fn hci_command_complete_returns_none_when_packet_is_truncated() {
    let packet = [0x04, 0x0E, 0x04, 0x01, 0x03, 0x0C, 0x00];
    for cut in 0..packet.len() {
        assert_eq!(command_complete(&packet[..cut]), None, "cut at {cut}");
    }
}

#[test]
fn hci_command_complete_returns_none_when_controller_announces_itself() {
    // At start-up the controller sends a Command Complete with opcode 0x0000 and no
    // return parameters, only to say how many commands it takes. It answers nothing.
    let answer = command_complete(&[0x04, 0x0E, 0x03, 0x01, 0x00, 0x00]);
    assert_eq!(answer, None);
}
