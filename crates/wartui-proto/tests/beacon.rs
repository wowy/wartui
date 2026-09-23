//! Reading access points out of 802.11 management frames.
//!
//! The frames here are assembled rather than captured, because the thing under
//! test is a classification ladder — the interesting cases are the ones a bench
//! capture is least likely to contain. Real frames go in `beacon_vectors.txt`
//! and are exercised by `every_captured_beacon_yields_a_usable_observation`.

use wartui_proto::air::{SIGHTING_MSG_MAX, SSID_MAX, Security, SightingMsg};
use wartui_proto::beacon::{parse_mgmt, rcoi_text};

/// One `tag, len, data` element.
fn ie(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![tag, u8::try_from(data.len()).expect("element fits")];
    out.extend_from_slice(data);
    out
}

/// An RSN element body: version 1, CCMP group cipher and pairwise suite, then
/// the given AKM suite types under the `00-0F-AC` OUI.
fn rsn(akms: &[u8]) -> Vec<u8> {
    suites(&[0x00, 0x0F, 0xAC], akms)
}

/// A WPA vendor element body, which is the same shape behind Microsoft's OUI.
fn wpa(akms: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00, 0x50, 0xF2, 0x01];
    out.extend_from_slice(&suites(&[0x00, 0x50, 0xF2], akms));
    out
}

fn suites(oui: &[u8], akms: &[u8]) -> Vec<u8> {
    let mut out = vec![0x01, 0x00];
    out.extend_from_slice(oui);
    out.push(4); // group cipher: CCMP
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(oui);
    out.push(4); // pairwise: CCMP
    out.extend_from_slice(&u16::try_from(akms.len()).expect("few AKMs").to_le_bytes());
    for &akm in akms {
        out.extend_from_slice(oui);
        out.push(akm);
    }
    out.extend_from_slice(&0u16.to_le_bytes()); // RSN capabilities
    out
}

const BSSID: [u8; 6] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

/// A management frame with the given subtype, capability bits and elements.
fn mgmt(subtype: u8, capability: u16, ies: &[u8]) -> Vec<u8> {
    let mut frame = vec![subtype << 4, 0x00, 0x00, 0x00];
    frame.extend_from_slice(&[0xFF; 6]); // addr1, destination
    frame.extend_from_slice(&BSSID); // addr2, source
    frame.extend_from_slice(&BSSID); // addr3, BSSID
    frame.extend_from_slice(&[0x00, 0x00]); // sequence control
    frame.extend_from_slice(&[0u8; 8]); // timestamp
    frame.extend_from_slice(&100u16.to_le_bytes()); // beacon interval
    frame.extend_from_slice(&capability.to_le_bytes());
    frame.extend_from_slice(ies);
    frame
}

/// A beacon with privacy set, since everything but `[OPEN]` has it.
fn beacon(ies: &[u8]) -> Vec<u8> {
    mgmt(8, 0x0011, ies)
}

fn security_of(ies: &[u8]) -> Security {
    parse_mgmt(&beacon(ies), -50, 6).expect("a beacon").security
}

#[test]
fn beacon_parser_classifies_network_as_open_when_no_privacy_or_ciphers_present() {
    let frame = mgmt(8, 0x0001, &ie(0, b"cafe"));
    let ap = parse_mgmt(&frame, -50, 6).expect("a beacon");
    assert_eq!(ap.security, Security::Open);
    assert_eq!(ap.ssid(), b"cafe");
    assert_eq!(ap.bssid, BSSID);
    assert_eq!(ap.rssi, -50);
}

#[test]
fn beacon_parser_classifies_network_as_wep_when_privacy_set_without_ciphers() {
    // Nothing else can explain an encrypted network that names no suite.
    assert_eq!(security_of(&ie(0, b"old")), Security::Wep);
}

