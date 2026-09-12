//! Reading access points out of 802.11 management frames.
//!
//! The frames here are assembled rather than captured, because the thing under
//! test is a classification ladder — the interesting cases are the ones a bench
//! capture is least likely to contain. Real frames go in `beacon_vectors.txt`
//! and are exercised by `every_captured_beacon_yields_a_usable_observation`.

use wartui_proto::air::{SIGHTING_MSG_MAX, SSID_MAX, Security, SightingMsg};
use wartui_proto::beacon::parse_mgmt;

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
fn an_open_network_has_neither_privacy_nor_cipher_suites() {
    let frame = mgmt(8, 0x0001, &ie(0, b"cafe"));
    let ap = parse_mgmt(&frame, -50, 6).expect("a beacon");
    assert_eq!(ap.security, Security::Open);
    assert_eq!(ap.ssid(), b"cafe");
    assert_eq!(ap.bssid, BSSID);
    assert_eq!(ap.rssi, -50);
}

#[test]
fn privacy_with_no_cipher_suites_is_the_wep_heuristic() {
    // Nothing else can explain an encrypted network that names no suite.
    assert_eq!(security_of(&ie(0, b"old")), Security::Wep);
}

#[test]
fn each_akm_combination_maps_to_the_token_the_exporter_expects() {
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
fn a_transitional_network_advertising_both_elements_is_wpa_wpa2() {
    let mut ies = ie(0, b"mixed");
    ies.extend_from_slice(&ie(48, &rsn(&[2])));
    ies.extend_from_slice(&ie(221, &wpa(&[2])));
    assert_eq!(security_of(&ies), Security::WpaWpa2Psk);
}

#[test]
fn wpa_only_enterprise_reports_as_wpa2_because_the_token_is_shared() {
    // `WIFI_AUTH_ENTERPRISE` and `WIFI_AUTH_WPA2_ENTERPRISE` share the `[WPA2]`
    // token, and the token is what has to match.
    assert_eq!(security_of(&ie(221, &wpa(&[1]))), Security::Wpa2Enterprise);
}

#[test]
fn a_legacy_enterprise_element_does_not_override_what_rsn_says() {
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
fn owe_collapses_to_undefined_along_with_everything_else_unmapped() {
    // The firmware's `switch` has no arm for `WIFI_AUTH_OWE`, so it falls to
    // `default`. Reproducing the gap is deliberate: the column is a contract
    // with an exporter, not a description of the network.
    assert_eq!(security_of(&ie(48, &rsn(&[18]))), Security::Undefined);
}

#[test]
fn wapi_outranks_every_other_element() {
    // It is tested first, before RSN is consulted.
    let mut ies = ie(48, &rsn(&[2]));
    ies.extend_from_slice(&ie(68, &[]));
    assert_eq!(security_of(&ies), Security::WapiPsk);
}

#[test]
fn a_hidden_network_still_yields_a_bssid() {
    // Twelve of the access points in the Phase 0 capture had empty SSIDs, and
    // a wardrive that dropped them would be missing the interesting ones.
    let ap = parse_mgmt(&beacon(&ie(0, b"")), -70, 11).expect("a beacon");
    assert!(ap.ssid().is_empty());
    assert_eq!(ap.bssid, BSSID);
}

#[test]
fn an_ssid_of_nothing_but_zero_bytes_is_a_hidden_network() {
    // The other way of cloaking: the element carries the name's real length
    // with every byte zeroed. This is the exact shape that reached a WiGLE
    // export as eight NULs in the SSID column.
    let ap = parse_mgmt(&beacon(&ie(0, &[0u8; 8])), -70, 11).expect("a beacon");
    assert!(ap.ssid().is_empty());
    assert_eq!(ap.bssid, BSSID);
}

#[test]
fn a_zero_padded_ssid_keeps_the_name_and_loses_the_padding() {
    let ap = parse_mgmt(&beacon(&ie(0, b"Home\0\0\0")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"Home");
}

#[test]
fn an_interior_zero_byte_is_not_padding_and_is_kept() {
    // The guard against over-trimming. Nothing here knows what a NUL in the
    // middle of a name was meant to be, so nothing here decides.
    let ap = parse_mgmt(&beacon(&ie(0, b"a\0b")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"a\0b");
    // And a name ending in a byte that is not zero is untouched.
    let ap = parse_mgmt(&beacon(&ie(0, b"plain")), -70, 11).expect("a beacon");
    assert_eq!(ap.ssid(), b"plain");
}

#[test]
fn an_over_long_ssid_of_nothing_but_zeros_is_a_hidden_network() {
    let ap = parse_mgmt(&beacon(&ie(0, &[0u8; 40])), -50, 6).expect("a beacon");
    assert!(ap.ssid().is_empty());
}

#[test]
fn a_name_past_the_legal_length_does_not_rescue_the_padding_in_front_of_it() {
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
fn the_channel_comes_from_the_element_that_carries_it() {
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
fn ds_parameter_set_wins_over_ht_operation() {
    let mut ies = ie(3, &[6]);
    ies.extend_from_slice(&ie(61, &[36, 0, 0, 0, 0]));
    assert_eq!(parse_mgmt(&beacon(&ies), -50, 1).expect("beacon").channel, 6);
}

#[test]
fn probe_responses_count_and_other_frames_do_not() {
    assert!(parse_mgmt(&mgmt(5, 0x0001, &ie(0, b"n")), -50, 6).is_some(), "probe response");
    assert!(parse_mgmt(&mgmt(4, 0x0001, &ie(0, b"n")), -50, 6).is_none(), "probe request");
    assert!(parse_mgmt(&mgmt(13, 0x0001, &[]), -50, 6).is_none(), "action frame");

    // Type 2 is data, which arrives constantly and carries none of this.
    let mut data = mgmt(8, 0x0001, &ie(0, b"n"));
    data[0] = 0b0000_1000;
    assert!(parse_mgmt(&data, -50, 6).is_none(), "data frame");
}

#[test]
fn frames_too_short_for_the_fixed_fields_are_refused() {
    let frame = beacon(&ie(0, b"n"));
    for len in 0..36 {
        assert!(parse_mgmt(&frame[..len], -50, 6).is_none(), "{len} bytes is not a beacon");
    }
    assert!(parse_mgmt(&frame[..36], -50, 6).is_some(), "36 bytes is header plus fixed fields");
}

#[test]
fn a_truncated_element_ends_the_walk_without_losing_the_access_point() {
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
fn a_suite_count_that_overruns_the_element_is_survivable() {
    // Claiming 4000 pairwise suites inside a 20-byte element is either a bug or
    // an attack; either way it must not read past the frame.
    let mut body = vec![0x01, 0x00, 0x00, 0x0F, 0xAC, 4];
    body.extend_from_slice(&4000u16.to_le_bytes());
    let ap = parse_mgmt(&beacon(&ie(48, &body)), -50, 6).expect("still a beacon");
    // No AKM was readable, so only the privacy bit remains to go on.
    assert_eq!(ap.security, Security::Wep);
}

#[test]
fn an_over_long_ssid_is_kept_to_what_the_standard_allows() {
    let long = [b'x'; 60];
    let ap = parse_mgmt(&beacon(&ie(0, &long)), -50, 6).expect("a beacon");
    assert_eq!(ap.ssid().len(), SSID_MAX);
}

#[test]
fn a_sighting_round_trips_through_the_wire_format() {
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
fn every_captured_beacon_yields_a_usable_observation() {
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