#[test]
fn beacon_parser_maps_akm_suites_to_security_tokens_when_parsing_rsn_and_wpa() {
    // These strings reach the WiGLE `AuthMode` column unaltered, so the ladder
    // in `classify` is a contract, not an interpretation.
    assert_eq!(security_of(&ie(48, &rsn(&[2]))), Security::Wpa2Psk);
    assert_eq!(security_of(&ie(221, &wpa(&[2]))), Security::WpaPsk);
    assert_eq!(security_of(&ie(48, &rsn(&[8]))), Security::Wpa3Psk);
    assert_eq!(security_of(&ie(48, &rsn(&[9]))), Security::Wpa3Psk);
    assert_eq!(security_of(&ie(48, &rsn(&[2, 8]))), Security::Wpa2Wpa3Psk);
    assert_eq!(security_of(&ie(48, &rsn(&[1]))), Security::Wpa2Enterprise);
    assert_eq!(security_of(&ie(48, &rsn(&[5]))), Security::Wpa2Enterprise);
    assert_eq!(security_of(&ie(68, &[])), Security::WapiPsk);

    // The SHA-256 PSK variant is still `[WPA2_PSK]`.
    assert_eq!(security_of(&ie(48, &rsn(&[6]))), Security::Wpa2Psk);
}

#[test]
fn beacon_parser_classifies_network_as_wpa_wpa2_psk_when_both_rsn_and_wpa_present() {
    let mut ies = ie(0, b"mixed");
    ies.extend_from_slice(&ie(48, &rsn(&[2])));
    ies.extend_from_slice(&ie(221, &wpa(&[2])));
    assert_eq!(security_of(&ies), Security::WpaWpa2Psk);
}

#[test]
fn beacon_parser_classifies_network_as_wpa2_enterprise_when_wpa_specifies_enterprise() {
    // `WIFI_AUTH_ENTERPRISE` and `WIFI_AUTH_WPA2_ENTERPRISE` share the `[WPA2]`
    // token, and the token is what has to match.
    assert_eq!(security_of(&ie(221, &wpa(&[1]))), Security::Wpa2Enterprise);
}

#[test]
fn beacon_parser_prioritises_rsn_psk_over_legacy_wpa_when_both_elements_present() {
    // The WPA element names 802.1X and the RSN element names PSK. The enterprise
    // rung is guarded with `!has_rsn`, so RSN decides and this is `[WPA2_PSK]`.
    // Without that guard it reads as `[WPA2]`, for an access point that will
    // negotiate PSK.
    let mut ies = ie(0, b"legacy");
    ies.extend_from_slice(&ie(48, &rsn(&[2])));
    ies.extend_from_slice(&ie(221, &wpa(&[1])));
    assert_eq!(security_of(&ies), Security::Wpa2Psk);
}

#[test]
fn beacon_parser_classifies_network_as_undefined_when_given_unmapped_akm_suite() {
    // The firmware's `switch` has no arm for `WIFI_AUTH_OWE`, so it falls to
    // `default`. Reproducing the gap is deliberate: the column is a contract
    // with an exporter, not a description of the network.
    assert_eq!(security_of(&ie(48, &rsn(&[18]))), Security::Undefined);
}

#[test]
fn beacon_parser_prioritises_wapi_security_when_multiple_security_ies_present() {
    // It is tested first, before RSN is consulted.
    let mut ies = ie(48, &rsn(&[2]));
    ies.extend_from_slice(&ie(68, &[]));
    assert_eq!(security_of(&ies), Security::WapiPsk);
}

#[test]
fn beacon_parser_extracts_bssid_when_ssid_element_is_empty() {
    // Twelve of the access points in the Phase 0 capture had empty SSIDs, and
    // a wardrive that dropped them would be missing the interesting ones.
    let ap = parse_mgmt(&beacon(&ie(0, b"")), -70, 11).expect("a beacon");
    assert!(ap.ssid().is_empty());
    assert_eq!(ap.bssid, BSSID);
}

#[test]
fn beacon_parser_recognises_hidden_network_when_ssid_contains_only_zero_bytes() {
    // The other way of cloaking: the element carries the name's real length
    // with every byte zeroed. This is the exact shape that reached a WiGLE
    // export as eight NULs in the SSID column.
    let ap = parse_mgmt(&beacon(&ie(0, &[0u8; 8])), -70, 11).expect("a beacon");
    assert!(ap.ssid().is_empty());
    assert_eq!(ap.bssid, BSSID);
}

#[test]
fn beacon_parser_strips_trailing_null_bytes_when_ssid_is_zero_padded() {
    let ap = parse_mgmt(&beacon(&ie(0, b"Home\0\0\0")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"Home");
}

#[test]
fn beacon_parser_preserves_interior_null_bytes_when_parsing_ssid() {
    // The guard against over-trimming. Nothing here knows what a NUL in the
    // middle of a name was meant to be, so nothing here decides.
    let ap = parse_mgmt(&beacon(&ie(0, b"a\0b")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"a\0b");
    // And a name ending in a byte that is not zero is untouched.
    let ap = parse_mgmt(&beacon(&ie(0, b"plain")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"plain");
}

#[test]
fn beacon_parser_recognises_hidden_network_when_overlong_ssid_contains_only_zeros() {
    let ap = parse_mgmt(&beacon(&ie(0, &[0u8; 40])), -50, 6).expect("a beacon");
    assert!(ap.ssid().is_empty());
}

#[test]
fn beacon_parser_clamps_before_trimming_when_ssid_exceeds_legal_length() {
    // The input that separates clamping first from trimming first, and so the
    // reason the parser does them in that order: trimmed first this keeps
    // thirty-nine bytes, and the clamp then yields thirty-two zeros — a
    // network named after its own padding.
    let mut ssid = [0u8; 40];
    ssid[39] = b'X';
    let ap = parse_mgmt(&beacon(&ie(0, &ssid)), -50, 6).expect("a beacon");
    assert!(ap.ssid().is_empty(), "clamped to padding, and padding is a hidden network");
}

#[test]
fn beacon_parser_extracts_channel_from_ds_or_ht_elements_when_present() {
    // DS Parameter Set on 2.4 GHz.
    let mut ies = ie(0, b"n");
    ies.extend_from_slice(&ie(3, &[11]));
    assert_eq!(parse_mgmt(&beacon(&ies), -50, 6).expect("beacon").channel, 11);

    // 5 GHz beacons omit it, so the HT Operation element's primary channel is
    // what says where a dual-band access point actually is.
    let mut ies = ie(0, b"n");
    ies.extend_from_slice(&ie(61, &[149, 0, 0, 0, 0]));
    assert_eq!(parse_mgmt(&beacon(&ies), -50, 36).expect("beacon").channel, 149);

    // With neither, the channel we were parked on is the honest answer.
    assert_eq!(parse_mgmt(&beacon(&ie(0, b"n")), -50, 44).expect("beacon").channel, 44);
}

#[test]
fn beacon_parser_prioritises_ds_parameter_set_over_ht_operation_when_both_present() {
    let mut ies = ie(3, &[6]);
    ies.extend_from_slice(&ie(61, &[36, 0, 0, 0, 0]));
    assert_eq!(parse_mgmt(&beacon(&ies), -50, 1).expect("beacon").channel, 6);
}

#[test]
fn beacon_parser_accepts_probe_responses_and_rejects_other_frames_when_parsing_mgmt() {
    assert!(parse_mgmt(&mgmt(5, 0x0001, &ie(0, b"n")), -50, 6).is_some(), "probe response");
    assert!(parse_mgmt(&mgmt(4, 0x0001, &ie(0, b"n")), -50, 6).is_none(), "probe request");
    assert!(parse_mgmt(&mgmt(13, 0x0001, &[]), -50, 6).is_none(), "action frame");

    // Type 2 is data, which arrives constantly and carries none of this.
    let mut data = mgmt(8, 0x0001, &ie(0, b"n"));
    data[0] = 0b0000_1000;
    assert!(parse_mgmt(&data, -50, 6).is_none(), "data frame");
}

#[test]
fn beacon_parser_rejects_frame_when_length_is_insufficient_for_fixed_fields() {
    let frame = beacon(&ie(0, b"n"));
    for len in 0..36 {
        assert!(parse_mgmt(&frame[..len], -50, 6).is_none(), "{len} bytes is not a beacon");
    }
    assert!(parse_mgmt(&frame[..36], -50, 6).is_some(), "36 bytes is header plus fixed fields");
}

#[test]
fn beacon_parser_preserves_bssid_when_element_list_is_truncated() {
    // A frame that was received well enough to have a BSSID is worth reporting
    // even if its tail was clipped.
    let mut ies = ie(0, b"cafe");
    ies.extend_from_slice(&ie(48, &rsn(&[2])));
    let full = beacon(&ies);
    for cut in 36..full.len() {
        let ap = parse_mgmt(&full[..cut], -50, 6).expect("still a beacon");
        assert_eq!(ap.bssid, BSSID);
    }
}

#[test]
fn beacon_parser_handles_corrupt_suite_count_without_overrunning_when_parsing_rsn() {
    // Claiming 4000 pairwise suites inside a 20-byte element is either a bug or
    // an attack; either way it must not read past the frame.
    let mut body = vec![0x01, 0x00, 0x00, 0x0F, 0xAC, 4];
    body.extend_from_slice(&4000u16.to_le_bytes());
    let ap = parse_mgmt(&beacon(&ie(48, &body)), -50, 6).expect("still a beacon");
    // No AKM was readable, so only the privacy bit remains to go on.
    assert_eq!(ap.security, Security::Wep);
}

#[test]
fn beacon_parser_truncates_ssid_to_max_length_when_element_is_overlong() {
    let long = [b'x'; 60];
    let ap = parse_mgmt(&beacon(&ie(0, &long)), -50, 6).expect("a beacon");
    assert_eq!(ap.ssid().len(), SSID_MAX);
}

/// The OpenRoaming triple, the widest roaming consortium element anybody real
/// beacons: three five-byte identifiers behind a count byte and a
/// nibble-packed lengths byte.
const OPEN_ROAMING: [u8; 17] = [
    0x02, 0x55, //
    0x5A, 0x03, 0xBA, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x00, 0x00, //
    0xBA, 0xA2, 0xD0, 0x20, 0x00,
];

#[test]
fn beacon_parser_extracts_roaming_consortium_verbatim_when_passpoint_ie_present() {
    // Kept raw — count and lengths bytes included — so reading it differently
    // later costs a re-export, never a reflash.
    let ap = parse_mgmt(&beacon(&ie(111, &OPEN_ROAMING)), -50, 6).expect("a beacon");
    assert_eq!(ap.rcoi(), &OPEN_ROAMING);

    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = ap.as_msg().encode_into(&mut buf).expect("fits");
    assert_eq!(SightingMsg::decode(&buf[..len]).expect("valid").ext, &OPEN_ROAMING);
}

#[test]
fn beacon_parser_drops_roaming_consortium_when_element_exceeds_wire_capacity() {
    // Truncating would invent an identifier nobody beaconed.
    let ap = parse_mgmt(&beacon(&ie(111, &[0u8; 18])), -50, 6).expect("a beacon");
    assert!(ap.rcoi().is_empty());
}

#[test]
fn beacon_parser_preserves_first_roaming_consortium_when_duplicate_elements_occur() {
    // The same rule the channel elements follow: a stray trailing element must
    // not rewrite what the access point actually said.
    let first = [0x01, 0x03, 0x5A, 0x03, 0xBA];
    let second = [0x01, 0x03, 0xBA, 0xA2, 0xD0];
    let mut ies = ie(111, &first);
    ies.extend_from_slice(&ie(111, &second));
    let ap = parse_mgmt(&beacon(&ies), -50, 6).expect("a beacon");
    assert_eq!(ap.rcoi(), &first);
}

#[test]
fn rcoi_text_formats_roaming_consortium_ids_when_rendering_wigle_strings() {
    // Three-byte identifiers get six hex digits and five-byte ones ten, which
    // are the two forms Hotspot 2.0 actually uses; the third identifier is
    // whatever of the body is left.
    assert_eq!(rcoi_text(&OPEN_ROAMING).to_string(), "5A03BA0000 BAA2D00000 BAA2D02000");

    // Lengths nibble 0x03: a three-byte first identifier, no second, and the
    // rest the third.
    let body = [0x01, 0x03, 0x5A, 0x03, 0xBA, 0x11, 0x22, 0x33];
    assert_eq!(rcoi_text(&body).to_string(), "5A03BA 112233");

    // A body whose claimed lengths do not fit is nobody's, and renders as
    // nothing rather than in part.
    let claims_more_than_it_carries = [0x01, 0x55, 0x5A, 0x03];
    assert_eq!(rcoi_text(&claims_more_than_it_carries).to_string(), "");
    assert_eq!(rcoi_text(&[]).to_string(), "");
}

#[test]
fn sighting_msg_survives_wire_round_trip_when_parsed_from_beacon() {
    // The point of the whole module: what the radio heard has to survive being
    // broadcast and read back by the host. The SSID here has a comma in it,
    // which the format this replaced could not carry — the sender rewrote it
    // as an underscore and the real name was gone.
    let mut ies = ie(0, b"My,Net");
    ies.extend_from_slice(&ie(3, &[11]));
    ies.extend_from_slice(&ie(48, &rsn(&[2])));
    let ap = parse_mgmt(&beacon(&ies), -42, 11).expect("a beacon");

    let mut buf = [0u8; SIGHTING_MSG_MAX];
    let len = ap.as_msg().encode_into(&mut buf).expect("the frame fits");

    let back = SightingMsg::decode(&buf[..len]).expect("valid frame");
    assert_eq!(back.bssid, ap.bssid);
    assert_eq!(back.ssid, b"My,Net");
    assert_eq!(back.security, ap.security);
    assert_eq!(back.security.token(), Some("[WPA2_PSK]"));
    assert_eq!(back.channel, ap.channel);
    assert_eq!(back.rssi, ap.rssi);
}

/// Real frames, appended by `tools/beacons/extract.py`.
const CAPTURED: &str = include_str!("beacon_vectors.txt");

#[test]
fn beacon_parser_yields_usable_observation_when_processing_captured_beacons() {
    // The question the assembled frames above cannot answer: whether the ladder
    // agrees with the air, rather than with a reading of the firmware. These
    // came off a real street through `tools/beacons/extract.py`, which scrubs
    // the addresses and names on the way past — a capture of the air around you
    // is a geolocation fingerprint, and none of that is what is under test. The
    // elements the parser reads are untouched.
    let mut checked = 0;
    let mut tokens = std::collections::BTreeSet::new();
    let mut hidden = 0;
    for line in CAPTURED.lines().filter(|l| !l.trim().is_empty() && !l.starts_with('#')) {
        let mut parts = line.split_whitespace();
        let name = parts.next().expect("name");
        let len: usize = parts.next().expect("length").parse().expect("numeric length");
        let hex = parts.next().expect("hex");
        let frame: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex byte"))
            .collect();
        assert_eq!(frame.len(), len, "{name}: declared length disagrees with payload");

        let ap = parse_mgmt(&frame, -50, 6).unwrap_or_else(|| panic!("{name}: not a beacon"));
        assert_ne!(ap.bssid, [0; 6], "{name}: an access point with no BSSID");
        assert!(ap.channel > 0, "{name}: no channel, and no fallback either");
        assert!(ap.ssid().len() <= SSID_MAX, "{name}: SSID longer than the standard allows");

        // The payload has to survive the wire, which is the only reason any of
        // this is parsed at all.
        let mut buf = [0u8; SIGHTING_MSG_MAX];
        let written = ap
            .as_msg()
            .encode_into(&mut buf)
            .unwrap_or_else(|| panic!("{name}: frame did not fit SIGHTING_MSG_MAX"));
        let back = SightingMsg::decode(&buf[..written]).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(back.ssid, ap.ssid(), "{name}: SSID did not survive the wire");

        tokens.insert(ap.security.to_string());
        hidden += usize::from(ap.ssid().is_empty());
        checked += 1;
    }

    // Floors rather than exact figures, so a re-capture somewhere else still
    // passes. What they catch is a fixture that has quietly stopped being worth
    // having: emptied by a bad merge, or replaced by ninety copies of one
    // access point on one vendor's firmware.
    assert!(checked >= 20, "only {checked} captured beacons; the fixture is too thin to mean much");
    assert!(tokens.len() >= 3, "one street's worth of security modes should not be {tokens:?}");
    assert!(hidden > 0, "no hidden networks: the empty-SSID path is going untested");
}
